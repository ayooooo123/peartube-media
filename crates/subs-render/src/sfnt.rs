//! Font file metadata without loading whole files: the table directory of
//! an OpenType/TrueType font or collection (OpenType 1.9, "Organization of
//! an OpenType Font" and "Font Collections"), and the `name`, `OS/2`,
//! `head`, `hhea` and `cmap` fields font selection needs. Clean-room from
//! the OpenType specification; tables are parsed by ttf-parser.

use std::io::{Read, Seek, SeekFrom};

/// Faces read from one collection at most.
const MAX_FACES: u32 = 64;
/// Tables a face lists at most.
const MAX_TABLES: u16 = 256;
/// Largest table read for metadata (`name` tables of CJK fonts run to
/// tens of KiB; a `cmap` to a few MiB).
const MAX_TABLE: u32 = 8 << 20;

fn be16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*b.get(at)?, *b.get(at + 1)?]))
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes([*b.get(at)?, *b.get(at + 1)?, *b.get(at + 2)?, *b.get(at + 3)?]))
}

/// A face's table records: tag, offset from the file start, length.
#[derive(Clone, Debug, Default)]
pub struct TableDirectory {
    pub tables: Vec<([u8; 4], u32, u32)>,
}

impl TableDirectory {
    pub fn find(&self, tag: &[u8; 4]) -> Option<(u32, u32)> {
        self.tables.iter().find(|(t, _, _)| t == tag).map(|&(_, o, l)| (o, l))
    }
}

/// Something font bytes can be read from at offsets.
pub trait Source {
    fn read_at(&mut self, offset: u64, len: usize) -> Option<Vec<u8>>;
}

impl Source for &[u8] {
    fn read_at(&mut self, offset: u64, len: usize) -> Option<Vec<u8>> {
        let start = usize::try_from(offset).ok()?;
        self.get(start..start.checked_add(len)?).map(<[u8]>::to_vec)
    }
}

impl Source for std::fs::File {
    fn read_at(&mut self, offset: u64, len: usize) -> Option<Vec<u8>> {
        self.seek(SeekFrom::Start(offset)).ok()?;
        let mut buf = vec![0; len];
        self.read_exact(&mut buf).ok()?;
        Some(buf)
    }
}

/// The offsets of the faces in a font file: one for a single font, each
/// member's for a collection (`ttcf`).
pub fn face_offsets(src: &mut impl Source) -> Vec<u32> {
    let Some(head) = src.read_at(0, 12) else { return Vec::new() };
    if &head[..4] == b"ttcf" {
        let n = be32(&head, 8).unwrap_or(0).min(MAX_FACES);
        let Some(list) = src.read_at(12, n as usize * 4) else { return Vec::new() };
        return (0..n as usize).filter_map(|i| be32(&list, i * 4)).collect();
    }
    match be32(&head, 0) {
        Some(0x0001_0000 | 0x4F54_544F | 0x7472_7565) => vec![0],
        _ => Vec::new(),
    }
}

pub fn table_directory(src: &mut impl Source, offset: u32) -> Option<TableDirectory> {
    let head = src.read_at(u64::from(offset), 12)?;
    let n = be16(&head, 4)?.min(MAX_TABLES);
    let records = src.read_at(u64::from(offset) + 12, usize::from(n) * 16)?;
    let tables = (0..usize::from(n))
        .filter_map(|i| {
            let r = &records[i * 16..i * 16 + 16];
            Some(([r[0], r[1], r[2], r[3]], be32(r, 8)?, be32(r, 12)?))
        })
        .collect();
    Some(TableDirectory { tables })
}

pub fn read_table(src: &mut impl Source, dir: &TableDirectory, tag: &[u8; 4]) -> Option<Vec<u8>> {
    let (offset, len) = dir.find(tag)?;
    if len > MAX_TABLE {
        return None;
    }
    src.read_at(u64::from(offset), len as usize)
}

/// Load just one face, including the tables shared by a collection. This
/// avoids keeping a whole CJK collection in memory for a single face.
/// The returned sfnt always has face index zero.
pub fn load_face(src: &mut impl Source, offset: u32) -> Option<Vec<u8>> {
    const MAX_FONT: usize = 32 << 20;
    let dir = table_directory(src, offset)?;
    let n = dir.tables.len();
    let mut size = 12 + n * 16;
    for (_, _, len) in &dir.tables {
        size = size.checked_add((*len as usize).checked_add(3)? & !3)?;
        if size > MAX_FONT { return None; }
    }
    let mut out = vec![0; size];
    let signature = src.read_at(u64::from(offset), 4)?;
    out[..4].copy_from_slice(&signature);
    out[4..6].copy_from_slice(&(n as u16).to_be_bytes());
    let mut at = 12 + n * 16;
    for (i, (tag, start, len)) in dir.tables.iter().enumerate() {
        let bytes = src.read_at(u64::from(*start), *len as usize)?;
        let record = 12 + i * 16;
        out[record..record + 4].copy_from_slice(tag);
        out[record + 8..record + 12].copy_from_slice(&(at as u32).to_be_bytes());
        out[record + 12..record + 16].copy_from_slice(&len.to_be_bytes());
        out[at..at + bytes.len()].copy_from_slice(&bytes);
        at += (bytes.len() + 3) & !3;
    }
    Some(out)
}

/// What font selection knows of a face.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FaceMeta {
    /// Microsoft-platform family names (name ID 1), every language.
    pub families: Vec<String>,
    /// Microsoft-platform full names (name ID 4).
    pub fullnames: Vec<String>,
    pub postscript_name: Option<String>,
    /// `FT_STYLE_FLAG_ITALIC` (1) and `FT_STYLE_FLAG_BOLD` (2) as GDI sees
    /// them: from OS/2 `fsSelection` when there is one.
    pub style_flags: u32,
    /// `ass_face_get_weight`.
    pub weight: i32,
    /// PostScript (CFF) outlines.
    pub is_postscript: bool,
    pub index: u32,
}

pub const STYLE_ITALIC: u32 = 1;
pub const STYLE_BOLD: u32 = 2;

/// UTF-16BE to a string (`ass_utf16be_to_utf8`, unpaired surrogates as
/// U+FFFD).
pub fn utf16be(bytes: &[u8]) -> String {
    let units = bytes.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]]));
    char::decode_utf16(units).map(|r| r.unwrap_or('\u{fffd}')).collect()
}

/// The `name` table's names with `name_id`: the Microsoft platform's (in
/// table order), else the first Unicode-decodable one of any platform.
fn names(table: &[u8], name_id: u16, microsoft_only: bool) -> Vec<String> {
    let Some(t) = ttf_parser::name::Table::parse(table) else { return Vec::new() };
    let mut out = Vec::new();
    for n in t.names {
        if n.name_id == name_id && n.platform_id == ttf_parser::PlatformId::Windows {
            let s = utf16be(n.name);
            if !s.is_empty() && out.len() < 100 {
                out.push(s);
            }
        }
    }
    if out.is_empty() && !microsoft_only {
        if let Some(s) = t.names.into_iter().filter(|n| n.name_id == name_id).find_map(|n| n.to_string()) {
            out.push(s);
        }
    }
    out
}

/// `ass_face_get_weight` from OS/2's `usWeightClass`.
pub fn weight_of(os2: Option<&[u8]>, mac_bold: bool) -> i32 {
    let class = os2.and_then(|t| be16(t, 4)).unwrap_or(0);
    match class {
        0 => 300 * i32::from(mac_bold) + 400,
        1 => 100,
        2 => 200,
        3 => 300,
        4 => 350,
        5 => 400,
        6 => 600,
        7 => 700,
        8 => 800,
        9 => 900,
        w => i32::from(w),
    }
}

/// `ass_face_get_style_flags`: from OS/2 `fsSelection` (bit 0 italic, bit
/// 5 bold), else `head`'s `macStyle` (bit 1 italic, bit 0 bold).
pub fn style_flags_of(os2: Option<&[u8]>, head: Option<&[u8]>) -> u32 {
    if let Some(sel) = os2.and_then(|t| be16(t, 62)) {
        return (if sel & 1 != 0 { STYLE_ITALIC } else { 0 }) | (if sel & (1 << 5) != 0 { STYLE_BOLD } else { 0 });
    }
    let mac = head.and_then(|t| be16(t, 44)).unwrap_or(0);
    (if mac & 2 != 0 { STYLE_ITALIC } else { 0 }) | (if mac & 1 != 0 { STYLE_BOLD } else { 0 })
}

/// A face's selection metadata (`get_font_info`); `None` for faces without
/// outlines or any family name.
pub fn face_meta(src: &mut impl Source, offset: u32, index: u32) -> Option<FaceMeta> {
    let dir = table_directory(src, offset)?;
    // Only outlines (TrueType or CFF) render.
    let is_postscript = dir.find(b"CFF ").is_some() || dir.find(b"CFF2").is_some();
    if dir.find(b"glyf").is_none() && !is_postscript {
        return None;
    }
    let name = read_table(src, &dir, b"name")?;
    let os2 = read_table(src, &dir, b"OS/2");
    let head = read_table(src, &dir, b"head");
    let mut families = names(&name, 1, true);
    if families.is_empty() {
        families = names(&name, 16, false);
        if families.is_empty() {
            families = names(&name, 1, false);
        }
    }
    if families.is_empty() {
        return None;
    }
    let fullnames = names(&name, 4, true);
    let postscript_name = names(&name, 6, false).into_iter().next();
    let style_flags = style_flags_of(os2.as_deref(), head.as_deref());
    let mac_bold = head.as_deref().and_then(|t| be16(t, 44)).is_some_and(|m| m & 1 != 0);
    Some(FaceMeta { families, fullnames, postscript_name, style_flags, weight: weight_of(os2.as_deref(), mac_bold), is_postscript, index })
}


/// The `cmap` subtable libass's `ass_charmap_magic` picks: Microsoft UCS-4,
/// else Microsoft BMP, else the first Microsoft one, else the first. With
/// it, whether that charmap is Microsoft Symbol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmapKind {
    Unicode,
    MsSymbol,
    /// A legacy multi-byte Microsoft encoding this renderer cannot map.
    MsOther,
    Other,
}

/// The glyph of `code` under the charmap `ass_charmap_magic` selects,
/// mapped as `ass_font_index_magic` maps it; 0 when there is none.
pub fn cmap_glyph(cmap: &[u8], os2: Option<&[u8]>, code: u32) -> u16 {
    let Some(table) = ttf_parser::cmap::Table::parse(cmap) else { return 0 };
    let ms = |enc: u16| table.subtables.into_iter().find(|s| s.platform_id == ttf_parser::PlatformId::Windows && s.encoding_id == enc);
    let Some(sub) = ms(10)
        .or_else(|| ms(1))
        .or_else(|| table.subtables.into_iter().find(|s| s.platform_id == ttf_parser::PlatformId::Windows))
        .or_else(|| table.subtables.into_iter().next()) else { return 0 };
    let kind = if sub.platform_id == ttf_parser::PlatformId::Windows {
        match sub.encoding_id {
            0 => CmapKind::MsSymbol,
            1 | 10 => CmapKind::Unicode,
            _ => CmapKind::MsOther,
        }
    } else {
        CmapKind::Other
    };
    let code = match kind {
        CmapKind::MsSymbol => {
            let charset = os2.and_then(|t| be16(t, 62)).map_or(0, |sel| (sel >> 8) as u8);
            match charset {
                178 => crate::arabic_charmap::simplified(code),
                179 => crate::arabic_charmap::traditional(code),
                _ => 0xF000 | code,
            }
        }
        // Legacy CJK code pages need a converter libass gets from iconv.
        CmapKind::MsOther => return 0,
        _ => code,
    };
    if code == 0 {
        return 0;
    }
    sub.glyph_index(code).map_or(0, |g| g.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_names_decode() {
        assert_eq!(utf16be(&[0, b'A', 0xd8, 0x3d, 0xde, 0x00]), "A\u{1F600}");
        assert_eq!(utf16be(&[0xdc, 0x00]), "\u{fffd}");
    }

    #[test]
    fn weights_follow_gdi() {
        let mut os2 = vec![0u8; 78];
        os2[4..6].copy_from_slice(&7u16.to_be_bytes());
        assert_eq!(weight_of(Some(&os2), false), 700);
        os2[4..6].copy_from_slice(&550u16.to_be_bytes());
        assert_eq!(weight_of(Some(&os2), false), 550);
        assert_eq!(weight_of(None, true), 700);
        os2[62..64].copy_from_slice(&0x21u16.to_be_bytes());
        assert_eq!(style_flags_of(Some(&os2), None), STYLE_ITALIC | STYLE_BOLD);
    }
}
