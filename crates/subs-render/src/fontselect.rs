// Copyright (C) 2006 Evgeniy Stepanov <eugeni.stepanov@gmail.com>
// Copyright (C) 2015 Vabishchevich Nikolay <vabnick@gmail.com>
//
// Permission to use, copy, modify, and distribute this software for any
// purpose with or without fee is hereby granted, provided that the above
// copyright notice and this permission notice appear in all copies.
// THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
// WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
// MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
// ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
// WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
// ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
// OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
//
// Selection scores ported from libass 0.17.5, ass_fontselect.c (4a05d81).
// Platform discovery is clean-room, using Android fonts.xml and sfnt names.

//! Runtime fonts only. Embedded fonts precede directory/platform fonts.
//! A fixed directory disables platform discovery, including fallback.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use crate::sfnt::{self, FaceMeta, STYLE_BOLD, STYLE_ITALIC};

const MAX_FILES: usize = 4096;
const MAX_MEMORY: usize = 64 << 20;

#[derive(Clone, Debug, Default)]
pub struct FontOptions {
    /// When set, use only these directories and embedded fonts. No OS fallback.
    pub directories: Option<Vec<PathBuf>>,
    pub default_family: Option<String>,
}

#[derive(Clone, Debug)]
enum Source {
    File(PathBuf),
    Memory(Arc<[u8]>),
}

#[derive(Debug)]
struct FontInfo {
    meta: FaceMeta,
    source: Source,
    offset: u32,
    aliases: Vec<String>,
    variations: Vec<ttf_parser::Variation>,
    coverage: OnceLock<Option<(Vec<u8>, Option<Vec<u8>>)>>,
}

impl FontInfo {
    fn supports(&self, code: u32) -> bool {
        if code == 0 { return true; }
        let coverage = self.coverage.get_or_init(|| {
            fn tables(src: &mut impl sfnt::Source, offset: u32) -> Option<(Vec<u8>, Option<Vec<u8>>)> {
                let dir = sfnt::table_directory(src, offset)?;
                Some((sfnt::read_table(src, &dir, b"cmap")?, sfnt::read_table(src, &dir, b"OS/2")))
            }
            match &self.source {
                Source::File(path) => tables(&mut File::open(path).ok()?, self.offset),
                Source::Memory(data) => tables(&mut data.as_ref(), self.offset),
            }
        });
        coverage.as_ref().is_some_and(|(cmap, os2)| sfnt::cmap_glyph(cmap, os2.as_deref(), code) != 0)
    }

    fn load(&self) -> Option<Arc<[u8]>> {
        let bytes = match &self.source {
            Source::File(path) => sfnt::load_face(&mut File::open(path).ok()?, self.offset)?,
            Source::Memory(data) => sfnt::load_face(&mut data.as_ref(), self.offset)?,
        };
        ttf_parser::Face::parse(&bytes, 0).ok()?;
        Some(bytes.into())
    }

    fn named(&self, family: &str) -> bool {
        self.meta.families.iter().chain(&self.aliases).any(|s| s.eq_ignore_ascii_case(family))
    }

    fn score(&self, family: &str, weight: i32, italic: bool) -> Option<u32> {
        if self.named(family) {
            let has_italic = self.meta.style_flags & STYLE_ITALIC != 0;
            let slant = match (italic, has_italic) { (true, false) => 1, (false, true) => 4, _ => 0 };
            let mut actual = self.meta.weight;
            if weight > actual + 150 && self.meta.style_flags & STYLE_BOLD == 0 { actual += 120; }
            Some(slant + (73 * i64::from(actual).abs_diff(i64::from(weight)) / 256) as u32)
        } else if if self.meta.is_postscript {
            self.meta.postscript_name.as_ref().is_some_and(|s| s.eq_ignore_ascii_case(family))
        } else {
            self.meta.fullnames.iter().any(|s| s.eq_ignore_ascii_case(family))
        } { Some(0) } else { None }
    }
}

#[derive(Default, Debug)]
struct Database {
    fonts: Vec<Arc<FontInfo>>,
    aliases: HashMap<String, (String, Option<i32>)>,
    fallback: Vec<String>,
}

impl Database {
    fn file(&mut self, path: &Path, aliases: &[String], weight: Option<i32>, italic: Option<bool>, variations: &[ttf_parser::Variation], index: Option<u32>) {
        if self.fonts.len() >= MAX_FILES { return; }
        let Ok(mut file) = File::open(path) else { return };
        for (i, offset) in sfnt::face_offsets(&mut file).into_iter().enumerate() {
            if index.is_some_and(|wanted| wanted != i as u32) { continue; }
            if self.fonts.len() >= MAX_FILES { break; }
            let Some(mut meta) = sfnt::face_meta(&mut file, offset, i as u32) else { continue };
            if let Some(w) = weight { meta.weight = w; }
            if let Some(i) = italic {
                meta.style_flags = (meta.style_flags & !STYLE_ITALIC) | if i { STYLE_ITALIC } else { 0 };
            }
            self.fonts.push(Arc::new(FontInfo { meta, source: Source::File(path.into()), offset, aliases: aliases.to_vec(), variations: variations.to_vec(), coverage: OnceLock::new() }));
        }
    }

    fn directory(&mut self, path: &Path, depth: u8) {
        if depth > 6 || self.fonts.len() >= MAX_FILES { return; }
        let Ok(entries) = std::fs::read_dir(path) else { return };
        // Stable order makes equal-score faces and fixed-directory fallback deterministic.
        let mut paths: Vec<_> = entries.take(MAX_FILES).filter_map(Result::ok).collect();
        paths.sort_by_key(|e| e.file_name());
        for entry in paths {
            if self.fonts.len() >= MAX_FILES { break; }
            let Ok(kind) = entry.file_type() else { continue };
            let path = entry.path();
            if kind.is_dir() { self.directory(&path, depth + 1); }
            else if kind.is_file() && path.extension().and_then(|s| s.to_str()).is_some_and(|s| ["ttf", "otf", "ttc", "otc"].iter().any(|ext| s.eq_ignore_ascii_case(ext))) {
                self.file(&path, &[], None, None, &[], None);
            }
        }
    }

    fn android_xml(&mut self, xml: &str, root: &Path) {
        let options = roxmltree::ParsingOptions { nodes_limit: 100_000, ..Default::default() };
        let Ok(doc) = roxmltree::Document::parse_with_options(xml, options) else { return };
        for node in doc.descendants().filter(|n| n.has_tag_name("alias")) {
            if let (Some(name), Some(to)) = (node.attribute("name"), node.attribute("to")) {
                self.aliases.insert(name.to_ascii_lowercase(), (to.into(), node.attribute("weight").and_then(|w| w.parse().ok())));
            }
        }
        for (i, family) in doc.descendants().filter(|n| n.has_tag_name("family")).enumerate() {
            let name = family.attribute("name").map(str::to_owned).unwrap_or_else(|| format!("android-fallback-{i}"));
            self.fallback.push(name.clone());
            for font in family.children().filter(|n| n.has_tag_name("font")) {
                let Some(filename) = font.children().find(|n| n.is_text()).and_then(|n| n.text()).map(str::trim) else { continue };
                if filename.is_empty() || Path::new(filename).components().any(|c| !matches!(c, std::path::Component::Normal(_))) { continue; }
                let weight = font.attribute("weight").and_then(|s| s.parse().ok());
                let italic = font.attribute("style").map(|s| s == "italic");
                let mut variations = Vec::new();
                for axis in font.children().filter(|n| n.has_tag_name("axis")) {
                    if let (Some(tag), Some(value)) = (axis.attribute("tag"), axis.attribute("stylevalue").and_then(|s| s.parse::<f32>().ok()).filter(|v| v.is_finite())) {
                        if let Ok(bytes) = <&[u8; 4]>::try_from(tag.as_bytes()) {
                            variations.push(ttf_parser::Variation { axis: ttf_parser::Tag::from_bytes(bytes), value });
                        }
                    }
                }
                self.file(&root.join(filename), std::slice::from_ref(&name), weight, italic, &variations, font.attribute("index").and_then(|s| s.parse().ok()));
            }
        }
    }

    fn platform() -> Self {
        let mut db = Self::default();
        if cfg!(target_os = "android") {
            // Android 12+ retains fonts.xml for clients of the system font API.
            if let Ok(xml) = std::fs::read_to_string("/system/etc/fonts.xml") {
                if xml.len() <= 4 << 20 { db.android_xml(&xml, Path::new("/system/fonts")); }
            }
            if db.fonts.is_empty() { db.directory(Path::new("/system/fonts"), 0); }
        } else if cfg!(any(target_os = "macos", target_os = "ios")) {
            for path in ["/System/Library/Fonts", "/Library/Fonts", "/System/Library/AssetsV2/com_apple_MobileAsset_Font7"] { db.directory(Path::new(path), 0); }
            if cfg!(target_os = "macos") {
                if let Some(home) = std::env::var_os("HOME") { db.directory(&PathBuf::from(home).join("Library/Fonts"), 0); }
            }
            for (generic, family) in [("sans-serif", "Helvetica"), ("serif", "Times"), ("monospace", "Menlo")] {
                db.aliases.insert(generic.into(), (family.into(), None));
            }
            db.fallback = ["Helvetica", "Arial Unicode MS", "Geeza Pro", "Devanagari Sangam MN", "PingFang SC", "Hiragino Sans"].map(str::to_owned).into();
        } else {
            for path in ["/usr/share/fonts", "/usr/local/share/fonts"] { db.directory(Path::new(path), 0); }
            if let Some(home) = std::env::var_os("HOME") {
                let home = PathBuf::from(home);
                db.directory(&home.join(".local/share/fonts"), 0);
                db.directory(&home.join(".fonts"), 0);
            }
            for (generic, family) in [("sans-serif", "DejaVu Sans"), ("serif", "DejaVu Serif"), ("monospace", "DejaVu Sans Mono")] { db.aliases.insert(generic.into(), (family.into(), None)); }
        }
        db
    }
}

/// A loaded, collection-independent face. Cloning shares its font bytes.
#[derive(Clone, Debug)]
pub struct SelectedFace {
    pub id: usize,
    pub data: Arc<[u8]>,
    pub meta: FaceMeta,
    pub variations: Vec<ttf_parser::Variation>,
}

impl SelectedFace {
    pub fn face(&self) -> Option<ttf_parser::Face<'_>> {
        let mut face = ttf_parser::Face::parse(&self.data, 0).ok()?;
        for v in &self.variations { face.set_variation(v.axis, v.value); }
        Some(face)
    }
}

pub struct FontSelector {
    database: OnceLock<Arc<Database>>,
    embedded: Vec<Arc<FontInfo>>,
    embedded_bytes: usize,
    default_family: String,
    loaded: VecDeque<Arc<SelectedFace>>,
    loaded_bytes: usize,
    selections: HashMap<(String, i32, bool, u32), Option<usize>>,
}

impl FontSelector {
    pub fn new(options: &FontOptions) -> Self {
        // Bitmap subtitle streams construct a renderer too. Discover system
        // fonts only when text needs them; explicit font directories stay fixed.
        let database = if let Some(dirs) = &options.directories {
            let mut db = Database::default();
            for dir in dirs { db.directory(dir, 0); }
            OnceLock::from(Arc::new(db))
        } else { OnceLock::new() };
        let default_family = options.default_family.clone().unwrap_or_else(|| {
            if options.directories.is_some() {
                database.get().and_then(|db| db.fonts.first()).and_then(|f| f.meta.families.first()).cloned().unwrap_or_default()
            } else { "sans-serif".into() }
        });
        Self { database, embedded: Vec::new(), embedded_bytes: 0, default_family, loaded: VecDeque::new(), loaded_bytes: 0, selections: HashMap::new() }
    }

    fn database(&self) -> &Database {
        static SYSTEM: OnceLock<Arc<Database>> = OnceLock::new();
        self.database.get_or_init(|| SYSTEM.get_or_init(|| Arc::new(Database::platform())).clone())
    }

    pub fn is_empty(&self) -> bool { self.embedded.is_empty() && self.database().fonts.is_empty() }

    /// Add a script font or Matroska attachment. No filename is trusted or written.
    pub fn add_font(&mut self, bytes: Arc<[u8]>) -> bool {
        if bytes.len() > MAX_MEMORY.saturating_sub(self.embedded_bytes) || self.embedded.len() >= MAX_FILES { return false; }
        let mut fonts = Vec::new();
        for (index, offset) in sfnt::face_offsets(&mut bytes.as_ref()).into_iter().enumerate() {
            if let Some(meta) = sfnt::face_meta(&mut bytes.as_ref(), offset, index as u32) {
                fonts.push(Arc::new(FontInfo { meta, source: Source::Memory(bytes.clone()), offset, aliases: Vec::new(), variations: Vec::new(), coverage: OnceLock::new() }));
            }
        }
        if fonts.is_empty() { return false; }
        self.embedded_bytes += bytes.len();
        self.embedded.extend(fonts);
        self.selections.clear();
        self.loaded.clear();
        self.loaded_bytes = 0;
        true
    }

    fn fonts(&self) -> impl Iterator<Item = &Arc<FontInfo>> { self.embedded.iter().chain(&self.database().fonts) }

    fn find(&self, family: &str, weight: i32, italic: bool, code: u32) -> Option<usize> {
        let (mut name, mut w) = (family, weight);
        for _ in 0..16 {
            let Some((to, override_weight)) = self.database().aliases.get(&name.to_ascii_lowercase()) else { break };
            name = to;
            w = override_weight.unwrap_or(w);
        }
        self.fonts().enumerate().filter_map(|(id, font)| font.score(name, w, italic).map(|score| (score, id, font)))
            .filter(|(_, _, font)| font.supports(code)).min_by_key(|(score, id, _)| (*score, *id)).map(|(_, id, _)| id)
    }

    pub fn select(&mut self, family: &str, weight: i32, italic: bool, code: u32) -> Option<Arc<SelectedFace>> {
        let key = (family.trim_start_matches('@').to_owned(), weight.clamp(100, 1000), italic, code);
        let id = if let Some(cached) = self.selections.get(&key) { (*cached)? } else {
            let id = self.find(&key.0, key.1, italic, code)
                .or_else(|| self.find(&self.default_family, key.1, italic, code))
                .or_else(|| self.database().fallback.iter().find_map(|f| self.find(f, key.1, italic, code)))
                .or_else(|| self.fonts().position(|font| font.supports(code)));
            if self.selections.len() >= 8192 { self.selections.clear(); }
            self.selections.insert(key, id);
            id?
        };
        if let Some(pos) = self.loaded.iter().position(|face| face.id == id) {
            let face = self.loaded.remove(pos)?;
            self.loaded.push_back(face.clone());
            return Some(face);
        }
        let info = self.fonts().nth(id)?;
        let face = Arc::new(SelectedFace { id, data: info.load()?, meta: info.meta.clone(), variations: info.variations.clone() });
        while self.loaded_bytes + face.data.len() > MAX_MEMORY {
            let old = self.loaded.pop_front()?;
            self.loaded_bytes -= old.data.len();
        }
        self.loaded_bytes += face.data.len();
        self.loaded.push_back(face.clone());
        Some(face)
    }
}
