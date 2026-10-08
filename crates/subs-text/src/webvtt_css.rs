//! The CSS of WebVTT `STYLE` blocks that browsers apply to cue text:
//! `::cue` and `::cue(selector)` rules (W3C WebVTT §8.2.1) with the
//! properties §8.2.1 lets them set: `color`, `background-color` (also the
//! colour of a `background` shorthand), `font-weight`, `font-style`,
//! `text-decoration`, `text-shadow`, `opacity`, `font-family` and
//! `font-size` (relative to the default cue font, the one size this
//! renderer has).
//!
//! Selectors: type (`c i b u ruby rt v lang`), class (`.loud`), the cue
//! identifier (`#name`), attributes (`v[voice="Roger"]`, `[voice]`,
//! `lang[lang="en"]`), the universal selector, compounds of these and the
//! descendant combinator; specificity and source order decide, as the CSS
//! cascade does. Clean-room implementation from W3C WebVTT §8 and CSS
//! Cascading/Selectors Level 3.
//!
//! The defaults are §7.4's: white text on a `rgba(0,0,0,0.8)` cue
//! background box.

use crate::webvtt_cue::{Kind, Node};

/// A straight-alpha RGBA colour.
pub type Rgba = [u8; 4];

/// The cue background box's colour when no rule sets one (§7.4).
pub const DEFAULT_BACKGROUND: Rgba = [0, 0, 0, 204];

/// Most rules a track keeps; later ones are ignored.
const MAX_RULES: usize = 512;
/// Most bytes of CSS read from a track's header.
const MAX_CSS_BYTES: usize = 64 * 1024;

/// A text shadow: offset in pixels of the default font, and its colour
/// (`None`: the text's colour).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shadow {
    pub dx: f32,
    pub dy: f32,
    pub color: Option<Rgba>,
}

/// What the cascade decides for a node, inherited properties included.
#[derive(Clone, Debug, PartialEq)]
pub struct Style {
    pub color: Rgba,
    /// The node's own background (not inherited); for the root, the cue
    /// background box.
    pub background: Option<Rgba>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub line_through: bool,
    pub shadow: Option<Shadow>,
    /// Multiplies the alpha of everything the node draws, its subtree's
    /// included.
    pub opacity: f32,
    /// The font size, relative to the default cue font.
    pub size: f32,
    pub family: Option<String>,
}

impl Style {
    /// The root's style before any rule: §7.4's defaults.
    pub fn root() -> Style {
        Style {
            color: [255, 255, 255, 255],
            background: Some(DEFAULT_BACKGROUND),
            bold: false,
            italic: false,
            underline: false,
            line_through: false,
            shadow: None,
            opacity: 1.0,
            size: 1.0,
            family: None,
        }
    }

    /// A child's style before its rules: inherited properties kept,
    /// background and opacity reset (opacity multiplies down instead).
    fn child_of(&self) -> Style {
        Style { background: None, opacity: 1.0, ..self.clone() }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Attribute {
    /// `[name]`
    Present(String),
    /// `[name="value"]`
    Equals(String, String),
}

/// One compound selector: all its parts must match one node.
#[derive(Clone, Debug, Default, PartialEq)]
struct Compound {
    /// `None`: any type.
    kind: Option<String>,
    classes: Vec<String>,
    id: Option<String>,
    attributes: Vec<Attribute>,
}

#[derive(Clone, Debug, PartialEq)]
struct Selector {
    /// Descendant chain, outermost first; empty for bare `::cue`.
    chain: Vec<Compound>,
    specificity: (u32, u32, u32),
}

#[derive(Clone, Debug, PartialEq)]
enum Declaration {
    Color(Rgba),
    Background(Rgba),
    Bold(bool),
    Italic(bool),
    Decoration { underline: bool, line_through: bool },
    Shadow(Option<Shadow>),
    Opacity(f32),
    /// A factor of the parent's size (`em`, `%`, `smaller`) or of the
    /// default font (`px`, `vh`, keywords).
    Size { factor: f32, of_parent: bool },
    Family(String),
}

#[derive(Clone, Debug, PartialEq)]
struct Rule {
    selector: Selector,
    order: usize,
    declarations: Vec<Declaration>,
}

/// The `::cue` rules of a track.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StyleSheet {
    rules: Vec<Rule>,
}

/// The node a selector is matched against.
#[derive(Clone, Copy)]
pub enum Target<'a> {
    /// The cue itself (`::cue`), with its identifier.
    Root { id: &'a str },
    Element { kind: Kind, classes: &'a [String], annotation: &'a str },
}

impl StyleSheet {
    /// The `::cue` rules of a WebVTT header's `STYLE` blocks (each runs to
    /// the next blank line). Other rules and unknown properties are
    /// skipped, as CSS skips what it does not understand.
    pub fn from_header(header: &[u8]) -> StyleSheet {
        let text = String::from_utf8_lossy(header);
        let mut css = String::new();
        let mut lines = text.lines().map(|l| l.trim_end_matches('\r')).peekable();
        while let Some(line) = lines.next() {
            if line.strip_prefix("STYLE").is_some_and(|rest| rest.trim().is_empty()) {
                while let Some(line) = lines.next_if(|l| !l.trim().is_empty()) {
                    css.push_str(line);
                    css.push('\n');
                }
            }
            if css.len() > MAX_CSS_BYTES {
                break;
            }
        }
        StyleSheet::parse(&css[..floor_char_boundary(&css, MAX_CSS_BYTES)])
    }

    /// The `::cue` rules of a style sheet.
    pub fn parse(css: &str) -> StyleSheet {
        let css = strip_comments(css);
        let mut rules = Vec::new();
        let mut rest = css.as_str();
        while let Some(open) = rest.find('{') {
            let prelude = &rest[..open];
            let body_end = rest[open..].find('}').map_or(rest.len(), |i| open + i);
            let body = &rest[open + 1..body_end];
            rest = rest.get(body_end + 1..).unwrap_or("");
            let declarations = parse_declarations(body);
            if declarations.is_empty() {
                continue;
            }
            for selector in split_top_level(prelude, ',').into_iter().flat_map(|s| parse_selectors(s.trim())) {
                if rules.len() >= MAX_RULES {
                    break;
                }
                rules.push(Rule { selector, order: rules.len(), declarations: declarations.clone() });
            }
        }
        StyleSheet { rules }
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// The style of `target`, a child of a node styled `parent` (`None`:
    /// the root), whose ancestors (outermost first) are `ancestors`.
    pub fn style(&self, target: Target, ancestors: &[Target], parent: Option<&Style>) -> Style {
        let mut style = parent.map_or_else(Style::root, Style::child_of);
        let parent_size = parent.map_or(1.0, |p| p.size);
        let mut matching: Vec<&Rule> = self.rules.iter().filter(|r| matches(&r.selector, target, ancestors)).collect();
        matching.sort_by_key(|r| (r.selector.specificity, r.order));
        for rule in matching {
            for declaration in &rule.declarations {
                match declaration {
                    Declaration::Color(c) => style.color = *c,
                    Declaration::Background(c) => style.background = Some(*c),
                    Declaration::Bold(b) => style.bold = *b,
                    Declaration::Italic(i) => style.italic = *i,
                    Declaration::Decoration { underline, line_through } => {
                        // Decorations propagate to descendants; `none` on a
                        // child cannot take its parent's away.
                        let inherited = parent.map_or((false, false), |p| (p.underline, p.line_through));
                        style.underline = inherited.0 || *underline;
                        style.line_through = inherited.1 || *line_through;
                    }
                    Declaration::Shadow(s) => style.shadow = *s,
                    Declaration::Opacity(o) => style.opacity = *o,
                    Declaration::Size { factor, of_parent } => {
                        style.size = (if *of_parent { parent_size * factor } else { *factor }).clamp(0.25, 8.0);
                    }
                    Declaration::Family(f) => style.family = Some(f.clone()),
                }
            }
        }
        style
    }
}

fn floor_char_boundary(s: &str, max: usize) -> usize {
    let mut i = max.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn strip_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        rest = rest[start + 2..].find("*/").map_or("", |end| &rest[start + 2 + end + 2..]);
    }
    out.push_str(rest);
    out
}

fn matches(selector: &Selector, target: Target, ancestors: &[Target]) -> bool {
    let Some((last, outer)) = selector.chain.split_last() else {
        // Bare `::cue`: the cue's text as a whole.
        return matches!(target, Target::Root { .. });
    };
    if !compound_matches(last, target) {
        return false;
    }
    // Each outer compound matches some ancestor, in order.
    let mut remaining = ancestors;
    for compound in outer.iter().rev() {
        match remaining.iter().rposition(|a| compound_matches(compound, *a)) {
            Some(i) => remaining = &remaining[..i],
            None => return false,
        }
    }
    true
}

fn compound_matches(c: &Compound, target: Target) -> bool {
    match target {
        Target::Root { id } => {
            // `::cue(#id)`, `::cue(*)`: the cue itself.
            c.kind.is_none() && c.classes.is_empty() && c.attributes.is_empty() && c.id.as_deref().is_none_or(|i| i == id) && c.id.is_some()
        }
        Target::Element { kind, classes, annotation } => {
            c.id.is_none()
                && c.kind.as_deref().is_none_or(|k| k == kind.tag())
                && c.classes.iter().all(|class| classes.contains(class))
                && c.attributes.iter().all(|a| match (a, kind) {
                    (Attribute::Present(n), Kind::Voice) => n == "voice",
                    (Attribute::Present(n), Kind::Language) => n == "lang",
                    (Attribute::Equals(n, v), Kind::Voice) => n == "voice" && v == annotation,
                    (Attribute::Equals(n, v), Kind::Language) => n == "lang" && v == annotation,
                    _ => false,
                })
        }
    }
}

/// `::cue` or `::cue(selector, …)`; nothing for any other selector.
fn parse_selectors(s: &str) -> Vec<Selector> {
    let Some(rest) = s.strip_prefix("::cue") else { return Vec::new() };
    if rest.trim().is_empty() {
        return vec![Selector { chain: Vec::new(), specificity: (0, 0, 1) }];
    }
    let Some(inner) = rest.trim().strip_prefix('(').and_then(|r| r.strip_suffix(')')) else { return Vec::new() };
    split_top_level(inner, ',').into_iter().filter_map(|one| parse_chain(one.trim())).collect()
}

/// One selector: compounds joined by descendant combinators.
fn parse_chain(s: &str) -> Option<Selector> {
    let mut chain = Vec::new();
    let mut specificity = (0, 0, 1);
    for part in split_outside_brackets(s) {
        let compound = parse_compound(part)?;
        specificity.0 += u32::from(compound.id.is_some());
        specificity.1 += (compound.classes.len() + compound.attributes.len()) as u32;
        specificity.2 += u32::from(compound.kind.is_some());
        chain.push(compound);
    }
    (!chain.is_empty()).then_some(Selector { chain, specificity })
}

/// `s` split at spaces outside `[…]` and quotes.
fn split_outside_brackets(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut quote, mut start) = (0, None, None);
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '[') => depth += 1,
            (None, ']') => depth -= 1,
            (None, c) if c.is_whitespace() && depth == 0 => {
                if let Some(from) = start.take() {
                    out.push(&s[from..i]);
                }
                continue;
            }
            _ => {}
        }
        start.get_or_insert(i);
    }
    if let Some(from) = start {
        out.push(&s[from..]);
    }
    out
}

fn parse_compound(s: &str) -> Option<Compound> {
    let mut c = Compound::default();
    let mut chars = s.char_indices().peekable();
    let ident = |s: &str, from: usize| -> usize {
        s[from..].find(|ch: char| !(ch.is_alphanumeric() || ch == '-' || ch == '_')).map_or(s.len(), |i| from + i)
    };
    let mut i = 0;
    if let Some(&(_, first)) = chars.peek() {
        if first == '*' {
            i = 1;
        } else if first.is_alphabetic() {
            let end = ident(s, 0);
            c.kind = Some(s[..end].to_ascii_lowercase());
            i = end;
        }
    }
    while i < s.len() {
        let ch = s[i..].chars().next()?;
        match ch {
            '.' | '#' => {
                let end = ident(s, i + 1);
                if end == i + 1 {
                    return None;
                }
                let name = s[i + 1..end].to_string();
                if ch == '.' {
                    c.classes.push(name);
                } else {
                    c.id = Some(name);
                }
                i = end;
            }
            '[' => {
                let close = s[i..].find(']').map(|j| i + j)?;
                let body = &s[i + 1..close];
                c.attributes.push(match body.split_once('=') {
                    Some((name, value)) => {
                        let value = value.trim();
                        let value = value.strip_prefix('"').and_then(|v| v.strip_suffix('"'))
                            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                            .unwrap_or(value);
                        Attribute::Equals(name.trim().to_ascii_lowercase(), value.to_string())
                    }
                    None => Attribute::Present(body.trim().to_ascii_lowercase()),
                });
                i = close + 1;
            }
            _ => return None,
        }
    }
    Some(c)
}

fn parse_declarations(body: &str) -> Vec<Declaration> {
    let mut out = Vec::new();
    for declaration in body.split(';') {
        let Some((name, value)) = declaration.split_once(':') else { continue };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        let value = value.strip_suffix("!important").map_or(value, str::trim_end);
        let lower = value.to_ascii_lowercase();
        let parsed = match name.as_str() {
            "color" => color(&lower).map(Declaration::Color),
            "background-color" => color(&lower).map(Declaration::Background),
            "background" if lower == "none" => Some(Declaration::Background([0, 0, 0, 0])),
            "background" => lower.split_whitespace().find_map(color).or_else(|| color(&lower)).map(Declaration::Background),
            "font-weight" => match lower.as_str() {
                "bold" | "bolder" => Some(true),
                "normal" | "lighter" => Some(false),
                n => n.parse::<u32>().ok().map(|w| w >= 600),
            }
            .map(Declaration::Bold),
            "font-style" => match lower.as_str() {
                "italic" => Some(true),
                "normal" => Some(false),
                s if s.starts_with("oblique") => Some(true),
                _ => None,
            }
            .map(Declaration::Italic),
            "text-decoration" | "text-decoration-line" => {
                let words: Vec<&str> = lower.split_whitespace().collect();
                (!words.is_empty()).then(|| Declaration::Decoration {
                    underline: words.contains(&"underline"),
                    line_through: words.contains(&"line-through"),
                })
            }
            "text-shadow" => shadow(&lower).map(Declaration::Shadow),
            "opacity" => {
                let v = lower.strip_suffix('%').map_or_else(|| lower.parse::<f32>().ok(), |p| p.parse::<f32>().ok().map(|v| v / 100.0));
                v.filter(|v| v.is_finite()).map(|v| Declaration::Opacity(v.clamp(0.0, 1.0)))
            }
            "font-family" => value
                .split(',')
                .next()
                .map(|first| first.trim().trim_matches(|c| c == '"' || c == '\''))
                .filter(|first| !first.is_empty())
                .map(|first| Declaration::Family(first.to_string())),
            "font-size" => font_size(&lower),
            _ => None,
        };
        out.extend(parsed);
    }
    out
}

/// A CSS length in pixels of the default font (16 px).
fn length(s: &str) -> Option<f32> {
    let v = |n: &str| n.parse::<f32>().ok().filter(|v| v.is_finite());
    if s == "0" {
        Some(0.0)
    } else if let Some(n) = s.strip_suffix("px") {
        v(n)
    } else if let Some(n) = s.strip_suffix("em") {
        v(n).map(|e| e * 16.0)
    } else {
        None
    }
}

fn shadow(s: &str) -> Option<Option<Shadow>> {
    if s == "none" {
        return Some(None);
    }
    // The first shadow of a list: two or three lengths and a colour.
    let first = split_top_level(s, ',').into_iter().next()?;
    let mut lengths = Vec::new();
    let mut color_value = None;
    for word in split_top_level(first.trim(), ' ') {
        let word = word.trim();
        if word.is_empty() {
            continue;
        }
        match length(word) {
            Some(l) => lengths.push(l),
            None => color_value = Some(color(word)?),
        }
    }
    match lengths.as_slice() {
        [dx, dy] | [dx, dy, _] => Some(Some(Shadow { dx: *dx, dy: *dy, color: color_value })),
        _ => None,
    }
}

fn font_size(s: &str) -> Option<Declaration> {
    let v = |n: &str| n.parse::<f32>().ok().filter(|v| v.is_finite() && *v > 0.0);
    let (factor, of_parent) = match s {
        "xx-small" => (0.6, false),
        "x-small" => (0.75, false),
        "small" => (0.89, false),
        "medium" => (1.0, false),
        "large" => (1.2, false),
        "x-large" => (1.5, false),
        "xx-large" => (2.0, false),
        "smaller" => (1.0 / 1.2, true),
        "larger" => (1.2, true),
        s if s.ends_with('%') => (v(&s[..s.len() - 1])? / 100.0, true),
        s if s.ends_with("em") => (v(&s[..s.len() - 2])?, true),
        // The default cue font is 5vh.
        s if s.ends_with("vh") => (v(&s[..s.len() - 2])? / 5.0, false),
        s if s.ends_with("px") => (v(&s[..s.len() - 2])? / 16.0, false),
        _ => return None,
    };
    Some(Declaration::Size { factor, of_parent })
}

/// `s` split at `sep` outside parentheses.
fn split_top_level(s: &str, sep: char) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut start) = (0i32, 0);
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            c if c == sep && depth == 0 => {
                out.push(&s[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// A CSS colour: a name, `#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa`,
/// `rgb()`/`rgba()`, `hsl()`/`hsla()` or `transparent`.
pub fn color(s: &str) -> Option<Rgba> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        let digits: Vec<u8> = hex.chars().map(|c| c.to_digit(16).map(|d| d as u8)).collect::<Option<_>>()?;
        return match digits.as_slice() {
            [r, g, b] => Some([r * 17, g * 17, b * 17, 255]),
            [r, g, b, a] => Some([r * 17, g * 17, b * 17, a * 17]),
            [r1, r2, g1, g2, b1, b2] => Some([r1 * 16 + r2, g1 * 16 + g2, b1 * 16 + b2, 255]),
            [r1, r2, g1, g2, b1, b2, a1, a2] => Some([r1 * 16 + r2, g1 * 16 + g2, b1 * 16 + b2, a1 * 16 + a2]),
            _ => None,
        };
    }
    if let Some(args) = s.strip_prefix("rgba(").or_else(|| s.strip_prefix("rgb(")).and_then(|a| a.strip_suffix(')')) {
        let parts: Vec<&str> = args.split(|c| c == ',' || c == '/' || c == ' ').map(str::trim).filter(|p| !p.is_empty()).collect();
        let channel = |p: &str| -> Option<u8> {
            let v = match p.strip_suffix('%') {
                Some(pct) => pct.parse::<f32>().ok()? * 2.55,
                None => p.parse::<f32>().ok()?,
            };
            v.is_finite().then(|| v.round().clamp(0.0, 255.0) as u8)
        };
        let alpha = |p: &str| -> Option<u8> {
            let v = match p.strip_suffix('%') {
                Some(pct) => pct.parse::<f32>().ok()? / 100.0,
                None => p.parse::<f32>().ok()?,
            };
            v.is_finite().then(|| (v.clamp(0.0, 1.0) * 255.0).round() as u8)
        };
        return match parts.as_slice() {
            [r, g, b] => Some([channel(r)?, channel(g)?, channel(b)?, 255]),
            [r, g, b, a] => Some([channel(r)?, channel(g)?, channel(b)?, alpha(a)?]),
            _ => None,
        };
    }
    if let Some(args) = s.strip_prefix("hsla(").or_else(|| s.strip_prefix("hsl(")).and_then(|a| a.strip_suffix(')')) {
        let parts: Vec<&str> = args.split(|c| c == ',' || c == '/' || c == ' ').map(str::trim).filter(|p| !p.is_empty()).collect();
        let num = |p: &str| p.trim_end_matches(|c| c == '%' || c == 'd' || c == 'e' || c == 'g').parse::<f32>().ok().filter(|v| v.is_finite());
        let (h, sat, l) = (num(parts.first()?)?, num(parts.get(1)?)? / 100.0, num(parts.get(2)?)? / 100.0);
        let a = parts.get(3).map_or(Some(1.0), |p| match p.strip_suffix('%') {
            Some(pct) => pct.parse::<f32>().ok().map(|v| v / 100.0),
            None => p.parse::<f32>().ok(),
        })?;
        let (sat, l) = (sat.clamp(0.0, 1.0), l.clamp(0.0, 1.0));
        let c = (1.0 - (2.0 * l - 1.0).abs()) * sat;
        let hp = h.rem_euclid(360.0) / 60.0;
        let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
        let (r, g, b) = match hp as u32 {
            0 => (c, x, 0.0),
            1 => (x, c, 0.0),
            2 => (0.0, c, x),
            3 => (0.0, x, c),
            4 => (x, 0.0, c),
            _ => (c, 0.0, x),
        };
        let m = l - c / 2.0;
        let to = |v: f32| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8;
        return Some([to(r), to(g), to(b), (a.clamp(0.0, 1.0) * 255.0).round() as u8]);
    }
    if s == "transparent" {
        return Some([0, 0, 0, 0]);
    }
    NAMED_COLORS.binary_search_by(|(name, _)| name.cmp(&s)).ok().map(|i| {
        let rgb = NAMED_COLORS[i].1;
        [(rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8, 255]
    })
}

/// CSS Color Level 4 named colours, sorted by name.
const NAMED_COLORS: &[(&str, u32)] = &[
    ("aliceblue", 0xf0f8ff), ("antiquewhite", 0xfaebd7), ("aqua", 0x00ffff), ("aquamarine", 0x7fffd4), ("azure", 0xf0ffff),
    ("beige", 0xf5f5dc), ("bisque", 0xffe4c4), ("black", 0x000000), ("blanchedalmond", 0xffebcd), ("blue", 0x0000ff),
    ("blueviolet", 0x8a2be2), ("brown", 0xa52a2a), ("burlywood", 0xdeb887), ("cadetblue", 0x5f9ea0), ("chartreuse", 0x7fff00),
    ("chocolate", 0xd2691e), ("coral", 0xff7f50), ("cornflowerblue", 0x6495ed), ("cornsilk", 0xfff8dc), ("crimson", 0xdc143c),
    ("cyan", 0x00ffff), ("darkblue", 0x00008b), ("darkcyan", 0x008b8b), ("darkgoldenrod", 0xb8860b), ("darkgray", 0xa9a9a9),
    ("darkgreen", 0x006400), ("darkgrey", 0xa9a9a9), ("darkkhaki", 0xbdb76b), ("darkmagenta", 0x8b008b), ("darkolivegreen", 0x556b2f),
    ("darkorange", 0xff8c00), ("darkorchid", 0x9932cc), ("darkred", 0x8b0000), ("darksalmon", 0xe9967a), ("darkseagreen", 0x8fbc8f),
    ("darkslateblue", 0x483d8b), ("darkslategray", 0x2f4f4f), ("darkslategrey", 0x2f4f4f), ("darkturquoise", 0x00ced1), ("darkviolet", 0x9400d3),
    ("deeppink", 0xff1493), ("deepskyblue", 0x00bfff), ("dimgray", 0x696969), ("dimgrey", 0x696969), ("dodgerblue", 0x1e90ff),
    ("firebrick", 0xb22222), ("floralwhite", 0xfffaf0), ("forestgreen", 0x228b22), ("fuchsia", 0xff00ff), ("gainsboro", 0xdcdcdc),
    ("ghostwhite", 0xf8f8ff), ("gold", 0xffd700), ("goldenrod", 0xdaa520), ("gray", 0x808080), ("green", 0x008000),
    ("greenyellow", 0xadff2f), ("grey", 0x808080), ("honeydew", 0xf0fff0), ("hotpink", 0xff69b4), ("indianred", 0xcd5c5c),
    ("indigo", 0x4b0082), ("ivory", 0xfffff0), ("khaki", 0xf0e68c), ("lavender", 0xe6e6fa), ("lavenderblush", 0xfff0f5),
    ("lawngreen", 0x7cfc00), ("lemonchiffon", 0xfffacd), ("lightblue", 0xadd8e6), ("lightcoral", 0xf08080), ("lightcyan", 0xe0ffff),
    ("lightgoldenrodyellow", 0xfafad2), ("lightgray", 0xd3d3d3), ("lightgreen", 0x90ee90), ("lightgrey", 0xd3d3d3), ("lightpink", 0xffb6c1),
    ("lightsalmon", 0xffa07a), ("lightseagreen", 0x20b2aa), ("lightskyblue", 0x87cefa), ("lightslategray", 0x778899), ("lightslategrey", 0x778899),
    ("lightsteelblue", 0xb0c4de), ("lightyellow", 0xffffe0), ("lime", 0x00ff00), ("limegreen", 0x32cd32), ("linen", 0xfaf0e6),
    ("magenta", 0xff00ff), ("maroon", 0x800000), ("mediumaquamarine", 0x66cdaa), ("mediumblue", 0x0000cd), ("mediumorchid", 0xba55d3),
    ("mediumpurple", 0x9370db), ("mediumseagreen", 0x3cb371), ("mediumslateblue", 0x7b68ee), ("mediumspringgreen", 0x00fa9a), ("mediumturquoise", 0x48d1cc),
    ("mediumvioletred", 0xc71585), ("midnightblue", 0x191970), ("mintcream", 0xf5fffa), ("mistyrose", 0xffe4e1), ("moccasin", 0xffe4b5),
    ("navajowhite", 0xffdead), ("navy", 0x000080), ("oldlace", 0xfdf5e6), ("olive", 0x808000), ("olivedrab", 0x6b8e23),
    ("orange", 0xffa500), ("orangered", 0xff4500), ("orchid", 0xda70d6), ("palegoldenrod", 0xeee8aa), ("palegreen", 0x98fb98),
    ("paleturquoise", 0xafeeee), ("palevioletred", 0xdb7093), ("papayawhip", 0xffefd5), ("peachpuff", 0xffdab9), ("peru", 0xcd853f),
    ("pink", 0xffc0cb), ("plum", 0xdda0dd), ("powderblue", 0xb0e0e6), ("purple", 0x800080), ("rebeccapurple", 0x663399),
    ("red", 0xff0000), ("rosybrown", 0xbc8f8f), ("royalblue", 0x4169e1), ("saddlebrown", 0x8b4513), ("salmon", 0xfa8072),
    ("sandybrown", 0xf4a460), ("seagreen", 0x2e8b57), ("seashell", 0xfff5ee), ("sienna", 0xa0522d), ("silver", 0xc0c0c0),
    ("skyblue", 0x87ceeb), ("slateblue", 0x6a5acd), ("slategray", 0x708090), ("slategrey", 0x708090), ("snow", 0xfffafa),
    ("springgreen", 0x00ff7f), ("steelblue", 0x4682b4), ("tan", 0xd2b48c), ("teal", 0x008080), ("thistle", 0xd8bfd8),
    ("tomato", 0xff6347), ("turquoise", 0x40e0d0), ("violet", 0xee82ee), ("wheat", 0xf5deb3), ("white", 0xffffff),
    ("whitesmoke", 0xf5f5f5), ("yellow", 0xffff00), ("yellowgreen", 0x9acd32),
];

/// The style of every node of a cue, by node, in document order: the
/// root's first, then each element's (text and timestamps take their
/// parent's).
pub fn cascade(sheet: &StyleSheet, nodes: &[Node], id: &str) -> Vec<Style> {
    let root = Target::Root { id };
    let mut styles = vec![sheet.style(root, &[], None)];
    fn walk<'a>(sheet: &StyleSheet, nodes: &'a [Node], ancestors: &mut Vec<Target<'a>>, parent: &Style, out: &mut Vec<Style>) {
        for node in nodes {
            if let Node::Element { kind, classes, annotation, children } = node {
                let target = Target::Element { kind: *kind, classes, annotation };
                let style = sheet.style(target, &ancestors[1..], Some(parent));
                out.push(style.clone());
                ancestors.push(target);
                walk(sheet, children, ancestors, &style, out);
                ancestors.pop();
            }
        }
    }
    let first = styles[0].clone();
    let mut ancestors = vec![root];
    walk(sheet, nodes, &mut ancestors, &first, &mut styles);
    styles
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webvtt_cue::parse;

    #[test]
    fn colours_parse_as_css_writes_them() {
        assert_eq!(color("#f80"), Some([255, 136, 0, 255]));
        assert_eq!(color("#ff880080"), Some([255, 136, 0, 128]));
        assert_eq!(color("rgba(0, 0, 0, 0.8)"), Some([0, 0, 0, 204]));
        assert_eq!(color("rgb(100% 0% 0% / 50%)"), Some([255, 0, 0, 128]));
        assert_eq!(color("hsl(120, 100%, 50%)"), Some([0, 255, 0, 255]));
        assert_eq!(color("rebeccapurple"), Some([0x66, 0x33, 0x99, 255]));
        assert_eq!(color("transparent"), Some([0, 0, 0, 0]));
        assert_eq!(color("#12345"), None);
        assert!(NAMED_COLORS.windows(2).all(|w| w[0].0 < w[1].0), "sorted for the search");
    }

    /// Specificity beats order, order breaks ties; inherited properties
    /// flow to children; background and opacity stay on their node.
    #[test]
    fn the_cascade_decides_as_css_does() {
        let sheet = StyleSheet::from_header(
            b"WEBVTT\n\nSTYLE\n::cue { color: yellow; background-color: rgba(0,0,255,0.5) }\n::cue(.loud) { font-weight: bold; color: red }\n\nSTYLE\n/* later, lower specificity */\n::cue(c) { color: lime; font-size: 150% }\n::cue(v[voice=\"Roger\"]) { font-style: italic; text-decoration: underline }\n::cue(#cue7) { opacity: 0.5 }\nvideo { color: blue }\n",
        );
        let nodes = parse("<v Roger>hi <c.loud>there</c></v><c>x</c>");
        let styles = cascade(&sheet, &nodes, "cue7");
        let [root, voice, loud, plain] = styles.as_slice() else { panic!("{styles:?}") };
        assert_eq!((root.color, root.background, root.opacity), ([255, 255, 0, 255], Some([0, 0, 255, 128]), 0.5));
        assert_eq!((voice.color, voice.italic, voice.underline, voice.background), ([255, 255, 0, 255], true, true, None));
        // `.loud` (0,1,1) outranks the later `c` (0,0,2).
        assert_eq!((loud.color, loud.bold, loud.italic, loud.underline, loud.size), ([255, 0, 0, 255], true, true, true, 1.5));
        assert_eq!((plain.color, plain.size, plain.italic), ([0, 255, 0, 255], 1.5, false));
        assert_eq!(cascade(&sheet, &nodes, "other")[0].opacity, 1.0);
    }

    #[test]
    fn shadows_sizes_and_selectors() {
        let sheet = StyleSheet::parse(
            "::cue(b i) { text-shadow: 2px 1px 3px #000 } ::cue(i) { font-size: 2em } ::cue(rt) { font-size: smaller } ::cue(b, u) { opacity: 40% } ::cue(:past) { color: red } ::cue { font-family: \"Roboto\", sans-serif; text-shadow: none }",
        );
        let nodes = parse("<b><i>x</i></b><i>y</i>");
        let styles = cascade(&sheet, &nodes, "");
        assert_eq!(styles[0].family.as_deref(), Some("Roboto"));
        assert_eq!(styles[2].shadow, Some(Shadow { dx: 2.0, dy: 1.0, color: Some([0, 0, 0, 255]) }));
        assert_eq!(styles[3].shadow, None, "`b i` needs a `b` ancestor");
        assert_eq!((styles[2].size, styles[1].opacity), (2.0, 0.4));
        // Pseudo-classes are not understood: the rule is skipped.
        assert!(styles.iter().all(|s| s.color == [255, 255, 255, 255]));
    }
}
