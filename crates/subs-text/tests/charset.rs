//! Character sets are decided per cue. A cue that is valid UTF-8 shows as
//! UTF-8, as FFmpeg shows it; a cue that is not (FFmpeg rejects it without
//! `-sub_charenc`) is read as Windows-1250. One bad cue never changes how
//! the other cues of the file are read.

mod common;

use std::path::PathBuf;

use common::visible_text;
use oxideav_core::{CodecId, CodecParameters, Frame, MediaType, Packet, TimeBase};

/// "café" in UTF-8, then "čaj" in Windows-1250 (invalid as UTF-8).
const VALID: &[u8] = b"caf\xc3\xa9";
const INVALID: &[u8] = b"\xe8aj";

fn file(name: &str, parts: &[&[u8]]) -> PathBuf {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("subs-text-charset");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, parts.concat()).unwrap();
    path
}

fn shown_cues(path: &PathBuf) -> Vec<String> {
    let decoded = refcheck::decode(path, &[codecs::register_all], MediaType::Subtitle, 0);
    decoded
        .frames
        .iter()
        .map(|f| {
            let Frame::Subtitle(cue) = f else { panic!("non-subtitle frame") };
            let mut text = String::new();
            visible_text(&cue.segments, &mut text);
            text.trim().to_string()
        })
        .collect()
}

fn assert_cues(name: &str, parts: &[&[u8]], expected: &[&str]) {
    let path = file(name, parts);
    assert_eq!(shown_cues(&path), expected, "{name}");
}

#[test]
fn standalone_files_decide_each_cue() {
    let cafe_caj = ["café", "čaj"];
    assert_cues("cues.srt", &[b"1\n00:00:01,000 --> 00:00:02,000\n", VALID, b"\n\n2\n00:00:03,000 --> 00:00:04,000\n", INVALID, b"\n"], &cafe_caj);
    assert_cues(
        "cues.ass",
        &[
            b"[Script Info]\nScriptType: v4.00+\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n",
            b"Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,", VALID,
            b"\nDialogue: 0,0:00:03.00,0:00:04.00,Default,,0,0,0,,", INVALID, b"\n",
        ],
        &cafe_caj,
    );
    assert_cues("cues.vtt", &[b"WEBVTT\n\n00:01.000 --> 00:02.000\n", VALID, b"\n\n00:03.000 --> 00:04.000\n", INVALID, b"\n"], &cafe_caj);
    assert_cues("cues.sub", &[b"{25}{50}", VALID, b"\n{75}{100}", INVALID, b"\n{125}{150}x\n"], &["café", "čaj", "x"]);
    assert_cues(
        "cues_subviewer.sub",
        &[b"[INFORMATION]\n[END INFORMATION]\n[SUBTITLE]\n00:00:01.00,00:00:02.00\n", VALID, b"\n\n00:00:03.00,00:00:04.00\n", INVALID, b"\n"],
        &cafe_caj,
    );
    assert_cues(
        "cues.smi",
        &[b"<SAMI><BODY>\n<SYNC Start=1000><P Class=ENCC>", VALID, b"\n<SYNC Start=3000><P Class=ENCC>", INVALID, b"\n</BODY></SAMI>\n"],
        &cafe_caj,
    );
    assert_cues("cues.txt", &[b"0:00:01:", VALID, b"\n0:00:03:", INVALID, b"\n"], &cafe_caj);
    assert_cues(
        "cues_subviewer1.sub",
        &[b"******** START SCRIPT ********\n[00:00:01]\n", VALID, b"\n[00:00:02]\n\n[00:00:03]\n", INVALID, b"\n[00:00:04]\n\n"],
        &cafe_caj,
    );
}

/// Matroska blocks: a valid cue keeps UTF-8, an invalid one is read as
/// Windows-1250 rather than dropped.
#[test]
fn container_packets_decide_each_cue() {
    let ctx = codecs::context();
    for (codec, prefix) in [("subrip", &b""[..]), ("ass", &b"0,0,Default,,0,0,0,,"[..]), ("webvtt", &b""[..])] {
        let mut decoder = ctx.codecs.first_decoder(&CodecParameters::subtitle(CodecId::new(codec))).unwrap();
        let mut shown = Vec::new();
        for (i, body) in [VALID, INVALID].into_iter().enumerate() {
            let packet = Packet::new(0, TimeBase::new(1, 1000), [prefix, body].concat()).with_pts(1000 * i as i64).with_duration(500);
            decoder.send_packet(&packet).unwrap_or_else(|e| panic!("{codec} cue {i}: {e}"));
            let Ok(Frame::Subtitle(cue)) = decoder.receive_frame() else { panic!("{codec} cue {i}: no cue") };
            let mut text = String::new();
            visible_text(&cue.segments, &mut text);
            shown.push(text);
        }
        assert_eq!(shown, ["café", "čaj"], "{codec}");
    }
}
