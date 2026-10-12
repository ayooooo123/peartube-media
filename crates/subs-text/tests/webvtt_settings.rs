//! Untrusted WebVTT placement and styling input, mutated: cue settings,
//! `REGION` headers, MP4 `wvtt` samples, cue text and `STYLE` sheets. Each
//! input gets at least 2000 mutations from a fixed seed. Nothing may
//! panic, and what parses must stay within the WebVTT specification's
//! ranges: percentages in 0..=100, a cue box inside the video, regions
//! only by an id the header defines, styles with alpha and sizes in range.
//!
//! The settings inputs are every cue settings line of FATE's two WebVTT
//! samples plus every setting the specification defines; the cue texts
//! are every cue of those samples; the headers, style sheets and samples
//! are built from the specification's syntax.

use std::panic::{catch_unwind, AssertUnwindSafe};

use subs_text::webvtt::mp4_sample_cues;
use subs_text::webvtt_css::{cascade, StyleSheet};
use subs_text::webvtt_cue::{parse, Node};
use subs_text::webvtt_settings::{header_regions, CueSettings, Region};

const TRIALS: usize = 2000;

/// xorshift64*.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// Bytes the settings syntax gives meaning to, and some it does not.
const ALPHABET: &[u8] = b"0123456789:%,.- \t\nrlinepostsizalgcvtdfbmu\x00\xff";

/// `input` mutated one to four times: a byte replaced, inserted, deleted
/// or duplicated, a run cut, a token repeated, or a number made huge.
fn mutate(rng: &mut Rng, input: &[u8]) -> Vec<u8> {
    let mut out = input.to_vec();
    for _ in 0..=rng.below(4) {
        let len = out.len();
        match rng.below(7) {
            0 if len > 0 => {
                let at = rng.below(len);
                out[at] = ALPHABET[rng.below(ALPHABET.len())];
            }
            1 => out.insert(rng.below(len + 1), ALPHABET[rng.below(ALPHABET.len())]),
            2 if len > 0 => {
                out.remove(rng.below(len));
            }
            3 if len > 0 => {
                let at = rng.below(len);
                out.insert(at, out[at]);
            }
            4 if len > 0 => {
                let at = rng.below(len);
                out.truncate(at + rng.below(len - at));
            }
            5 => {
                let copy = out.clone();
                out.push(b' ');
                out.extend_from_slice(&copy);
            }
            _ => {
                let at = rng.below(len + 1);
                let digits: Vec<u8> = (0..=rng.below(24)).map(|_| b'0' + rng.below(10) as u8).collect();
                out.splice(at..at, digits);
            }
        }
    }
    out
}

fn check_settings(settings: &[u8], regions: &[Region]) {
    let parsed = CueSettings::parse(settings, regions);
    let at = String::from_utf8_lossy(settings);
    let percent = |v: f64| (0.0..=100.0).contains(&v);
    assert!(parsed.position.is_none_or(percent), "{at:?}: position {:?}", parsed.position);
    assert!(percent(parsed.size), "{at:?}: size {}", parsed.size);
    match parsed.line {
        Some(line) if parsed.snap_to_lines => assert!(line.is_finite(), "{at:?}: line {line}"),
        Some(line) => assert!(percent(line), "{at:?}: line {line}%"),
        None => {}
    }
    assert!(parsed.region.as_ref().is_none_or(|r| regions.contains(r)), "{at:?}: region {:?}", parsed.region);
    // The cue box lies within the video, whichever the text's direction.
    for rtl in [false, true] {
        let (start, size) = (parsed.box_start(rtl), parsed.computed_size(rtl));
        assert!(start >= -1e-9 && start + size <= 100.0 + 1e-9 && size >= 0.0, "{at:?} (rtl {rtl}): box {start}..{}", start + size);
    }
    assert!(parsed.computed_line().is_finite(), "{at:?}");
}

#[test]
fn mutated_cue_settings_stay_within_the_specification() {
    let fate = [
        "align:end size:50%",
        "align:start size:50%",
        "size:50% align:start",
        "region:fred align:left",
        "region:bill align:right",
        "vertical:rl",
        "vertical:lr line:0 position:20% size:60% align:start",
    ];
    let spec = [
        "line:-2,end position:10%,line-right size:35.5% align:center",
        "line:62.5%,center position:100%,center align:right",
        "line:0 position:0%,line-left size:100% align:left region:fred",
        "vertical:rl line:-1.5 position:50%,center size:0%",
    ];
    let regions = header_regions(b"WEBVTT\n\nREGION\nid:fred width:40% lines:3\n\nREGION\nid:bill width:40%\n");
    assert_eq!(regions.len(), 2);
    let mut rng = Rng(0x5E77_1265);
    for input in fate.iter().chain(&spec) {
        check_settings(input.as_bytes(), &regions);
        for trial in 0..TRIALS {
            let mutated = mutate(&mut rng, input.as_bytes());
            let run = catch_unwind(AssertUnwindSafe(|| check_settings(&mutated, &regions)));
            assert!(run.is_ok(), "{input:?} trial {trial}: {:?} failed", String::from_utf8_lossy(&mutated));
        }
    }
}

#[test]
fn mutated_region_headers_stay_within_the_specification() {
    let header: &[u8] = b"WEBVTT\n\nREGION\nid:fred width:40% lines:3 regionanchor:0%,100% viewportanchor:10%,90% scroll:up\n\nREGION\nid:bill\nwidth:40%\nlines:3\nregionanchor:100%,100%\nviewportanchor:90%,90%\n";
    let percent = |v: f64| (0.0..=100.0).contains(&v);
    let mut rng = Rng(0x0E61_0465);
    for trial in 0..TRIALS * 2 {
        let mutated = mutate(&mut rng, header);
        let run = catch_unwind(AssertUnwindSafe(|| {
            for region in header_regions(&mutated) {
                assert!(!region.id.is_empty());
                assert!(percent(region.width) && percent(region.region_anchor.0) && percent(region.region_anchor.1));
                assert!(percent(region.viewport_anchor.0) && percent(region.viewport_anchor.1));
            }
        }));
        assert!(run.is_ok(), "trial {trial}: {:?} failed", String::from_utf8_lossy(&mutated));
    }
}

fn mp4_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    [&(8 + body.len() as u32).to_be_bytes()[..], kind, body].concat()
}

#[test]
fn mutated_mp4_samples_unpack_without_panicking() {
    let cue = |iden: &[u8], sttg: &[u8], payl: &[u8]| {
        mp4_box(b"vttc", &[mp4_box(b"iden", iden), mp4_box(b"sttg", sttg), mp4_box(b"payl", payl)].concat())
    };
    let sample = [cue(b"1", b"line:0 align:start", b"<b>one</b>"), mp4_box(b"vtta", b"note"), cue(b"", b"", b"two")].concat();
    let mut rng = Rng(0x0004_7770);
    for trial in 0..TRIALS * 2 {
        let mut mutated = sample.clone();
        for _ in 0..=rng.below(6) {
            let len = mutated.len();
            match rng.below(3) {
                0 => {
                    let at = rng.below(len);
                    mutated[at] = rng.next() as u8;
                }
                1 => mutated.truncate(rng.below(len)),
                _ => {
                    // A box size field set to an edge value.
                    let at = rng.below(len.saturating_sub(4).max(1));
                    let size = [0u32, 1, 7, 8, u32::MAX, len as u32, (len + 1) as u32][rng.below(7)];
                    if at + 4 <= mutated.len() {
                        mutated[at..at + 4].copy_from_slice(&size.to_be_bytes());
                    }
                }
            }
            if mutated.is_empty() {
                break;
            }
        }
        let run = catch_unwind(AssertUnwindSafe(|| {
            if let Some(cues) = mp4_sample_cues(&mutated) {
                let total: usize = cues.iter().map(|(text, m)| text.len() + m.identifier.len() + m.settings.len()).sum();
                assert!(total <= mutated.len());
            }
        }));
        assert!(run.is_ok(), "trial {trial}: {mutated:02x?}");
    }
}

/// Nodes of a parsed cue at most (the parser's bound) and the deepest
/// nesting.
fn shape(nodes: &[Node]) -> (usize, usize) {
    nodes.iter().fold((0, 0), |(count, depth), node| match node {
        Node::Element { children, .. } => {
            let (c, d) = shape(children);
            (count + 1 + c, depth.max(d + 1))
        }
        _ => (count + 1, depth),
    })
}

/// The cue texts of FATE's two WebVTT samples: what follows each timing
/// line up to the blank line.
fn fate_cue_texts() -> Vec<String> {
    ["sub/WebVTT_capability_tester.vtt", "sub/WebVTT_extended_tester.vtt"]
        .iter()
        .flat_map(|sample| {
            let text = std::fs::read_to_string(refcheck::fate(sample)).unwrap().replace("\r\n", "\n");
            text.split("\n\n")
                .filter_map(|block| {
                    let mut lines = block.lines().skip_while(|l| !l.contains("-->"));
                    lines.next()?;
                    Some(lines.collect::<Vec<_>>().join("\n"))
                })
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>()
        })
        .collect()
}

#[test]
fn mutated_cue_text_parses_within_its_bounds() {
    let texts = fate_cue_texts();
    assert!(texts.len() > 20, "{} cue texts", texts.len());
    let sheet = StyleSheet::parse("::cue(c.a v[voice=\"x\"] rt) { color: red; font-size: 300% } ::cue(i) { font-size: 0.5em } ::cue(ruby) { opacity: 0.5 }");
    let mut rng = Rng(0x7e47_c0de);
    for text in &texts {
        for trial in 0..TRIALS {
            let mutated = mutate(&mut rng, text.as_bytes());
            let mutated = String::from_utf8_lossy(&mutated);
            let run = catch_unwind(AssertUnwindSafe(|| {
                let nodes = parse(&mutated);
                let (count, depth) = shape(&nodes);
                assert!(count <= 4096 && depth <= 32, "{count} nodes, depth {depth}");
                for style in cascade(&sheet, &nodes, "id") {
                    assert!((0.0..=1.0).contains(&style.opacity) && (0.25..=8.0).contains(&style.size), "{style:?}");
                }
            }));
            assert!(run.is_ok(), "{text:?} trial {trial}: {mutated:?} failed");
        }
    }
}

#[test]
fn mutated_style_sheets_parse_within_their_bounds() {
    let sheets = [
        "::cue { color: rgba(255,255,0,0.5); background-color: #0008 }\n::cue(.loud) { font-weight: bold; font-size: 150% }",
        "::cue(v[voice=\"Roger Bingham\"]) { text-shadow: 2px 2px 3px hsl(120, 100%, 50%); text-decoration: underline line-through }",
        "::cue(#cue7), ::cue(c.a.b i) { opacity: 40%; font-family: \"Roboto\", sans-serif; background: none }\n/* comment */ ::cue(lang[lang='en']) { font-style: italic }",
    ];
    let nodes = parse("<v Roger Bingham><c.loud.a.b><i>x</i></c></v><lang en>y</lang>");
    let mut rng = Rng(0x0c55_57e5);
    for css in sheets {
        for trial in 0..TRIALS {
            let mutated = mutate(&mut rng, css.as_bytes());
            let mutated = String::from_utf8_lossy(&mutated);
            let run = catch_unwind(AssertUnwindSafe(|| {
                let sheet = StyleSheet::from_header(format!("WEBVTT\n\nSTYLE\n{mutated}\n").as_bytes());
                for style in cascade(&sheet, &nodes, "cue7") {
                    assert!((0.0..=1.0).contains(&style.opacity) && (0.25..=8.0).contains(&style.size), "{style:?}");
                    assert!(style.shadow.is_none_or(|s| s.dx.is_finite() && s.dy.is_finite()), "{style:?}");
                }
            }));
            assert!(run.is_ok(), "{css:?} trial {trial}: {mutated:?} failed");
        }
    }
}
