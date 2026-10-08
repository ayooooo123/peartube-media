//! codec-shorten against FFmpeg 2da55bf (`-cpuflags 0`): the FATE sample
//! of tests/fate/lossless-audio.mak (fate-lossless-shorten:
//! lossless-audio/luckynight-partial.shn) and Shorten streams this test
//! writes (versions 0 to 3; 8-bit unsigned and 16-bit; WAVE, AIFF and AIFC
//! headers; every prediction command, bit shifts, block size changes and
//! verbatim chunks; 1 to 8 channels), each FFmpeg's samples byte for byte
//! and its sample count. The output does not depend on how the stream is
//! cut into packets, as in FFmpeg.

use std::path::{Path, PathBuf};
use std::process::Command;

use oxideav_core::{Decoder, Frame, MediaType, RuntimeContext, SampleFormat};
use refcheck::{decode, fate, pinned_ffmpeg};

fn ffmpeg_pcm(path: &Path, format: SampleFormat) -> Vec<u8> {
    let f = if format == SampleFormat::U8P { "u8" } else { "s16le" };
    let out = Command::new(pinned_ffmpeg())
        .args(["-v", "error", "-nostdin", "-cpuflags", "0", "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-f", f, "-"])
        .output()
        .expect("pinned ffmpeg runs");
    out.stdout
}

/// Planar frames interleaved, as FFmpeg writes them.
fn interleaved(frames: &[Frame], channels: usize, bytes: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for f in frames {
        let Frame::Audio(a) = f else { continue };
        for i in 0..a.samples as usize {
            for plane in a.data.iter().take(channels) {
                out.extend_from_slice(&plane[i * bytes..(i + 1) * bytes]);
            }
        }
    }
    out
}

fn assert_same(name: &str, ours: &[u8], theirs: &[u8]) {
    let first = ours.iter().zip(theirs).position(|(a, b)| a != b);
    assert_eq!(first, None, "{name}: first differing byte");
    assert_eq!(ours.len(), theirs.len(), "{name}: bytes");
}

/// `path` through the `shn` demuxer and decoder equals FFmpeg; the samples
/// per channel.
fn check(path: &Path) -> usize {
    let decoded = decode(path, &[codec_shorten::register], MediaType::Audio, 0);
    let format = decoded.audio_format.expect("output format");
    let bytes = if format.sample_format == SampleFormat::U8P { 1 } else { 2 };
    let channels = usize::from(format.channels);
    let ours = interleaved(&decoded.frames, channels, bytes);
    assert_same(&path.display().to_string(), &ours, &ffmpeg_pcm(path, format.sample_format));
    ours.len() / bytes / channels
}

/// fate-lossless-shorten. The file is cut short: FFmpeg's last call reads
/// past the end ("overread"), fails and loses that block, and so does the
/// port.
#[test]
fn luckynight_matches_ffmpeg() {
    let n = check(&fate("lossless-audio/luckynight-partial.shn"));
    assert_eq!(n, 396_544, "FFmpeg's 1549 blocks");
    eprintln!("luckynight-partial.shn: {n} samples/channel, bit-exact");
}

/// Packets of any size give the same samples: the decoder waits for a
/// frame's worth of bytes, as FFmpeg's does.
#[test]
fn packet_size_does_not_change_the_samples() {
    let data = std::fs::read(fate("lossless-audio/luckynight-partial.shn")).expect("read");
    let header = codec_shorten::parse_stream_header(&data).expect("header");
    let mut params = oxideav_core::CodecParameters::audio(oxideav_core::CodecId::new("shorten"));
    params.channels = Some(header.channels);
    params.sample_rate = Some(header.sample_rate);
    params.sample_format = Some(header.sample_format);
    let mut reference = None;
    for size in [1usize, 7, 1024, 4099, 65536, data.len()] {
        let mut decoder = codec_shorten::ShortenDecoder::new(&params).expect("decoder");
        let mut frames = Vec::new();
        for chunk in data.chunks(size) {
            let p = oxideav_core::Packet::new(0, oxideav_core::TimeBase::new(1, 44100), chunk.to_vec());
            decoder.send_packet(&p).expect("send");
            while let Ok(f) = decoder.receive_frame() {
                frames.push(f);
            }
        }
        decoder.flush().expect("flush");
        while let Ok(f) = decoder.receive_frame() {
            frames.push(f);
        }
        let pcm = interleaved(&frames, 2, 2);
        match &reference {
            None => reference = Some(pcm),
            Some(r) => assert!(pcm == *r, "packets of {size} bytes"),
        }
    }
}

/// A seek goes back to the start; the stream then decodes from its header
/// again, to the same samples.
#[test]
fn a_seek_restarts_the_stream() {
    let path = fate("lossless-audio/luckynight-partial.shn");
    let mut ctx = RuntimeContext::new();
    codec_shorten::register(&mut ctx);
    assert_eq!(refcheck::probe_container(&ctx, &path).as_deref(), Ok("shn"));
    let file = std::fs::File::open(&path).expect("open");
    let mut demuxer = ctx.containers.open_demuxer("shn", Box::new(file), &ctx.codecs).expect("open");
    let params = demuxer.streams()[0].params.clone();
    let mut decoder = codec_shorten::ShortenDecoder::new(&params).expect("decoder");
    let mut first = Vec::new();
    for _ in 0..40 {
        let p = demuxer.next_packet().expect("packet");
        decoder.send_packet(&p).expect("send");
        while let Ok(f) = decoder.receive_frame() {
            first.push(f);
        }
    }
    assert_eq!(demuxer.seek_to(0, 5 * 44100).expect("seek"), 0);
    decoder.reset().expect("reset");
    let mut again = Vec::new();
    for _ in 0..40 {
        let p = demuxer.next_packet().expect("packet");
        decoder.send_packet(&p).expect("send");
        while let Ok(f) = decoder.receive_frame() {
            again.push(f);
        }
    }
    assert!(!first.is_empty());
    assert!(interleaved(&first, 2, 2) == interleaved(&again, 2, 2));
}

// ---------------------------------------------------------------------------
// A Shorten stream writer, for the configurations FATE has no sample of.

struct Bits {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl Bits {
    fn new() -> Self {
        Self { out: Vec::new(), acc: 0, n: 0 }
    }
    fn put(&mut self, bits: u32, v: u64) {
        for i in (0..bits).rev() {
            self.acc = self.acc << 1 | (v >> i & 1);
            self.n += 1;
            if self.n == 8 {
                self.out.push(self.acc as u8);
                self.acc = 0;
                self.n = 0;
            }
        }
    }
    /// An unsigned Rice code: `v >> k` zeros, a one, the low `k` bits.
    fn ur(&mut self, k: u32, v: u64) {
        for _ in 0..v >> k {
            self.put(1, 0);
        }
        self.put(1, 1);
        self.put(k, v & ((1 << k) - 1));
    }
    fn sr(&mut self, k: u32, v: i64) {
        let u = if v < 0 { ((-v as u64) << 1) - 1 } else { (v as u64) << 1 };
        self.ur(k + 1, u);
    }
    fn finish(mut self) -> Vec<u8> {
        while self.n != 0 {
            self.put(1, 0);
        }
        self.out
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[derive(Clone, Copy)]
enum Header {
    Wave,
    Aiff,
    Aifc,
}

struct Spec {
    name: &'static str,
    version: u8,
    u8_type: bool,
    channels: u32,
    blocksize: u32,
    maxnlpc: u32,
    nmean: u32,
    header: Header,
    blocks: usize,
    seed: u64,
}

/// The 44-byte (WAVE) or 54-byte (AIFF/AIFC) header the stream carries.
fn container_header(spec: &Spec) -> Vec<u8> {
    let bits: u16 = if spec.u8_type { 8 } else { 16 };
    let rate = 22050u32;
    match spec.header {
        Header::Wave => {
            let mut h = Vec::new();
            h.extend_from_slice(b"RIFF");
            h.extend_from_slice(&0u32.to_le_bytes());
            h.extend_from_slice(b"WAVEfmt ");
            h.extend_from_slice(&16u32.to_le_bytes());
            h.extend_from_slice(&1u16.to_le_bytes());
            h.extend_from_slice(&(spec.channels as u16).to_le_bytes());
            h.extend_from_slice(&rate.to_le_bytes());
            h.extend_from_slice(&(rate * spec.channels * u32::from(bits) / 8).to_le_bytes());
            h.extend_from_slice(&((spec.channels * u32::from(bits) / 8) as u16).to_le_bytes());
            h.extend_from_slice(&bits.to_le_bytes());
            h.extend_from_slice(b"data");
            h.extend_from_slice(&0u32.to_le_bytes());
            h
        }
        Header::Aiff | Header::Aifc => {
            let mut h = Vec::new();
            h.extend_from_slice(b"FORM");
            h.extend_from_slice(&0u32.to_be_bytes());
            h.extend_from_slice(if matches!(spec.header, Header::Aifc) { b"AIFC" } else { b"AIFF" });
            h.extend_from_slice(b"COMM");
            h.extend_from_slice(&18u32.to_be_bytes());
            h.extend_from_slice(&(spec.channels as u16).to_be_bytes());
            h.extend_from_slice(&0u32.to_be_bytes());
            h.extend_from_slice(&bits.to_be_bytes());
            // 22050 as an 80-bit float: exponent 16383 + 14, mantissa 22050 << 49.
            h.extend_from_slice(&(16383u16 + 14).to_be_bytes());
            h.extend_from_slice(&(22050u64 << 49).to_be_bytes());
            h.extend_from_slice(b"SSND");
            h.extend_from_slice(&0u32.to_be_bytes());
            h.extend_from_slice(&[0; 8]);
            h
        }
    }
}

/// A random but valid Shorten stream for `spec`.
fn stream(spec: &Spec) -> Vec<u8> {
    let mut rng = Rng(spec.seed);
    let mut w = Bits::new();
    let uint = |w: &mut Bits, k: u32, v: u64| {
        if spec.version == 0 {
            w.ur(k, v);
        } else {
            let k = 64 - v.leading_zeros().min(64);
            w.ur(2, u64::from(k));
            w.ur(k, v);
        }
    };
    w.put(32, u64::from(u32::from_be_bytes(*b"ajkg")));
    w.put(8, u64::from(spec.version));
    uint(&mut w, 4, if spec.u8_type { 2 } else { 5 });
    uint(&mut w, 0, u64::from(spec.channels));
    if spec.version > 0 {
        uint(&mut w, 8, u64::from(spec.blocksize));
        uint(&mut w, 2, u64::from(spec.maxnlpc));
        uint(&mut w, 0, u64::from(spec.nmean));
        uint(&mut w, 1, 0);
    }
    let header = container_header(spec);
    w.ur(2, 9);
    w.ur(5, header.len() as u64);
    for &b in &header {
        w.ur(8, u64::from(b));
    }
    let mut blocksize = if spec.version > 0 { spec.blocksize } else { 256 };
    let nwrap = spec.maxnlpc.max(3);
    for block in 0..spec.blocks {
        // Now and then a bit shift, a smaller block or a verbatim chunk.
        match rng.below(12) {
            0 => {
                w.ur(2, 6);
                w.ur(2, rng.below(4));
            }
            1 if blocksize > 1 && block > 2 => {
                // Version 0 codes the new size in as many bits as the old
                // one's `av_log2`.
                let width = 31 - blocksize.leading_zeros().min(31);
                blocksize = 1 + rng.below(u64::from(blocksize)) as u32;
                w.ur(2, 5);
                uint(&mut w, width, u64::from(blocksize));
            }
            2 => {
                w.ur(2, 9);
                let len = rng.below(5);
                w.ur(5, len);
                for _ in 0..len {
                    w.ur(8, rng.below(256));
                }
            }
            _ => {}
        }
        for _ in 0..spec.channels {
            let cmd = match rng.below(7) {
                c @ 0..=3 => c,
                4 if spec.maxnlpc > 0 => 7,
                5 => 8,
                _ => 1,
            };
            w.ur(2, cmd);
            if cmd == 8 {
                continue;
            }
            let energy = rng.below(6) as u32;
            w.ur(3, u64::from(if spec.version == 0 { energy + 1 } else { energy }));
            if cmd == 7 {
                let order = 1 + rng.below(u64::from(spec.maxnlpc.min(nwrap))) as u32;
                w.ur(2, u64::from(order));
                for _ in 0..order {
                    w.sr(5, rng.below(40) as i64 - 20);
                }
            }
            let spread = 1i64 << (energy + 2);
            for _ in 0..blocksize {
                w.sr(energy, rng.below(2 * spread as u64) as i64 - spread);
            }
        }
    }
    w.ur(2, 4);
    w.finish()
}

#[test]
fn written_streams_match_ffmpeg() {
    let specs = [
        Spec { name: "v2-s16-stereo-wave", version: 2, u8_type: false, channels: 2, blocksize: 256, maxnlpc: 0, nmean: 4, header: Header::Wave, blocks: 300, seed: 1 },
        Spec { name: "v3-s16-mono-qlpc", version: 3, u8_type: false, channels: 1, blocksize: 512, maxnlpc: 16, nmean: 4, header: Header::Wave, blocks: 200, seed: 2 },
        Spec { name: "v1-s16-6ch", version: 1, u8_type: false, channels: 6, blocksize: 128, maxnlpc: 8, nmean: 2, header: Header::Wave, blocks: 120, seed: 3 },
        Spec { name: "v0-s16-stereo", version: 0, u8_type: false, channels: 2, blocksize: 256, maxnlpc: 0, nmean: 0, header: Header::Wave, blocks: 150, seed: 4 },
        Spec { name: "v2-u8-mono", version: 2, u8_type: true, channels: 1, blocksize: 256, maxnlpc: 4, nmean: 4, header: Header::Wave, blocks: 200, seed: 5 },
        Spec { name: "v2-s16-aiff", version: 2, u8_type: false, channels: 2, blocksize: 1024, maxnlpc: 32, nmean: 0, header: Header::Aiff, blocks: 60, seed: 6 },
        Spec { name: "v2-s16-aifc-swapped", version: 2, u8_type: false, channels: 2, blocksize: 256, maxnlpc: 2, nmean: 4, header: Header::Aifc, blocks: 100, seed: 7 },
        Spec { name: "v3-s16-8ch-small-blocks", version: 3, u8_type: false, channels: 8, blocksize: 3, maxnlpc: 3, nmean: 1, header: Header::Wave, blocks: 400, seed: 8 },
        Spec { name: "v2-s16-big-blocks", version: 2, u8_type: false, channels: 2, blocksize: 8192, maxnlpc: 0, nmean: 4, header: Header::Wave, blocks: 12, seed: 9 },
    ];
    for spec in &specs {
        let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("codec-shorten-{}.shn", spec.name));
        std::fs::write(&path, stream(spec)).expect("write");
        let n = check(&path);
        assert!(n > 0, "{}: no samples", spec.name);
        eprintln!("{}: {n} samples/channel, bit-exact", spec.name);
    }
}
