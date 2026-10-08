//! Damaged input to the forked decoder. Packets of the reference streams
//! (H.263, advanced prediction, H.263+, Intel H.263, QuickTime H.263),
//! damaged at random from a fixed seed, go to one decoder; decoders are
//! also made from damaged stream parameters (Intel H.263 takes its size
//! from them) and fed intact packets. Nothing may panic, every frame must
//! hold the planes its reported size and format need, and after the
//! damage an intact intra picture decodes again.
//!
//! A failure names the input and the case; the same seed replays it.

mod support;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use oxideav_core::{CodecParameters, Decoder, Error, Frame, MediaType, Packet};
use support::*;

/// Damaged packets per input, then damaged setups.
const CASES: usize = 2500;

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

/// The first video stream of `path` as the player opens it: its
/// parameters and every packet.
fn video_packets(path: &Path) -> (CodecParameters, Vec<Packet>) {
    let ctx = codecs::context();
    let format = refcheck::probe_container(&ctx, path).unwrap();
    let file = std::fs::File::open(path).unwrap();
    let mut demuxer = ctx.containers.open_demuxer(&format, Box::new(file), &ctx.codecs).unwrap();
    let stream = demuxer.streams().iter().find(|s| s.params.media_type == MediaType::Video).unwrap().clone();
    let mut packets = Vec::new();
    loop {
        match demuxer.next_packet() {
            Ok(p) if p.stream_index == stream.index => packets.push(p),
            Ok(_) => {}
            Err(Error::Eof) => return (stream.params, packets),
            Err(e) => panic!("{}: demux: {e}", path.display()),
        }
    }
}

/// Receives every ready frame, checks each against the layout reported
/// with it, and counts them.
fn drain(decoder: &mut dyn Decoder) -> usize {
    let mut frames = 0;
    loop {
        let frame = match decoder.receive_frame() {
            Ok(Frame::Video(frame)) => frame,
            Ok(_) => panic!("a frame that is not video"),
            Err(Error::NeedMore | Error::Eof) => return frames,
            Err(e) => panic!("receive_frame: {e}"),
        };
        let (Some((w, h)), Some(format)) = (decoder.output_video_dimensions(), decoder.output_pixel_format()) else {
            panic!("a frame without a reported layout");
        };
        let planes = frame.image_planes();
        assert_eq!(planes.len(), format.plane_count(), "planes");
        for (p, plane) in planes.iter().enumerate() {
            let (_, rows) = format.plane_dimensions(p, w, h).expect("plane geometry");
            let row = format.plane_row_bytes(p, w).expect("plane row bytes");
            assert!(plane.stride >= row, "plane {p}: stride {} < {row}", plane.stride);
            assert!(plane.data.len() >= plane.stride * (rows as usize - 1) + row, "plane {p}: short");
        }
        frames += 1;
    }
}

/// `CASES` packets of `path` into one decoder, damaged but for one in
/// eight, then four intact packets from the first (intra) picture, which
/// must give a frame; then decoders from every pair of edge sizes, and
/// `CASES` from damaged parameters, fed three intact packets each.
fn survive(name: &str, path: &Path, seed: u64) {
    let ctx = codecs::context();
    let (params, packets) = video_packets(path);
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
    let mut frames = 0;
    for packet in &packets[..4] {
        let _ = decoder.send_packet(packet);
        frames += drain(decoder.as_mut());
    }
    let _ = decoder.flush();
    frames += drain(decoder.as_mut());
    assert!(frames > 0, "{name}: no frame from intact packets after the damage (seed {seed:#x})");

    // Each side of every size bound, and the top of the `u32` range,
    // which random picks almost never reach (Intel H.263 takes its size
    // from these parameters).
    const EDGES: [u32; 15] =
        [0, 1, 15, 16, 17, 1151, 1152, 1153, 2047, 2048, 2049, u32::MAX - 16, u32::MAX - 15, u32::MAX - 1, u32::MAX];
    for (w, h) in EDGES.iter().flat_map(|&w| EDGES.iter().map(move |&h| (w, h))) {
        let mut setup = params.clone();
        setup.width = Some(w);
        setup.height = Some(h);
        let run = catch_unwind(AssertUnwindSafe(|| {
            if let Ok(mut decoder) = ctx.codecs.first_decoder(&setup) {
                for packet in &packets[..3] {
                    let _ = decoder.send_packet(packet);
                    drain(decoder.as_mut());
                }
                let _ = decoder.flush();
                drain(decoder.as_mut());
            }
        }));
        assert!(run.is_ok(), "{name}: setup {w}x{h} panicked");
    }

    for case in 0..CASES {
        let mut setup = params.clone();
        setup.extradata = damage(&mut rng, &params.extradata);
        let pick = |rng: &mut Rng| match rng.below(4) {
            0 => None,
            1 => Some(rng.below(3000) as u32),
            2 => Some(rng.next() as u32),
            _ => Some(16 * (1 + rng.below(160)) as u32),
        };
        setup.width = pick(&mut rng);
        setup.height = pick(&mut rng);
        let picks: Vec<usize> = (0..3).map(|_| rng.below(packets.len())).collect();
        let run = catch_unwind(AssertUnwindSafe(|| {
            if let Ok(mut decoder) = ctx.codecs.first_decoder(&setup) {
                for &p in &picks {
                    let _ = decoder.send_packet(&packets[p]);
                    drain(decoder.as_mut());
                }
                let _ = decoder.flush();
                drain(decoder.as_mut());
            }
        }));
        assert!(run.is_ok(), "{name}: setup case {case} (seed {seed:#x}) panicked");
    }
}

#[test]
fn h263_survives_damage() {
    for (test, seed) in [("h263", 0x2631), ("h263-obmc", 0x2632), ("h263p", 0x2633)] {
        let (path, _) = fate_vsynth("vsynth1", test);
        survive(&format!("vsynth1-{test}"), &path, seed);
    }
    let mov = "V-codecs/h263/baikonur_r7_overflight.mov";
    survive(mov, &archive(mov), 0x2634);
}

/// From FFmpeg's video-only remux: oxideav-avi refuses the archive files'
/// MP3 audio header (see `reference.rs`).
#[test]
fn intel_h263_survives_damage() {
    for (file, seed) in [("V-codecs/I263/i263.avi", 0x1263), ("V-codecs/I263/i263_2.avi", 0x1264)] {
        let original = archive(file);
        let name = original.file_stem().unwrap().to_str().unwrap();
        let video = remux(&format!("{name}-video.avi"), &original, &["-map", "0:v:0", "-c", "copy", "-f", "avi"]);
        survive(file, &video, seed);
    }
}
