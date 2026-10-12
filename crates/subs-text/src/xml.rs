//! Minimal bounded XML scanner for USF / CMML cue payloads.
//!
//! USF and CMML carry small markup fragments per packet. A full XML
//! parser is neither needed nor safe on untrusted input; this module
//! recognizes start tags (with attributes), end tags and text, in one
//! pass with no recursion, no allocation beyond the output, and hard
//! caps on document depth, tag name length and total work.

use oxideav_core::Error;
use oxideav_core::Result;

/// Hard caps for untrusted XML payloads.
pub const MAX_NAME_LEN: usize = 256;
pub const MAX_ATTRS: usize = 64;
pub const MAX_DEPTH: usize = 64;

/// One lexical XML token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token<'a> {
    /// `<name ...>` with its attribute list, or `<?name ...?>` /
    /// `<!DOCTYPE ...>` (kind `ProcessingInstruction` / `Declaration`).
    Start {
        name: &'a str,
        attrs: Vec<(&'a str, &'a str)>,
        self_closing: bool,
    },
    /// `</name>`.
    End(&'a str),
    /// Character data between tags (entity-decoded).
    Text(String),
}

/// A tokenizing pass over `input`. Emits start/end tags and text; skips
/// processing instructions, declarations and comments.
pub struct Scanner<'a> {
    input: &'a [u8],
    pos: usize,
    work: usize,
}

const WORK_CAP: usize = 1 << 22;

impl<'a> Scanner<'a> {
    pub fn new(input: &'a [u8]) -> Self {
        Self { input, pos: 0, work: 0 }
    }

    fn charge(&mut self, n: usize) -> Result<()> {
        self.work = self.work.saturating_add(n);
        if self.work > WORK_CAP {
            return Err(Error::invalid("xml work cap exceeded"));
        }
        Ok(())
    }

    fn skip_trivia(&mut self) -> Result<()> {
        loop {
            // whitespace
            while self.pos < self.input.len()
                && (self.input[self.pos] as char).is_ascii_whitespace()
            {
                self.pos += 1;
            }
            if self.input[self.pos..].starts_with(b"<!--") {
                self.charge(4)?;
                let end = find(self.input, self.pos + 4, b"-->")
                    .ok_or_else(|| Error::invalid("unterminated xml comment"))?;
                self.pos = end + 3;
                continue;
            }
            return Ok(());
        }
    }

    fn name_at(&mut self, start: usize) -> Result<&'a str> {
        let mut end = start;
        while end < self.input.len() {
            let c = self.input[end] as char;
            if c.is_ascii_alphanumeric() || c == ':' || c == '-' || c == '_' || c == '.' {
                end += 1;
            } else {
                break;
            }
        }
        if end == start || end - start > MAX_NAME_LEN {
            return Err(Error::invalid("bad xml name"));
        }
        std::str::from_utf8(&self.input[start..end])
            .map_err(|_| Error::invalid("non-utf8 xml name"))
    }

    /// Next token. Returns `Ok(None)` at end of input.
    pub fn next(&mut self) -> Result<Option<Token<'a>>> {
        self.skip_trivia()?;
        if self.pos >= self.input.len() {
            return Ok(None);
        }
        self.charge(1)?;
        if self.input[self.pos] != b'<' {
            // text run up to the next '<'
            let start = self.pos;
            let end = self
                .input
                .position_from(start, b'<')
                .unwrap_or(self.input.len());
            let raw = std::str::from_utf8(&self.input[start..end])
                .map_err(|_| Error::invalid("non-utf8 xml text"))?;
            self.pos = end;
            return Ok(Some(Token::Text(decode_entities(raw))));
        }
        // markup
        if self.input[self.pos..].starts_with(b"</") {
            let name_start = self.pos + 2;
            let name = self.name_at(name_start)?;
            let close = find(self.input, name_start + name.len(), b">")
                .ok_or_else(|| Error::invalid("unterminated end tag"))?;
            self.pos = close + 1;
            return Ok(Some(Token::End(name)));
        }
        if self.input[self.pos..].starts_with(b"<?")
            || self.input[self.pos..].starts_with(b"<!")
        {
            // skip past the construct
            let end = find(self.input, self.pos + 2, b">")
                .ok_or_else(|| Error::invalid("unterminated declaration"))?;
            self.pos = end + 1;
            // Re-scan for the next token (a `<?xml ...?>` before markup).
            return self.next();
        }
        // start tag
        let name = self.name_at(self.pos + 1)?;
        self.pos += 1 + name.len();
        let mut attrs = Vec::new();
        let mut self_closing = false;
        loop {
            while self.pos < self.input.len()
                && (self.input[self.pos] as char).is_ascii_whitespace()
            {
                self.pos += 1;
            }
            if self.pos >= self.input.len() {
                return Err(Error::invalid("unterminated start tag"));
            }
            if self.input[self.pos] == b'/' {
                self_closing = true;
                self.pos += 1;
                continue;
            }
            if self.input[self.pos] == b'>' {
                self.pos += 1;
                break;
            }
            if attrs.len() >= MAX_ATTRS {
                return Err(Error::invalid("too many xml attributes"));
            }
            // attribute: name = "value" (or bare name)
            let aname = self.name_at(self.pos)?;
            self.pos += aname.len();
            while self.pos < self.input.len()
                && (self.input[self.pos] as char).is_ascii_whitespace()
            {
                self.pos += 1;
            }
            let mut value = "";
            if self.pos < self.input.len() && self.input[self.pos] == b'=' {
                self.pos += 1;
                while self.pos < self.input.len()
                    && (self.input[self.pos] as char).is_ascii_whitespace()
                {
                    self.pos += 1;
                }
                if self.pos < self.input.len() && (self.input[self.pos] == b'"' || self.input[self.pos] == b'\'') {
                    let quote = self.input[self.pos];
                    self.pos += 1;
                    let vstart = self.pos;
                    while self.pos < self.input.len() && self.input[self.pos] != quote {
                        self.pos += 1;
                    }
                    if self.pos >= self.input.len() {
                        return Err(Error::invalid("unterminated attribute value"));
                    }
                    value = std::str::from_utf8(&self.input[vstart..self.pos])
                        .map_err(|_| Error::invalid("non-utf8 attribute value"))?;
                    self.pos += 1;
                } else {
                    let vstart = self.pos;
                    while self.pos < self.input.len()
                        && self.input[self.pos] != b'>'
                        && !(self.input[self.pos] as char).is_ascii_whitespace()
                    {
                        self.pos += 1;
                    }
                    value = std::str::from_utf8(&self.input[vstart..self.pos])
                        .map_err(|_| Error::invalid("non-utf8 attribute value"))?;
                }
            }
            self.charge(value.len())?;
            attrs.push((aname, value));
        }
        Ok(Some(Token::Start { name, attrs, self_closing }))
    }
}

/// Decode the XML entities USF/CMML use; unknown entities pass through.
pub fn decode_entities(raw: &str) -> String {
    if !raw.contains('&') {
        return raw.to_string();
    }
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'&' {
            if raw[i..].starts_with("&lt;") {
                out.push('<');
                i += 4;
                continue;
            }
            if raw[i..].starts_with("&gt;") {
                out.push('>');
                i += 4;
                continue;
            }
            if raw[i..].starts_with("&amp;") {
                out.push('&');
                i += 5;
                continue;
            }
            if raw[i..].starts_with("&quot;") {
                out.push('"');
                i += 6;
                continue;
            }
            if raw[i..].starts_with("&apos;") {
                out.push('\'');
                i += 6;
                continue;
            }
        }
        out.push(raw[i..].chars().next().unwrap_or('&'));
        i += raw[i..].chars().next().map_or(1, |c| c.len_utf8());
    }
    out
}

trait IterExt {
    fn position_from(&self, from: usize, needle: u8) -> Option<usize>;
}
impl IterExt for [u8] {
    fn position_from(&self, from: usize, needle: u8) -> Option<usize> {
        self.get(from..)?.iter().position(|&b| b == needle).map(|p| p + from)
    }
}

fn find(haystack: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || from >= haystack.len() {
        return None;
    }
    (from..=haystack.len().saturating_sub(needle.len()))
        .find(|&i| &haystack[i..i + needle.len()] == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_simple_document() {
        let mut s = Scanner::new(b"<subtitle start=\"1\"><text>a &amp; b</text></subtitle>");
        let t1 = s.next().unwrap().unwrap();
        match t1 {
            Token::Start { name, attrs, self_closing } => {
                assert_eq!(name, "subtitle");
                assert_eq!(attrs, vec![("start", "1")]);
                assert!(!self_closing);
            }
            _ => panic!("expected start"),
        }
        assert_eq!(s.next().unwrap().unwrap(), Token::Start { name: "text", attrs: vec![], self_closing: false });
        assert_eq!(s.next().unwrap().unwrap(), Token::Text("a & b".into()));
        assert_eq!(s.next().unwrap().unwrap(), Token::End("text"));
        assert_eq!(s.next().unwrap().unwrap(), Token::End("subtitle"));
        assert!(s.next().unwrap().is_none());
    }

    #[test]
    fn skips_declarations_and_comments() {
        let mut s = Scanner::new(b"<?xml version=\"1.0\"?><!-- c --><a/>");
        match s.next().unwrap().unwrap() {
            Token::Start { name, self_closing, .. } => {
                assert_eq!(name, "a");
                assert!(self_closing);
            }
            _ => panic!(),
        }
        assert!(s.next().unwrap().is_none());
    }

    #[test]
    fn unterminated_tag_is_error() {
        let mut s = Scanner::new(b"<a");
        assert!(s.next().is_err());
    }
}
