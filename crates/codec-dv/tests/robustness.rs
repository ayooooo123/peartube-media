//! Untrusted input: truncated and bit-flipped DV frames through the video
//! decoder, Ulead DV audio blocks through the audio decoder, and damaged
//! raw DV files through the demuxer (opened, read to the end, seeked),
//! with a fixed seed: no panic, nothing slow.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use oxideav_core::{CodecId, CodecParameters, CodecTag, Error, Packet, RuntimeContext, TimeBase};
use refcheck::fate;

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

fn made(name: &str, args: &[&str]) -> PathBuf {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("codec-dv-robust-{}-{name}", std::process::id()));
    let out = Command::new(refcheck::system_ffmpeg())
        .args(["-nostdin", "-v", "error", "-y"])
        .args(args)
        .args(["-f", "dv"])
        .arg(&path)
        .output()
        .expect("the fixture FFmpeg runs");
    assert!(out.status.success(), "{name}: {}", String::from_utf8_lossy(&out.stderr));
    path
}

/// Raw DV of three frames: PAL 4:2:0 with audio, NTSC 4:1:1, DVCPRO50 and
/// DVCPRO HD 1080i50 with audio.
fn samples() -> Vec<PathBuf> {
    let video = |size: &str, rate: &str| format!("testsrc=size={size}:rate={rate}:duration=0.12");
    let sine = "sine=frequency=1000:sample_rate=48000:duration=0.12";
    vec![
        made("pal.dv", &["-f", "lavfi", "-i", &video("720x576", "25"), "-f", "lavfi", "-i", sine, "-c:v", "dvvideo", "-pix_fmt", "yuv420p", "-c:a", "pcm_s16le", "-ac", "2"]),
        made("ntsc.dv", &["-f", "lavfi", "-i", &video("720x480", "30000/1001"), "-c:v", "dvvideo", "-pix_fmt", "yuv411p"]),
        made("dv50.dv", &["-f", "lavfi", "-i", &video("720x576", "25"), "-c:v", "dvvideo", "-pix_fmt", "yuv422p"]),
        made("hd.dv", &["-f", "lavfi", "-i", &video("1440x1080", "25"), "-f", "lavfi", "-i", sine, "-c:v", "dvvideo", "-pix_fmt", "yuv422p", "-c:a", "pcm_s16le", "-ac", "2"]),
    ]
}

/// One damaged copy: bit flips (mostly in the DIF block headers and the
/// profile bytes, else anywhere) or a truncation.
fn mutate(rng: &mut Rng, data: &[u8]) -> Vec<u8> {
    let mut out = data.to_vec();
    if out.is_empty() {
        return out;
    }
    match rng.below(4) {
        0 => out.truncate(rng.below(out.len())),
        1 => {
            for _ in 0..1 + rng.below(4) {
                let at = rng.below(out.len() / 80) * 80 + rng.below(8);
                if let Some(b) = out.get_mut(at) {
                    *b ^= 1 << rng.below(8);
                }
            }
        }
        2 => {
            let at = rng.below(480.min(out.len()));
            out[at] ^= 1 << rng.below(8);
        }
        _ => {
            for _ in 0..1 + rng.below(32) {
                let at = rng.below(out.len());
                out[at] ^= 1 << rng.below(8);
            }
        }
    }
    out
}

#[test]
fn damaged_frames_do_not_panic_the_decoder() {
    let mut ctx = RuntimeContext::new();
    codec_dv::register(&mut ctx);
    oxideav_mov::registry::register(&mut ctx);
    let first_frame = |format: &str, path: &Path| {
        let mut d = ctx.containers.open_demuxer(format, Box::new(std::fs::File::open(path).unwrap()), &ctx.codecs).unwrap();
        d.next_packet().unwrap().data
    };
    // The first frame of each made sample and of FATE's DVCPRO HD 720p50.
    let mut frames: Vec<Vec<u8>> = samples().iter().map(|p| first_frame("dv", p)).collect();
    frames.push(first_frame("mov", &fate("dv/dvcprohd_720p50.mov")));
    let mut rng = Rng(0x0D15_EA5E_5EED_0001);
    let mut mutations = 0;
    let start = Instant::now();
    for (k, data) in frames.iter().enumerate() {
        let decoder_params = CodecParameters::video(CodecId::new("dvvideo"));
        let mut decoder = ctx.codecs.first_decoder(&decoder_params).unwrap();
        let rounds = if data.len() > 300_000 { 160 } else { 520 };
        for _ in 0..rounds {
            let packet = Packet::new(0, TimeBase::new(1, 25), mutate(&mut rng, data));
            if decoder.send_packet(&packet).is_ok() {
                let _ = decoder.receive_frame();
            }
            mutations += 1;
        }
        assert!(start.elapsed() < Duration::from_secs(300), "sample {k}: decoding damaged frames is slow");
    }
    assert!(mutations >= 2000, "{mutations} mutations");
}

#[test]
fn damaged_files_do_not_panic_the_demuxer() {
    let mut rng = Rng(0x0D15_EA5E_5EED_0002);
    let mut mutations = 0;
    for path in samples() {
        let data = std::fs::read(&path).unwrap();
        for _ in 0..150 {
            let damaged = mutate(&mut rng, &data);
            let mut ctx = RuntimeContext::new();
            codec_dv::register(&mut ctx);
            let start = Instant::now();
            if let Ok(mut d) = ctx.containers.open_demuxer("dv", Box::new(Cursor::new(damaged)), &ctx.codecs) {
                let mut packets = 0;
                loop {
                    match d.next_packet() {
                        Ok(_) => packets += 1,
                        Err(Error::Eof) | Err(_) => break,
                    }
                    assert!(packets < 1000, "{}: reading never ends", path.display());
                }
                let _ = d.seek_to(0, 2400);
                let _ = d.next_packet();
                let streams = d.streams().len();
                if streams > 1 {
                    let _ = d.seek_to(1, 1);
                }
            }
            assert!(start.elapsed() < Duration::from_secs(10), "{}: slow on damaged input", path.display());
            mutations += 1;
        }
    }
    assert!(mutations >= 600, "{mutations} mutations");
}

#[test]
fn damaged_blocks_do_not_panic_the_audio_decoder() {
    // The audio DIF blocks of the first PAL frame, Ulead's layout.
    let frame = std::fs::read(&samples()[0]).unwrap();
    let mut block = Vec::new();
    for seq in 0..12 {
        for blk in 0..9 {
            let at = seq * 150 * 80 + (6 + 16 * blk) * 80;
            block.extend_from_slice(&frame[at..at + 80]);
        }
    }
    let mut ctx = RuntimeContext::new();
    codec_dv::register(&mut ctx);
    let mut params = CodecParameters::audio(CodecId::new("dvaudio"));
    params.tag = Some(CodecTag::wave_format(0x0216));
    params.sample_rate = Some(48_000);
    let mut decoder = ctx.codecs.first_decoder(&params).unwrap();
    let mut rng = Rng(0x0D15_EA5E_5EED_0003);
    for _ in 0..400 {
        let packet = Packet::new(0, TimeBase::new(1, 48_000), mutate(&mut rng, &block));
        if decoder.send_packet(&packet).is_ok() {
            while decoder.receive_frame().is_ok() {}
        }
    }
}
