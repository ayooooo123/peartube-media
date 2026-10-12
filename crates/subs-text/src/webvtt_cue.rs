//! WebVTT cue text as W3C WebVTT §6.4 parses it: the tokenizer and the
//! node tree a renderer styles and lays out (classes, voices, languages,
//! ruby, inline timestamps).
//!
//! Clean-room implementation from W3C WebVTT: The Web Video Text Tracks
//! Format (<https://www.w3.org/TR/webvtt1/>), §6.4 "WebVTT cue text parsing
//! rules". The `webvtt` decoder keeps FFmpeg's conversion of the text,
//! which drops classes, voices and ruby; this tree is what browsers render
//! from.
//!
//! Character references: numeric ones, and the named ones WebVTT files
//! use (the HTML table is larger; an unknown name stays as text).

/// Which tag an internal node is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Class,
    Italic,
    Bold,
    Underline,
    Ruby,
    RubyText,
    Voice,
    Language,
}

impl Kind {
    /// The tag name that opens and closes it.
    pub fn tag(self) -> &'static str {
        match self {
            Kind::Class => "c",
            Kind::Italic => "i",
            Kind::Bold => "b",
            Kind::Underline => "u",
            Kind::Ruby => "ruby",
            Kind::RubyText => "rt",
            Kind::Voice => "v",
            Kind::Language => "lang",
        }
    }
}

/// A node of the cue text.
#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    Text(String),
    /// An inline timestamp, in milliseconds.
    Timestamp(i64),
    Element {
        kind: Kind,
        classes: Vec<String>,
        /// The voice's name (`<v Name>`) or the language (`<lang en>`).
        annotation: String,
        children: Vec<Node>,
    },
}

/// Most nodes a cue keeps (a hostile cue cannot grow the tree further).
const MAX_NODES: usize = 4096;
/// Deepest nesting kept; deeper tags are ignored.
const MAX_DEPTH: usize = 32;

enum Token {
    Text(String),
    Start { name: String, classes: Vec<String>, annotation: String },
    End(String),
    Timestamp(String),
}

fn is_ws(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\x0c' | ' ')
}

/// Named character references WebVTT files use.
const NAMED: &[(&str, char)] = &[
    ("amp", '&'), ("lt", '<'), ("gt", '>'), ("quot", '"'), ("apos", '\''), ("nbsp", '\u{a0}'),
    ("lrm", '\u{200e}'), ("rlm", '\u{200f}'), ("zwj", '\u{200d}'), ("zwnj", '\u{200c}'),
    ("copy", '©'), ("reg", '®'), ("trade", '™'), ("hellip", '…'), ("mdash", '—'), ("ndash", '–'),
    ("lsquo", '‘'), ("rsquo", '’'), ("ldquo", '“'), ("rdquo", '”'), ("laquo", '«'), ("raquo", '»'),
    ("deg", '°'), ("middot", '·'), ("bull", '•'), ("euro", '€'), ("pound", '£'), ("yen", '¥'),
    ("cent", '¢'), ("sect", '§'), ("para", '¶'), ("times", '×'), ("divide", '÷'), ("iexcl", '¡'),
    ("iquest", '¿'), ("shy", '\u{ad}'),
];

/// A character reference at `rest` (just after `&`): the text it stands
/// for and how many characters it took, or `None` to keep the `&`.
fn char_ref(rest: &[char]) -> Option<(char, usize)> {
    if rest.first() == Some(&'#') {
        let hex = matches!(rest.get(1), Some('x' | 'X'));
        let start = if hex { 2 } else { 1 };
        let radix = if hex { 16 } else { 10 };
        let digits = rest[start..].iter().take_while(|c| c.is_digit(radix)).count();
        if digits == 0 {
            return None;
        }
        let value = rest[start..start + digits].iter().fold(0u32, |n, c| n.saturating_mul(radix).saturating_add(c.to_digit(radix).unwrap_or(0)));
        let semi = rest.get(start + digits) == Some(&';');
        let ch = match value {
            0 => '\u{fffd}',
            v => char::from_u32(v).unwrap_or('\u{fffd}'),
        };
        return Some((ch, start + digits + usize::from(semi)));
    }
    let name_len = rest.iter().take_while(|c| c.is_ascii_alphanumeric()).count();
    if rest.get(name_len) != Some(&';') {
        return None;
    }
    let name: String = rest[..name_len].iter().collect();
    NAMED.iter().find(|(n, _)| *n == name).map(|&(_, ch)| (ch, name_len + 1))
}

fn tokens(text: &str) -> Vec<Token> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let mut data = String::new();
    while i < chars.len() {
        match chars[i] {
            '&' => match char_ref(&chars[i + 1..]) {
                Some((ch, used)) => {
                    data.push(ch);
                    i += 1 + used;
                }
                None => {
                    data.push('&');
                    i += 1;
                }
            },
            '<' => {
                if !data.is_empty() {
                    out.push(Token::Text(std::mem::take(&mut data)));
                }
                i += 1;
                let (token, used) = tag(&chars[i..]);
                out.push(token);
                i += used;
            }
            c => {
                data.push(c);
                i += 1;
            }
        }
    }
    if !data.is_empty() {
        out.push(Token::Text(data));
    }
    out
}

/// The tag after a `<`, and how many characters it took (up to and
/// including its `>`, or to the end).
fn tag(rest: &[char]) -> (Token, usize) {
    let mut i = 0;
    let close = |i: usize| if rest.get(i) == Some(&'>') { i + 1 } else { i };
    match rest.first() {
        Some('/') => {
            let len = rest[1..].iter().take_while(|&&c| c != '>').count();
            (Token::End(rest[1..1 + len].iter().collect()), close(1 + len))
        }
        Some(c) if c.is_ascii_digit() => {
            let len = rest.iter().take_while(|&&c| c != '>').count();
            (Token::Timestamp(rest[..len].iter().collect()), close(len))
        }
        _ => {
            let mut name = String::new();
            while let Some(&c) = rest.get(i) {
                if c == '>' || c == '.' || is_ws(c) {
                    break;
                }
                name.push(c);
                i += 1;
            }
            let mut classes = Vec::new();
            while rest.get(i) == Some(&'.') {
                i += 1;
                let mut class = String::new();
                while let Some(&c) = rest.get(i) {
                    if c == '>' || c == '.' || is_ws(c) {
                        break;
                    }
                    class.push(c);
                    i += 1;
                }
                if !class.is_empty() {
                    classes.push(class);
                }
            }
            let mut annotation = String::new();
            if rest.get(i).is_some_and(|&c| is_ws(c)) {
                i += 1;
                while let Some(&c) = rest.get(i) {
                    if c == '>' {
                        break;
                    }
                    if c == '&' {
                        if let Some((ch, used)) = char_ref(&rest[i + 1..]) {
                            annotation.push(ch);
                            i += 1 + used;
                            continue;
                        }
                    }
                    annotation.push(c);
                    i += 1;
                }
            }
            let annotation = annotation.split(is_ws).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ");
            (Token::Start { name, classes, annotation }, close(i))
        }
    }
}

/// "Collect a WebVTT timestamp" over a whole timestamp tag, in ms.
fn timestamp(s: &str) -> Option<i64> {
    let parts: Vec<&str> = s.split(':').collect();
    let (h, m, rest) = match parts.as_slice() {
        [h, m, rest] => (h.parse::<i64>().ok().filter(|_| h.chars().all(|c| c.is_ascii_digit()))?, *m, *rest),
        [m, rest] => (0, *m, *rest),
        _ => return None,
    };
    let (sec, ms) = rest.split_once('.')?;
    let two = |p: &str| (p.len() == 2 && p.chars().all(|c| c.is_ascii_digit())).then(|| p.parse::<i64>().ok()).flatten();
    let (m, sec) = (two(m)?, two(sec)?);
    if ms.len() != 3 || !ms.chars().all(|c| c.is_ascii_digit()) || m > 59 || sec > 59 {
        return None;
    }
    // Hours beyond what milliseconds can count are not a timestamp.
    h.checked_mul(3_600_000)?.checked_add((m * 60 + sec) * 1000 + ms.parse::<i64>().ok()?)
}

/// The node tree of a cue's text (§6.4).
pub fn parse(text: &str) -> Vec<Node> {
    let mut root: Vec<Node> = Vec::new();
    // The open elements, outermost first: each one's index path is its
    // ancestors' child counts, kept as indices into `children`.
    let mut path: Vec<usize> = Vec::new();
    let mut count = 0usize;
    fn current<'a>(root: &'a mut Vec<Node>, path: &[usize]) -> &'a mut Vec<Node> {
        let mut list = root;
        for &i in path {
            match &mut list[i] {
                Node::Element { children, .. } => list = children,
                _ => unreachable!("only elements are open"),
            }
        }
        list
    }
    fn open_kind(root: &mut Vec<Node>, path: &[usize]) -> Option<Kind> {
        let (&last, parent) = path.split_last()?;
        match &current(root, parent)[last] {
            Node::Element { kind, .. } => Some(*kind),
            _ => None,
        }
    }
    for token in tokens(text) {
        if count >= MAX_NODES {
            break;
        }
        match token {
            Token::Text(s) => {
                current(&mut root, &path).push(Node::Text(s));
                count += 1;
            }
            Token::Timestamp(s) => {
                if let Some(ms) = timestamp(&s) {
                    current(&mut root, &path).push(Node::Timestamp(ms));
                    count += 1;
                }
            }
            Token::Start { name, classes, annotation } => {
                let kind = match name.as_str() {
                    "c" => Kind::Class,
                    "i" => Kind::Italic,
                    "b" => Kind::Bold,
                    "u" => Kind::Underline,
                    "ruby" => Kind::Ruby,
                    "rt" if open_kind(&mut root, &path) == Some(Kind::Ruby) => Kind::RubyText,
                    "v" => Kind::Voice,
                    "lang" => Kind::Language,
                    _ => continue,
                };
                if path.len() >= MAX_DEPTH {
                    continue;
                }
                let annotation = if matches!(kind, Kind::Voice | Kind::Language) { annotation } else { String::new() };
                let list = current(&mut root, &path);
                list.push(Node::Element { kind, classes, annotation, children: Vec::new() });
                path.push(list.len() - 1);
                count += 1;
            }
            Token::End(name) => {
                let open = open_kind(&mut root, &path);
                if open.is_some_and(|k| k.tag() == name) {
                    path.pop();
                } else if name == "ruby" && open == Some(Kind::RubyText) {
                    path.pop();
                    path.pop();
                }
            }
        }
    }
    root
}

/// The text of `nodes`, timestamps and markup left out.
pub fn plain_text(nodes: &[Node]) -> String {
    let mut out = String::new();
    fn walk(nodes: &[Node], out: &mut String) {
        for node in nodes {
            match node {
                Node::Text(s) => out.push_str(s),
                Node::Timestamp(_) => {}
                Node::Element { children, .. } => walk(children, out),
            }
        }
    }
    walk(nodes, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(kind: Kind, classes: &[&str], annotation: &str, children: Vec<Node>) -> Node {
        Node::Element { kind, classes: classes.iter().map(|c| c.to_string()).collect(), annotation: annotation.into(), children }
    }

    fn text(s: &str) -> Node {
        Node::Text(s.into())
    }

    #[test]
    fn tags_classes_voices_and_ruby_build_the_tree() {
        let nodes = parse("<v.loud Yumiko  Sato>Hi <c.a.b>there</c></v> <ruby>愛<rt>あい</ruby>!");
        assert_eq!(
            nodes,
            [
                el(Kind::Voice, &["loud"], "Yumiko Sato", vec![text("Hi "), el(Kind::Class, &["a", "b"], "", vec![text("there")])]),
                text(" "),
                el(Kind::Ruby, &[], "", vec![text("愛"), el(Kind::RubyText, &[], "", vec![text("あい")])]),
                text("!"),
            ]
        );
    }

    #[test]
    fn entities_timestamps_and_stray_tags() {
        let nodes = parse("a&amp;b&#x41;&#66;&lrm;&bogus; <00:01.500>x<rt>y</rt></i>z<00:99.000>");
        assert_eq!(nodes, [text("a&bAB\u{200e}&bogus; "), Node::Timestamp(1500), text("x"), text("y"), text("z")]);
        assert_eq!(plain_text(&parse("<b>bold <i>both</b> after")), "bold both after");
        // Hours past what milliseconds count: not a timestamp, no overflow.
        assert_eq!(parse("<9999999999999999:00:00.000>x"), [text("x")]);
        assert_eq!(parse("<2562047788015:00:00.000>x")[0], Node::Timestamp(2562047788015 * 3_600_000));
    }
}
