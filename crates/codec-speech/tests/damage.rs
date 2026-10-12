//! Damaged input. Every decoder gets packets from FATE's samples, damaged
//! at random from a fixed seed, and decoders made from damaged stream
//! parameters; this crate's demuxers (`amr`, `qcp`) get damaged copies
//! of their files, read to the end and seeked. Nothing may panic, and
//! every frame a decoder returns must hold the samples its reported
//! layout says.
//!
//! A failure names the input and the case; the same seed replays it.

mod support;

use std::io::Cursor;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;

use oxideav_core::{CodecParameters, Decoder, Error, Frame, MediaType, Packet, RuntimeContext};
use refcheck::fate;

/// Damaged packets per input, then damaged setups, then damaged files.
const CASES: usize = 2500;
/// Packets read from one damaged file at most.
const MAX_PACKETS: usize = 20_000;

fn context() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    codec_speech::register(&mut ctx);
    oxideav_mov::registry::register(&mut ctx);
    oxideav_mp4::__oxideav_entry(&mut ctx);
    ctx
}

/// xorshift64*.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// 0 .. n (0 for n = 0).
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// `data` damaged one of five ways: bits flipped, cut short, a run of
/// random bytes, a run of 0x00 or 0xff, random bytes appended.
fn damage(rng: &mut Rng, data: &[u8]) -> Vec<u8> {
    let mut out = data.to_vec();
    let len = out.len();
    match rng.below(5) {
        0 if len > 0 => {
            for _ in 0..=rng.below(8) {
                let at = rng.below(len);
                out[at] ^= 1 << rng.below(8);
            }
        }
        1 => out.truncate(rng.below(len)),
        2 if len > 0 => {
            let at = rng.below(len);
            let n = 1 + rng.below(16.min(len - at));
            for b in &mut out[at..at + n] {
                *b = rng.next() as u8;
            }
        }
        3 if len > 0 => {
            let at = rng.below(len);
            let n = 1 + rng.below(64.min(len - at));
            out[at..at + n].fill(if rng.below(2) == 0 { 0 } else { 0xff });
        }
        _ => {
            for _ in 0..=rng.below(32) {
                out.push(rng.next() as u8);
            }
        }
    }
    out
}

/// The first audio stream of `bytes` as the player opens it: the
/// parameters and up to `MAX_PACKETS` packets, stopping at an error.
fn read(ctx: &RuntimeContext, bytes: Vec<u8>, ext: &str, seek_to: Option<i64>) -> Option<(CodecParameters, Vec<Packet>)> {
    let probe = oxideav_core::ProbeData { buf: &bytes[..bytes.len().min(256 * 1024)], ext: Some(ext) };
    let format = ctx.containers.probe_candidates(&probe).first().map(|c| c.name.to_string())?;
    let mut demuxer = ctx.containers.open_demuxer(&format, Box::new(Cursor::new(bytes)), &ctx.codecs).ok()?;
    let stream = demuxer.streams().iter().find(|s| s.params.media_type == MediaType::Audio)?.clone();
    if let Some(pts) = seek_to {
        let _ = demuxer.seek_to(stream.index, pts);
    }
    let mut packets = Vec::new();
    while packets.len() < MAX_PACKETS {
        match demuxer.next_packet() {
            Ok(p) if p.stream_index == stream.index => packets.push(p),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    Some((stream.params, packets))
}

/// Receives every ready frame and checks it against the reported layout.
fn drain(decoder: &mut dyn Decoder) {
    loop {
        let frame = match decoder.receive_frame() {
            Ok(Frame::Audio(frame)) => frame,
            Ok(_) => panic!("a frame that is not audio"),
            Err(Error::NeedMore) => return,
            Err(e) => panic!("receive_frame: {e}"),
        };
        let format = decoder.output_audio_format().expect("a layout for every frame");
        let plane = frame.samples as usize * format.sample_format.bytes_per_sample();
        let channels = usize::from(format.channels);
        if format.sample_format.is_planar() {
            assert_eq!(frame.data.len(), channels, "planes");
            assert!(frame.data.iter().all(|p| p.len() == plane), "plane sizes");
        } else {
            assert_eq!(frame.data.len(), 1, "planes");
            assert_eq!(frame.data[0].len(), plane * channels, "interleaved size");
        }
    }
}

/// `CASES` damaged packets into one decoder (a clean one now and then),
/// `CASES` decoders from damaged parameters fed three packets each, and
/// `CASES` damaged copies of the file through its demuxer, read to the
/// end (from a random seek point in one case of four).
fn survive(path: &Path, seed: u64) {
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    let ext = path.extension().unwrap().to_str().unwrap().to_ascii_lowercase();
    let ctx = context();
    let bytes = std::fs::read(path).unwrap();
    let (params, packets) = read(&ctx, bytes.clone(), &ext, None).unwrap_or_else(|| panic!("{name}: does not open"));
    assert!(!packets.is_empty(), "{name}: no packets");
    let mut rng = Rng(seed);

    let mut decoder = ctx.codecs.first_decoder(&params).unwrap_or_else(|e| panic!("{name}: {e}"));
    for case in 0..CASES {
        let mut packet = packets[rng.below(packets.len())].clone();
        if rng.below(8) != 0 {
            packet.data = damage(&mut rng, &packet.data);
        }
        let run = catch_unwind(AssertUnwindSafe(|| {
            let _ = decoder.send_packet(&packet);
            drain(decoder.as_mut());
        }));
        assert!(run.is_ok(), "{name}: packet case {case} (seed {seed:#x}) panicked");
    }

    for case in 0..CASES {
        let mut setup = params.clone();
        setup.extradata = damage(&mut rng, &params.extradata);
        if rng.below(2) == 0 {
            setup.channels = Some(rng.below(70) as u16);
        }
        if rng.below(4) == 0 {
            setup.sample_rate = Some(rng.next() as u32);
        }
        let picks: Vec<usize> = (0..3).map(|_| rng.below(packets.len())).collect();
        let run = catch_unwind(AssertUnwindSafe(|| {
            if let Ok(mut decoder) = ctx.codecs.first_decoder(&setup) {
                for &p in &picks {
                    let _ = decoder.send_packet(&packets[p]);
                    drain(decoder.as_mut());
                }
            }
        }));
        assert!(run.is_ok(), "{name}: setup case {case} (seed {seed:#x}) panicked");
    }

    // The 3GP files are oxideav-mp4's to read; only this crate's
    // containers get damaged files.
    let ours = matches!(ext.as_str(), "amr" | "qcp");
    for case in (0..CASES).filter(|_| ours) {
        let damaged = damage(&mut rng, &bytes);
        let seek = (rng.below(4) == 0).then(|| rng.next() as i64 % 400_000);
        let run = catch_unwind(AssertUnwindSafe(|| {
            let _ = read(&ctx, damaged, &ext, seek);
        }));
        assert!(run.is_ok(), "{name}: file case {case} (seed {seed:#x}) panicked");
    }
}

#[test]
fn amr_nb_survives_damage() {
    survive(&fate("amrnb/4.75k.amr"), 0xA4B4);
    survive(&fate("amrnb/12.2k.amr"), 0xA12B);
}

#[test]
fn amr_wb_survives_damage() {
    survive(&support::raw_amr_wb("seed-6k60"), 0xB660);
    survive(&support::raw_amr_wb("seed-23k85"), 0xB238);
}

#[test]
fn qcelp_and_qcp_survive_damage() {
    survive(&fate("qcp/0036580847.QCP"), 0x0C1F);
}
