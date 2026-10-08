//! TTML1 text, timing and inherited inline styles (W3C TTML1, Third Edition,
//! §§8–10). Clean-room. XML entities/DTDs are not enabled; input, tree depth,
//! timing partitions and output are bounded. Cues use the common font renderer.
use std::collections::{HashMap, VecDeque};
use std::io::Read;
use oxideav_core::{CodecId, CodecParameters, CodecResolver, CuePosition, Decoder, Demuxer, Error, Frame, Packet, ReadSeek, Result, Segment, StreamInfo, SubtitleCue, TextAlign, TimeBase};
use roxmltree::{Document, Node};

pub const CODEC_ID: &str = "ttml";
const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
const MAX_BYTES: usize = 1 << 20;
// Standalone documents are decoded once; packets address a cue in that document.
// Container-carried TTML still has XML payloads and no such private header.
const DOCUMENT: &[u8] = b"PT-TTML-DOCUMENT\0";

fn attr<'a>(node: Node<'a, '_>, key: &str) -> Option<&'a str> {
    node.attributes().find(|a| a.name() == key).map(|a| a.value())
}
fn number(value: &str) -> Option<f64> { value.parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0) }
fn time(value: &str, frame_rate: f64, sub_frame_rate: f64, tick_rate: f64) -> Option<i64> {
    let seconds = if value.contains(':') {
        let parts: Vec<_> = value.split(':').collect();
        if !(3..=4).contains(&parts.len()) { return None; }
        let frames = if parts.len() == 4 {
            let (frame, sub) = parts[3].split_once('.').unwrap_or((parts[3], "0"));
            (number(frame)? + number(sub)? / sub_frame_rate) / frame_rate
        } else { 0.0 };
        number(parts[0])? * 3600.0 + number(parts[1])? * 60.0 + number(parts[2])? + frames
    } else {
        let (v, scale) = [("ms", 0.001), ("h", 3600.0), ("m", 60.0), ("s", 1.0), ("f", 1.0 / frame_rate), ("t", 1.0 / tick_rate)]
            .into_iter().find_map(|(suffix, scale)| value.strip_suffix(suffix).map(|v| (v, scale)))?;
        number(v)? * scale
    };
    (seconds.is_finite() && seconds <= i64::MAX as f64 / 1e6).then_some((seconds * 1e6).round() as i64)
}

#[derive(Clone, Default)]
struct Style { bold: bool, italic: bool, underline: bool, strike: bool, family: Option<String>, size: Option<f32>, colour: Option<(u8,u8,u8)>, align: TextAlign, position: Option<(f32,f32)> }
fn style(node: Node<'_, '_>, defs: &HashMap<String, Node<'_, '_>>, mut inherited: Style, depth: u8, budget: &mut usize) -> Style {
    if depth >= 32 || *budget == 0 { return inherited; }
    *budget -= 1;
    if let Some(ids) = attr(node, "style") {
        for id in ids.split_whitespace().take(32) { if let Some(&definition) = defs.get(id) { inherited = style(definition, defs, inherited, depth + 1, budget); } }
    }
    for a in node.attributes() {
        match a.name() {
            "fontWeight" => inherited.bold = a.value() == "bold",
            "fontStyle" => inherited.italic = a.value() == "italic" || a.value() == "oblique",
            "textDecoration" => {
                for item in a.value().split_whitespace() { match item { "underline" => inherited.underline = true, "noUnderline" => inherited.underline = false, "lineThrough" => inherited.strike = true, "noLineThrough" => inherited.strike = false, _ => {} } }
            }
            "fontFamily" => inherited.family = a.value().split(',').next().map(|s| s.trim_matches([' ', '\'', '"']).to_owned()),
            "fontSize" => {
                let v = a.value().split_whitespace().next().unwrap_or("");
                inherited.size = v.strip_suffix('%').and_then(number).map(|n| inherited.size.unwrap_or(16.0) * n as f32 / 100.0)
                    .or_else(|| v.strip_suffix("px").and_then(number).map(|n| n as f32))
                    .or_else(|| v.strip_suffix('c').and_then(number).map(|n| n as f32 * 19.2)).filter(|n| *n > 0.0 && *n <= 8192.0);
            }
            "color" => {
                if let Some(c) = crate::webvtt_css::color(a.value()) { inherited.colour = Some((c[0], c[1], c[2])); }
            }
            "textAlign" => inherited.align = match a.value() { "left" => TextAlign::Left, "right" => TextAlign::Right, "end" => TextAlign::End, "center" => TextAlign::Center, _ => TextAlign::Start },
            "origin" => {
                let values: Vec<_> = a.value().split_whitespace().take(2).collect();
                if values.len() == 2 {
                    let coord = |s: &str, dimension: f64| s.strip_suffix('%').and_then(number).map(|v| v * dimension / 100.0).or_else(|| s.strip_suffix("px").and_then(number));
                    inherited.position = coord(values[0], 384.0).zip(coord(values[1], 288.0)).map(|(x,y)| (x as f32,y as f32));
                }
            }
            _ => {}
        }
    }
    inherited
}

fn wrap(mut text: Vec<Segment>, style: &Style) -> Vec<Segment> {
    if let Some(rgb) = style.colour { text = vec![Segment::Color { rgb, children: text }]; }
    if style.bold { text = vec![Segment::Bold(text)]; }
    if style.italic { text = vec![Segment::Italic(text)]; }
    if style.underline { text = vec![Segment::Underline(text)]; }
    if style.strike { text = vec![Segment::Strike(text)]; }
    if style.family.is_some() || style.size.is_some() { text = vec![Segment::Font { family: style.family.clone(), size: style.size, children: text }]; }
    text
}

pub fn decode(data: &[u8]) -> Result<Vec<SubtitleCue>> {
    if data.len() > MAX_BYTES { return Err(Error::invalid("TTML input exceeds 1 MiB")); }
    let source = crate::text_common::decode_subtitle_text(data);
    let doc = Document::parse_with_options(&source, roxmltree::ParsingOptions { nodes_limit: 32_768, ..Default::default() }).map_err(|_| Error::invalid("invalid TTML XML"))?;
    let root = doc.root_element();
    if root.tag_name().name() != "tt" { return Err(Error::invalid("TTML root is not tt")); }
    let mut fps = attr(root, "frameRate").and_then(number).filter(|&v| v > 0.0).unwrap_or(30.0);
    if let Some(multiplier) = attr(root, "frameRateMultiplier") {
        let mut parts = multiplier.split_whitespace().filter_map(number);
        if let Some((a,b)) = parts.next().zip(parts.next()).filter(|&(a,b)| a > 0.0 && b > 0.0) { fps *= a / b; }
    }
    let sub_fps = attr(root, "subFrameRate").and_then(number).filter(|&v| v > 0.0).unwrap_or(1.0);
    // TTML1 §6.2.11: without an explicit frameRate, one tick is one second.
    let default_tick = if attr(root, "frameRate").is_some() { fps * sub_fps } else { 1.0 };
    let tick = attr(root, "tickRate").and_then(number).filter(|&v| v > 0.0).unwrap_or(default_tick);
    if !fps.is_finite() || fps <= 0.0 || !tick.is_finite() { return Err(Error::invalid("invalid TTML clock rate")); }
    let defs: HashMap<_,_> = doc.descendants().filter(|n| matches!(n.tag_name().name(), "style" | "region"))
        .filter_map(|n| n.attribute((XML_NS, "id")).map(|id| (id.to_owned(), n))).collect();
    let mut timings = HashMap::new();
    for node in doc.descendants().filter(Node::is_element) {
        let depth = node.ancestors().take(34).count();
        if depth > 32 { return Err(Error::invalid("TTML nesting exceeds 32")); }
        let (parent_start, parent_end) = node.parent().and_then(|p| timings.get(&p.id()).copied()).unwrap_or((0i64, i64::MAX));
        let base = if node.parent().is_some_and(|p| attr(p, "timeContainer") == Some("seq")) {
            node.prev_siblings().skip(1).find(|p| p.is_element()).and_then(|p| timings.get(&p.id())).map_or(parent_start, |&(_,end)| end)
        } else { parent_start };
        let start = base.saturating_add(attr(node, "begin").and_then(|v| time(v, fps, sub_fps, tick)).unwrap_or(0));
        let end = attr(node, "end").and_then(|v| time(v, fps, sub_fps, tick)).map(|v| base.saturating_add(v)).unwrap_or(parent_end)
            .min(attr(node, "dur").and_then(|v| time(v, fps, sub_fps, tick)).map(|v| start.saturating_add(v)).unwrap_or(parent_end)).min(parent_end);
        timings.insert(node.id(), (start, end));
    }
    fn text(node: Node<'_, '_>, defs: &HashMap<String, Node<'_, '_>>, timings: &HashMap<roxmltree::NodeId,(i64,i64)>, inherited: &Style, at: i64, preserve: bool, depth: u8) -> Vec<Segment> {
        if depth >= 32 || timings.get(&node.id()).is_some_and(|&(start,end)| at < start || at >= end) { return Vec::new(); }
        if node.is_text() {
            let raw = node.text().unwrap_or("");
            let value = if preserve { raw.into() } else {
                let mut out = String::new(); let mut space = false;
                for c in raw.chars() { if c.is_whitespace() { if !space { out.push(' '); } space = true; } else { out.push(c); space = false; } }
                out
            };
            return wrap(vec![Segment::Text(value)], inherited);
        }
        if node.tag_name().name() == "br" { return vec![Segment::LineBreak]; }
        let s = style(node, defs, inherited.clone(), 0, &mut 4096);
        let preserve = node.attribute((XML_NS,"space")).map_or(preserve, |v| v == "preserve");
        node.children().flat_map(|c| text(c, defs, timings, &s, at, preserve, depth + 1)).collect()
    }
    let mut cues = Vec::new();
    let mut output_bytes = 0usize;
    for p in doc.descendants().filter(|n| n.has_tag_name("p")) {
        let &(start,end) = timings.get(&p.id()).unwrap();
        if end <= start || end == i64::MAX { continue; }
        let mut inherited = Style { align: TextAlign::Center, ..Style::default() };
        let chain: Vec<_> = p.ancestors().skip(1).filter(Node::is_element).collect();
        for &ancestor in chain.iter().rev() {
            if let Some(region) = attr(ancestor,"region").and_then(|id| defs.get(id)) { inherited = style(*region, &defs, inherited, 0, &mut 4096); }
            inherited = style(ancestor, &defs, inherited, 0, &mut 4096);
        }
        if let Some(region) = attr(p, "region").and_then(|id| defs.get(id)) { inherited = style(*region, &defs, inherited, 0, &mut 4096); }
        let mut boundaries = vec![start,end];
        for n in p.descendants() { if let Some(&(a,b)) = timings.get(&n.id()) { boundaries.extend([a.clamp(start,end),b.clamp(start,end)]); } }
        boundaries.sort_unstable(); boundaries.dedup();
        for range in boundaries.windows(2) {
            if cues.len() >= 4096 { return Err(Error::invalid("too many TTML timing intervals")); }
            let preserve = chain.iter().find_map(|n| n.attribute((XML_NS, "space"))).is_some_and(|v| v == "preserve");
            let segments = text(p, &defs, &timings, &inherited, range[0], preserve, 0);
            if segments.is_empty() { continue; }
            // A hostile set of timed spans cannot duplicate a large paragraph
            // thousands of times without hitting the decoded-output cap.
            output_bytes = output_bytes.saturating_add(p.range().len());
            if output_bytes > 16 << 20 { return Err(Error::invalid("TTML output exceeds 16 MiB")); }
            let own_style = style(p, &defs, inherited.clone(), 0, &mut 4096);
            cues.push(SubtitleCue { start_us: range[0], end_us: range[1], style_ref: None,
                positioning: Some(CuePosition { x: own_style.position.map(|p|p.0), y: own_style.position.map(|p|p.1), align: own_style.align, size: None }), segments });
        }
    }
    cues.sort_by_key(|c| c.start_us);
    Ok(cues)
}

pub fn probe(data: &oxideav_core::ProbeData) -> oxideav_core::ProbeScore {
    let text = String::from_utf8_lossy(data.buf);
    if text.contains("<tt") && (text.contains("http://www.w3.org/ns/ttml") || text.contains("http://www.w3.org/2006/10/ttaf1")) { 100 } else { 0 }
}
pub fn open_demuxer(mut input: Box<dyn ReadSeek>, _: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    let mut data = Vec::new();
    std::io::Read::take(&mut input, MAX_BYTES as u64 + 1).read_to_end(&mut data)?;
    let cues = decode(&data)?;
    let duration = cues.iter().map(|c| c.end_us).max().unwrap_or(0);
    let time_base = TimeBase::new(1,1_000_000);
    let mut params = CodecParameters::subtitle(CodecId::new(CODEC_ID));
    params.extradata = [DOCUMENT, &data].concat();
    let packets = cues.iter().enumerate().map(|(index, cue)| {
        let mut packet = Packet::new(0, time_base, (index as u32).to_be_bytes().to_vec());
        packet.pts = Some(cue.start_us);
        packet.duration = Some(cue.end_us.saturating_sub(cue.start_us));
        packet
    }).collect();
    Ok(Box::new(crate::text_common::TextSubtitleDemuxer { format_name: CODEC_ID,
        streams: [StreamInfo { index: 0, time_base, start_time: Some(0), duration: Some(duration), params }], packets }))
}

struct TtmlDecoder { id: CodecId, document: Option<Vec<SubtitleCue>>, pending: VecDeque<Frame>, eof: bool }
pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    let document = params.extradata.strip_prefix(DOCUMENT).map(decode).transpose()?;
    Ok(Box::new(TtmlDecoder { id: params.codec_id.clone(), document, pending: VecDeque::new(), eof: false }))
}
impl Decoder for TtmlDecoder {
    fn codec_id(&self) -> &CodecId { &self.id }
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if self.pending.len() >= 4096 { return Err(Error::invalid("TTML cue queue is full")); }
        if let Some(document) = &self.document {
            let index = <[u8; 4]>::try_from(packet.data.as_slice()).map(u32::from_be_bytes).map_err(|_| Error::invalid("invalid TTML cue index"))?;
            let cue = document.get(index as usize).ok_or_else(|| Error::invalid("TTML cue index out of bounds"))?;
            self.pending.push_back(Frame::Subtitle(cue.clone()));
        } else {
            let cues = decode(&packet.data)?;
            if self.pending.len() + cues.len() > 4096 { return Err(Error::invalid("TTML cue queue is full")); }
            self.pending.extend(cues.into_iter().map(Frame::Subtitle));
        }
        Ok(())
    }
    fn receive_frame(&mut self) -> Result<Frame> { self.pending.pop_front().ok_or(if self.eof { Error::Eof } else { Error::NeedMore }) }
    fn flush(&mut self) -> Result<()> { self.eof = true; Ok(()) }
    fn reset(&mut self) -> Result<()> { self.pending.clear(); self.eof = false; Ok(()) }
}
