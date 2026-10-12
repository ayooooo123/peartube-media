//! Untrusted input: truncated, bit-flipped and byte-smashed Shorten
//! streams, cut into packets of random sizes and drained at the end, and
//! the demuxer's probe and open on them, must never panic. Deterministic:
//! fixed seeds, 2000 mutations per source.

use oxideav_core::{CodecId, CodecParameters, Decoder, Packet, ProbeData, RuntimeContext, SampleFormat, TimeBase};
use refcheck::fate;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }
}

fn mutate(rng: &mut Rng, data: &[u8], hot: usize) -> Vec<u8> {
    let mut d = data.to_vec();
    // Half the damage lands in the first `hot` bytes (the header and first
    // blocks), the rest anywhere.
    let span = if rng.below(2) == 0 { hot.min(d.len()) } else { d.len() };
    match rng.below(4) {
        0 => d.truncate(rng.below(d.len() + 1)),
        1 => {
            for _ in 0..1 + rng.below(8) {
                if span > 0 {
                    let i = rng.below(span);
                    d[i] ^= 1 << rng.below(8);
                }
            }
        }
        2 => {
            for _ in 0..1 + rng.below(16) {
                if span > 0 {
                    let i = rng.below(span);
                    d[i] = rng.next() as u8;
                }
            }
        }
        _ => {
            // Zero runs: long Rice prefixes.
            if span > 0 {
                let at = rng.below(span);
                let end = (at + 1 + rng.below(64)).min(d.len());
                d[at..end].fill(0);
            }
        }
    }
    d
}

fn params() -> CodecParameters {
    let mut p = CodecParameters::audio(CodecId::new("shorten"));
    p.channels = Some(2);
    p.sample_rate = Some(44100);
    p.sample_format = Some(SampleFormat::S16P);
    p
}

/// Decodes `data` in packets of random sizes, then drains; at most `limit`
/// bytes, so each mutant stays quick.
fn run(rng: &mut Rng, data: &[u8], limit: usize) {
    let mut decoder = codec_shorten::ShortenDecoder::new(&params()).expect("decoder");
    let data = &data[..data.len().min(limit)];
    let mut at = 0;
    while at < data.len() {
        let size = 1 + rng.below(4096);
        let end = (at + size).min(data.len());
        let _ = decoder.send_packet(&Packet::new(0, TimeBase::new(1, 44100), data[at..end].to_vec()));
        while decoder.receive_frame().is_ok() {}
        at = end;
    }
    let _ = decoder.flush();
    while decoder.receive_frame().is_ok() {}
    let _ = decoder.output_audio_format();
}

#[test]
fn damaged_streams_never_panic() {
    let file = std::fs::read(fate("lossless-audio/luckynight-partial.shn")).expect("read");
    let mut rng = Rng(0x5A0E);
    for _ in 0..2000 {
        let data = mutate(&mut rng, &file, 4096);
        run(&mut rng, &data, 48 * 1024);
    }
}

/// Damaged headers: random stream parameters (version, type, channels,
/// block size, LPC order, means) and damaged WAVE/AIFF headers, then the
/// probe, the demuxer and the decoder.
#[test]
fn damaged_headers_never_panic() {
    let file = std::fs::read(fate("lossless-audio/luckynight-partial.shn")).expect("read");
    let mut ctx = RuntimeContext::new();
    codec_shorten::register(&mut ctx);
    let mut rng = Rng(0x4EAD);
    for _ in 0..2000 {
        let mut data = file[..file.len().min(32 * 1024)].to_vec();
        for _ in 0..1 + rng.below(6) {
            let i = 4 + rng.below(120);
            data[i] = rng.next() as u8;
        }
        let _ = ctx.containers.probe_candidates(&ProbeData { buf: &data, ext: Some("shn") });
        let _ = codec_shorten::parse_stream_header(&data);
        if let Ok(mut demuxer) = ctx.containers.open_demuxer("shn", Box::new(std::io::Cursor::new(data.clone())), &ctx.codecs) {
            let params = demuxer.streams()[0].params.clone();
            if let Ok(mut decoder) = codec_shorten::ShortenDecoder::new(&params) {
                while let Ok(p) = demuxer.next_packet() {
                    let _ = decoder.send_packet(&p);
                    while decoder.receive_frame().is_ok() {}
                }
                let _ = decoder.flush();
                while decoder.receive_frame().is_ok() {}
            }
            let _ = demuxer.seek_to(0, rng.next() as i64);
        }
        run(&mut rng, &data, 32 * 1024);
    }
}
