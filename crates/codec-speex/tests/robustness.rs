//! Untrusted-input robustness: truncated, bit-flipped, byte-smashed and
//! reordered copies of each sample's Speex packets, plus corrupted
//! headers, sample rates, channel counts and the ZygoAudio tag, must never
//! panic the decoder. Deterministic: fixed seeds, 2000 mutations per
//! sample.

use oxideav_core::{CodecParameters, CodecTag, Decoder, MediaType, Packet, RuntimeContext};
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

fn context() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    codec_speex::register(&mut ctx);
    oxideav_ogg::register(&mut ctx);
    oxideav_avi::__oxideav_entry(&mut ctx);
    ctx
}

/// The parameters and packets of the first audio stream of `sample`.
fn packets(ctx: &RuntimeContext, sample: &str, container: &str) -> (CodecParameters, Vec<Packet>) {
    let path = fate(sample);
    let file = std::fs::File::open(&path).expect("open sample");
    let mut demuxer = ctx.containers.open_demuxer(container, Box::new(file), &ctx.codecs).expect("open demuxer");
    let stream = demuxer.streams().iter().find(|s| s.params.media_type == MediaType::Audio).expect("audio stream").clone();
    let mut out = Vec::new();
    while let Ok(p) = demuxer.next_packet() {
        if p.stream_index == stream.index {
            out.push(p);
        }
    }
    assert!(!out.is_empty(), "{sample}: no audio packets");
    (stream.params, out)
}

fn mutate(rng: &mut Rng, data: &[u8]) -> Vec<u8> {
    let mut d = data.to_vec();
    match rng.below(7) {
        0 => d.truncate(rng.below(d.len() + 1)),
        1 => {
            for _ in 0..1 + rng.below(8) {
                if !d.is_empty() {
                    let i = rng.below(d.len());
                    d[i] ^= 1 << rng.below(8);
                }
            }
        }
        2 => {
            for _ in 0..1 + rng.below(16) {
                if !d.is_empty() {
                    let i = rng.below(d.len());
                    d[i] = rng.next() as u8;
                }
            }
        }
        3 => {
            // Mode, submode and in-band request bits at the start.
            let n = d.len().min(4);
            for _ in 0..1 + rng.below(3) {
                if n > 0 {
                    let i = rng.below(n);
                    d[i] = rng.next() as u8;
                }
            }
        }
        4 => {
            let n = rng.below(80);
            d = (0..n).map(|_| rng.next() as u8).collect();
        }
        5 => {
            // A run of the same byte (0xff gives terminators and wideband
            // bits, 0x00 null modes).
            let fill = if rng.below(2) == 0 { 0xff } else { 0x00 };
            let start = rng.below(d.len() + 1);
            let end = (start + 1 + rng.below(16)).min(d.len());
            d[start..end].fill(fill);
        }
        _ => {
            if d.len() > 2 {
                let cut = rng.below(d.len());
                d.truncate(cut.max(1));
                d.extend((0..rng.below(32)).map(|_| rng.next() as u8));
            }
        }
    }
    d
}

fn drain(dec: &mut Box<dyn Decoder>) {
    while dec.receive_frame().is_ok() {}
}

/// Parameters a damaged or unusual stream could arrive with.
fn corrupt_params(rng: &mut Rng, params: &CodecParameters) -> CodecParameters {
    let mut p = params.clone();
    match rng.below(4) {
        0 => p.extradata = mutate(rng, &p.extradata),
        1 => {
            // No header: the mode comes from the rate.
            p.extradata.clear();
            p.sample_rate = Some([8000, 16000, 32000, 44100, 1, 0][rng.below(6)]);
            p.channels = Some(rng.below(4) as u16);
        }
        2 => {
            // ZygoAudio: a quality byte at offset 37.
            p.tag = Some(CodecTag::fourcc(b"SPXN"));
            p.extradata = (0..47 + rng.below(40)).map(|_| rng.below(12) as u8).collect();
        }
        _ => {
            // Header fields: rate, mode, channels, frame size, frames per
            // packet.
            if let Some(start) = p.extradata.windows(8).position(|w| w == b"Speex   ") {
                let field = start + 28 + 4 * [2, 3, 5, 7, 9][rng.below(5)];
                if field + 4 <= p.extradata.len() {
                    let v = [0u32, 1, 2, 3, 63, 64, 65, 160, 320, 640, 1280, u32::MAX][rng.below(12)];
                    p.extradata[field..field + 4].copy_from_slice(&v.to_le_bytes());
                }
            }
        }
    }
    p
}

fn fuzz(sample: &str, container: &str, seed: u64) {
    let ctx = context();
    let (params, pkts) = packets(&ctx, sample, container);
    let mut rng = Rng(seed);
    let mut dec: Option<Box<dyn Decoder>> = None;
    // CODEC_SPEEX_FUZZ_ITERS raises the mutation count for longer local runs.
    let iters = std::env::var("CODEC_SPEEX_FUZZ_ITERS").ok().and_then(|v| v.parse().ok()).unwrap_or(2000usize).max(2000);
    for i in 0..iters {
        if dec.is_none() || i % 97 == 0 {
            let p = if rng.below(3) == 0 { corrupt_params(&mut rng, &params) } else { params.clone() };
            dec = ctx.codecs.first_decoder(&p).ok();
            if dec.is_none() {
                dec = Some(ctx.codecs.first_decoder(&params).expect("decoder"));
            }
        }
        let d = dec.as_mut().unwrap();
        // A short run of packets from a random position, one or more
        // mutated, sometimes out of order.
        let start = rng.below(pkts.len());
        let run = 1 + rng.below(4);
        for k in 0..run {
            let idx = if rng.below(10) == 0 { rng.below(pkts.len()) } else { (start + k) % pkts.len() };
            let mut pkt = pkts[idx].clone();
            if k == 0 || rng.below(3) == 0 {
                pkt.data = mutate(&mut rng, &pkt.data);
            }
            let _ = d.send_packet(&pkt);
            drain(d);
        }
        if rng.below(20) == 0 {
            let _ = d.flush();
            drain(d);
        }
        if rng.below(50) == 0 {
            let _ = d.reset();
        }
    }
}

#[test]
fn narrowband_mutations_do_not_panic() {
    fuzz("speex/nb_q7.spx", "ogg", 0x5350_4558_0000_0001);
}

#[test]
fn stereo_mutations_do_not_panic() {
    fuzz("speex/stereo_q4.spx", "ogg", 0x5350_4558_0000_0002);
}

#[test]
fn wideband_mutations_do_not_panic() {
    fuzz("speex/wb_q8.spx", "ogg", 0x5350_4558_0000_0003);
}

#[test]
fn ultra_wideband_mutations_do_not_panic() {
    fuzz("speex/uwb_q4.spx", "ogg", 0x5350_4558_0000_0004);
}

#[test]
fn avi_mutations_do_not_panic() {
    fuzz("vp5/potter512-400-partial.avi", "avi", 0x5350_4558_0000_0005);
}
