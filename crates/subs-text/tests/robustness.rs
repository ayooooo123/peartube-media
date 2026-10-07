//! Untrusted input: every subtitle byte comes from peers, so nothing here
//! may panic.
//!
//! * Decoders: real reference packets — demuxed from FATE samples, or from
//!   files the reference muxer (mkvmerge) wrote from reference sources —
//!   are mutated one at a time and decoded through the production registry
//!   with the stream's real CodecParameters, after the real packets before
//!   them, so header and style state (ASS CodecPrivate, mov_text sample
//!   entry, Kate headers, MicroDVD default style) is present. At least 2000
//!   trials per codec; the extradata itself is mutated in further trials.
//! * Demuxers: reference files, mutated or truncated, go through every
//!   registered probe and through the registered demuxer under test, and
//!   whatever packets come out through the production decoders.
//!
//! CMML has no reference sample anywhere (no FATE sample, no muxer that
//! writes it), so its trials mutate a hand-written document.

use std::fs::File;
use std::io::Cursor;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::Command;

use oxideav_core::{CodecId, CodecParameters, Decoder, Error, Packet, ProbeData, RuntimeContext, TimeBase};
use refcheck::fate;

const DECODER_TRIALS: usize = 2000;
const HEADER_TRIALS: usize = 500;
const FILE_TRIALS: usize = 1000;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }
}

/// Bytes that steer text-subtitle parsers into their markup, timing and
/// encoding paths.
const TOKENS: &[&[u8]] = &[
    b"{\\", b"}", b"{", b"\\N", b"\\n", b"\\", b"<", b">", b"</", b"<b>", b"<font color=\"red\" size=9>", b"</font>",
    b"&amp;", b"&", b"|", b"/", b"\n", b"\r", b"\r\n", b"\n\n", b"\0", b"\xff", b"\xc3", b"\xef\xbb\xbf", b"-->", b",",
    b":", b".", b"{y:ib}", b"{C:$ff00ff}", b"{o:1,2}", b"{1}{2}", b"[br]", b"[SIZE]", b"{\\fnArial}", b"{\\c&HFF00&}",
    b"{\\move(1,2,3,4,5,6)}", b"{\\an8}", b"{\\r}", b"<00:00:01.000>", b"<v A>", b"Dialogue: 0,0:00:01.00,0:00:02.00,",
    b"Style: X,Arial,99999999999,&HFFFFFFFF,-1,-1,", b"[V4+ Styles]\n", b"9999999999", b"-2147483648", b"00:00:01,000 --> 00:00:00,500",
    b"<SYNC Start=", b"<P Class=", b"[00:00:01]", b"WEBVTT\n", b"<text>", b"<subtitle start=\"",
];

fn mutate(rng: &mut Rng, data: &[u8]) -> Vec<u8> {
    let mut out = data.to_vec();
    match rng.below(7) {
        0 => out.truncate(rng.below(out.len() + 1)),
        1 => {
            for _ in 0..1 + rng.below(8) {
                if !out.is_empty() {
                    let i = rng.below(out.len());
                    out[i] ^= 1 << rng.below(8);
                }
            }
        }
        2 => {
            if !out.is_empty() {
                let i = rng.below(out.len());
                out[i] = rng.next() as u8;
            }
        }
        3 => {
            let at = rng.below(out.len() + 1);
            let token = TOKENS[rng.below(TOKENS.len())];
            out.splice(at..at, token.iter().copied());
        }
        4 => {
            if !out.is_empty() {
                let a = rng.below(out.len());
                let b = (a + 1 + rng.below(64)).min(out.len());
                out.drain(a..b);
            }
        }
        5 => {
            if !out.is_empty() {
                let a = rng.below(out.len());
                let b = (a + 1 + rng.below(256)).min(out.len());
                let copy = out[a..b].to_vec();
                let at = rng.below(out.len() + 1);
                out.splice(at..at, copy);
            }
        }
        _ => {
            out.truncate(rng.below(out.len() + 1));
            for _ in 0..rng.below(32) {
                out.push(rng.next() as u8);
            }
        }
    }
    out
}

/// Sends `packet` and drains what the decoder returns; the number of frames.
fn feed(decoder: &mut Box<dyn Decoder>, packet: &Packet) -> usize {
    let _ = decoder.send_packet(packet);
    let mut frames = 0;
    while frames < 100_000 && decoder.receive_frame().is_ok() {
        frames += 1;
    }
    frames
}

fn finish(decoder: &mut Box<dyn Decoder>) -> usize {
    let _ = decoder.flush();
    let mut frames = 0;
    while frames < 100_000 && decoder.receive_frame().is_ok() {
        frames += 1;
    }
    frames
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("subs-text-robustness");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

/// mkvmerge's Matroska remux of `source`: the reference muxer's packets.
fn mkvmerge(source: &Path, name: &str) -> PathBuf {
    let out = scratch(name);
    let status = Command::new("mkvmerge").args(["-q", "-o"]).arg(&out).arg(source).status().expect("run mkvmerge");
    assert!(status.success(), "mkvmerge {}", source.display());
    out
}

/// The stream with codec id `codec` of `path`, demuxed by the production
/// registry: its CodecParameters and every packet.
fn demux(ctx: &RuntimeContext, path: &Path, codec: &str) -> (CodecParameters, Vec<Packet>) {
    let format = refcheck::probe_container(ctx, path).unwrap();
    let mut demuxer = ctx.containers.open_demuxer(&format, Box::new(File::open(path).unwrap()), &ctx.codecs).unwrap();
    let stream = demuxer
        .streams()
        .iter()
        .find(|s| s.params.codec_id.as_str() == codec)
        .unwrap_or_else(|| panic!("{}: no {codec} stream", path.display()))
        .clone();
    let mut packets = Vec::new();
    loop {
        match demuxer.next_packet() {
            Ok(p) if p.stream_index == stream.index => packets.push(p),
            Ok(_) => {}
            Err(Error::Eof) => break,
            Err(e) => panic!("{}: {e}", path.display()),
        }
    }
    assert!(!packets.is_empty(), "{}: no {codec} packets", path.display());
    (stream.params, packets)
}

/// Mutated real packets through a fresh production decoder each trial.
fn mutate_packets(codec: &str, params: &CodecParameters, packets: &[Packet], seed: u64) {
    let ctx = codecs::context();
    let mut clean = ctx.codecs.first_decoder(params).unwrap();
    let cues: usize = packets.iter().map(|p| feed(&mut clean, p)).sum::<usize>() + finish(&mut clean);
    assert!(cues > 0, "{codec}: the unmutated packets decode to no cue, so mutating them proves nothing");

    let mut rng = Rng(seed);
    for trial in 0..DECODER_TRIALS {
        let k = trial % packets.len();
        let mut mutated = packets[k].clone();
        mutated.data = mutate(&mut rng, &mutated.data);
        let result = catch_unwind(AssertUnwindSafe(|| {
            let mut decoder = ctx.codecs.first_decoder(params).unwrap();
            for p in &packets[..k] {
                feed(&mut decoder, p);
            }
            feed(&mut decoder, &mutated);
            for p in packets.iter().skip(k + 1).take(2) {
                feed(&mut decoder, p);
            }
            finish(&mut decoder);
        }));
        assert!(result.is_ok(), "{codec} trial {trial}: packet {k} mutated to {:?} panicked", String::from_utf8_lossy(&mutated.data));
    }
    if params.extradata.is_empty() {
        return;
    }
    for trial in 0..HEADER_TRIALS {
        let mut mutated = params.clone();
        mutated.extradata = mutate(&mut rng, &params.extradata);
        let result = catch_unwind(AssertUnwindSafe(|| {
            if let Ok(mut decoder) = ctx.codecs.first_decoder(&mutated) {
                for p in packets {
                    feed(&mut decoder, p);
                }
                finish(&mut decoder);
            }
        }));
        assert!(result.is_ok(), "{codec} header trial {trial}: extradata {:?} panicked", String::from_utf8_lossy(&mutated.extradata));
    }
}

fn mutate_stream(path: &Path, codec: &str, seed: u64) {
    let ctx = codecs::context();
    let (params, packets) = demux(&ctx, path, codec);
    mutate_packets(codec, &params, &packets, seed);
}

#[test]
fn subrip_packets() {
    mutate_stream(&fate("sub/SubRip_capability_tester.srt"), "subrip", 1);
}

#[test]
fn ass_packets_with_script_header() {
    mutate_stream(&fate("sub/1ededcbd7b.ass"), "ass", 2);
}

#[test]
fn ssa_packets_from_matroska() {
    mutate_stream(&mkvmerge(&fate("sub/a9-misc.ssa"), "a9-misc.mkv"), "ssa", 3);
}

#[test]
fn webvtt_packets() {
    mutate_stream(&fate("sub/WebVTT_capability_tester.vtt"), "webvtt", 4);
}

#[test]
fn microdvd_packets() {
    mutate_stream(&fate("sub/MicroDVD_capability_tester.sub"), "microdvd", 5);
}

#[test]
fn subviewer_packets() {
    mutate_stream(&fate("sub/SubViewer_capability_tester.sub"), "subviewer2", 6);
}

#[test]
fn subviewer1_packets() {
    mutate_stream(&fate("sub/SubViewer1_capability_tester.sub"), "subviewer1", 7);
}

#[test]
fn vplayer_packets() {
    mutate_stream(&fate("sub/VPlayer_capability_tester.txt"), "vplayer", 8);
}

#[test]
fn sami_packets() {
    mutate_stream(&fate("sub/SAMI_capability_tester.smi"), "sami", 9);
}

#[test]
fn mpl2_packets() {
    mutate_stream(&fate("sub/MPL2_capability_tester.txt"), "mpl2", 10);
}

#[test]
fn mov_text_packets_with_sample_entry() {
    mutate_stream(&fate("sub/MovText_capability_tester.mp4"), "mov_text", 11);
}

#[test]
fn kate_packets_with_headers() {
    mutate_stream(&fate("ogg-kate/kate-subtitles.ogg"), "kate", 12);
}

#[test]
fn usf_packets_from_matroska() {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/sample.usf");
    mutate_stream(&mkvmerge(&source, "sample-usf.mkv"), "usf", 13);
}

#[test]
fn cmml_hand_written_document() {
    let document = br#"<cmml>
<clip id="intro" start="0.000" end="2.500">
 <title>Introduction</title>
 <desc>This is the introduction clip.</desc>
</clip>
<clip id="middle" start="npt:3.5" end="npt:5.0">
 <title>Middle section</title>
</clip>
</cmml>"#;
    let params = CodecParameters::subtitle(CodecId::new("cmml"));
    let packet = Packet::new(0, TimeBase::new(1, 1000), document.to_vec());
    mutate_packets("cmml", &params, &[packet], 14);
}

/// The registered demuxer `container`, then the production decoders, over
/// `data`; the number of cues decoded.
fn run_file(ctx: &RuntimeContext, container: &str, data: Vec<u8>) -> usize {
    let Ok(mut demuxer) = ctx.containers.open_demuxer(container, Box::new(Cursor::new(data)), &ctx.codecs) else {
        return 0;
    };
    let mut decoders: Vec<(u32, Box<dyn Decoder>)> = demuxer
        .streams()
        .iter()
        .filter_map(|s| ctx.codecs.first_decoder(&s.params).ok().map(|d| (s.index, d)))
        .collect();
    let mut cues = 0;
    for _ in 0..1_000_000 {
        match demuxer.next_packet() {
            Ok(p) => {
                if let Some((_, decoder)) = decoders.iter_mut().find(|(index, _)| *index == p.stream_index) {
                    cues += feed(decoder, &p);
                }
            }
            Err(_) => break,
        }
    }
    cues + decoders.iter_mut().map(|(_, d)| finish(d)).sum::<usize>()
}

/// Mutated and truncated copies of a reference file through the registered
/// demuxer `container` and the production decoders.
fn mutate_file(sample: &str, container: &str, seed: u64) {
    let path = fate(sample);
    let raw = std::fs::read(&path).unwrap();
    let ctx = codecs::context();
    assert_eq!(refcheck::probe_container(&ctx, &path).unwrap(), container, "{sample} opens with another demuxer");
    assert!(run_file(&ctx, container, raw.clone()) > 0, "{sample}: the unmutated file decodes to no cue");
    let mut rng = Rng(seed);
    for trial in 0..FILE_TRIALS {
        let data = mutate(&mut rng, &raw);
        let result = catch_unwind(AssertUnwindSafe(|| run_file(&ctx, container, data.clone())));
        assert!(result.is_ok(), "{sample} trial {trial}: {:?} panicked", String::from_utf8_lossy(&data));
    }
}

/// The reference subtitle files the probe trials mutate.
const FILES: [&str; 12] = [
    "sub/SubRip_capability_tester.srt",
    "sub/madness.srt",
    "sub/1ededcbd7b.ass",
    "sub/a9-misc.ssa",
    "sub/WebVTT_capability_tester.vtt",
    "sub/WebVTT_extended_tester.vtt",
    "sub/MicroDVD_capability_tester.sub",
    "sub/SubViewer_capability_tester.sub",
    "sub/SubViewer1_capability_tester.sub",
    "sub/VPlayer_capability_tester.txt",
    "sub/SAMI_capability_tester.smi",
    "sub/MPL2_capability_tester.txt",
];

/// Every probe the player's registry runs on an opened file sees the same
/// mutated and truncated subtitle files.
#[test]
fn registered_probes_survive_mutated_subtitle_files() {
    let ctx = codecs::context();
    for (n, sample) in FILES.iter().enumerate() {
        let path = fate(sample);
        let raw = std::fs::read(&path).unwrap();
        let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
        let mut rng = Rng(100 + n as u64);
        for trial in 0..FILE_TRIALS {
            let data = mutate(&mut rng, &raw);
            let probe = ProbeData { buf: &data, ext: ext.as_deref() };
            let result = catch_unwind(AssertUnwindSafe(|| ctx.containers.probe_candidates(&probe).len()));
            assert!(result.is_ok(), "{sample} trial {trial}: probing {:?} panicked", String::from_utf8_lossy(&data));
        }
    }
}

#[test]
fn srt_files() {
    mutate_file("sub/SubRip_capability_tester.srt", "srt", 21);
    mutate_file("sub/madness.srt", "srt", 22);
}

#[test]
fn ass_files() {
    mutate_file("sub/1ededcbd7b.ass", "ass", 23);
    mutate_file("sub/a9-misc.ssa", "ass", 24);
}

#[test]
fn webvtt_files() {
    mutate_file("sub/WebVTT_capability_tester.vtt", "webvtt", 25);
    mutate_file("sub/WebVTT_extended_tester.vtt", "webvtt", 26);
}

#[test]
fn microdvd_files() {
    mutate_file("sub/MicroDVD_capability_tester.sub", "microdvd", 27);
}

#[test]
fn subviewer_files() {
    mutate_file("sub/SubViewer_capability_tester.sub", "subviewer2", 28);
    mutate_file("sub/SubViewer1_capability_tester.sub", "subviewer1", 29);
}

#[test]
fn vplayer_sami_and_mpl2_files() {
    mutate_file("sub/VPlayer_capability_tester.txt", "vplayer", 30);
    mutate_file("sub/SAMI_capability_tester.smi", "sami", 31);
    mutate_file("sub/MPL2_capability_tester.txt", "mpl2", 32);
}
