//! Untrusted input: truncated, bit-flipped and byte-smashed copies of the
//! FATE samples' GSM and Microsoft GSM packets, any `block_align` a WAVE
//! header can declare, and damaged raw `.gsm` files read to the end and
//! seeked, must never panic. Deterministic: fixed seeds, 2000 mutations
//! per source.

use oxideav_core::{CodecId, CodecParameters, Decoder, MediaType, Packet, RuntimeContext, TimeBase};
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

fn mutate(rng: &mut Rng, data: &[u8]) -> Vec<u8> {
    let mut d = data.to_vec();
    match rng.below(4) {
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
        _ => {
            let extra = rng.below(80);
            d.extend((0..extra).map(|_| rng.next() as u8));
        }
    }
    d
}

/// The first audio stream's parameters and packets of `sample`.
fn packets(ctx: &RuntimeContext, sample: &str) -> (CodecParameters, Vec<Packet>) {
    let path = fate(sample);
    let format = refcheck::probe_container(ctx, &path).expect("probe");
    let file = std::fs::File::open(&path).expect("open sample");
    let mut demuxer = ctx.containers.open_demuxer(&format, Box::new(file), &ctx.codecs).expect("open demuxer");
    let stream = demuxer.streams().iter().find(|s| s.params.media_type == MediaType::Audio).expect("audio").clone();
    let mut out = Vec::new();
    while let Ok(p) = demuxer.next_packet() {
        if p.stream_index == stream.index {
            out.push(p);
        }
        if out.len() >= 400 {
            break;
        }
    }
    (stream.params, out)
}

fn drain(decoder: &mut dyn Decoder) {
    while decoder.receive_frame().is_ok() {}
}

fn mutate_packets(params: &CodecParameters, packets: &[Packet], seed: u64) {
    let mut rng = Rng(seed);
    for _ in 0..2000 {
        let mut decoder = codec_gsm::GsmDecoder::new(params).expect("decoder");
        let start = rng.below(packets.len());
        for p in &packets[start..(start + 8).min(packets.len())] {
            let mut q = p.clone();
            q.data = mutate(&mut rng, &p.data);
            let _ = decoder.send_packet(&q);
            drain(&mut decoder);
        }
        let _ = decoder.flush();
        drain(&mut decoder);
    }
}

#[test]
fn damaged_ms_gsm_packets_never_panic() {
    let mut ctx = RuntimeContext::new();
    codec_gsm::register(&mut ctx);
    oxideav_basic::__oxideav_entry(&mut ctx);
    let (params, packets) = packets(&ctx, "gsm/ciao.wav");
    assert_eq!(params.codec_id, CodecId::new("gsm_ms"));
    mutate_packets(&params, &packets, 0x1234_5678);
}

#[test]
fn damaged_gsm_packets_never_panic() {
    let mut ctx = RuntimeContext::new();
    codec_gsm::register(&mut ctx);
    oxideav_mov::registry::register(&mut ctx);
    let (params, packets) = packets(&ctx, "gsm/sample-gsm-8000.mov");
    assert_eq!(params.codec_id, CodecId::new("gsm"));
    mutate_packets(&params, &packets, 0x8765_4321);
}

/// Every `block_align` from 0 to 300 and random larger ones: a decoder or
/// an error, and random packets through those it accepts.
#[test]
fn any_block_align_never_panics() {
    let mut rng = Rng(0xB10C);
    let mut accepted = 0;
    for i in 0..2000u64 {
        let block_align = if i <= 300 { i } else { rng.next() };
        let mut params = CodecParameters::audio(CodecId::new("gsm_ms"));
        params.options.insert("block_align", block_align.to_string());
        let Ok(mut decoder) = codec_gsm::GsmDecoder::new(&params) else { continue };
        accepted += 1;
        let len = rng.below(300);
        let data: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        let _ = decoder.send_packet(&Packet::new(0, TimeBase::new(1, 8000), data));
        drain(&mut decoder);
    }
    // 0 (the default) and the eight sizes from 41 to 62 and 65.
    assert_eq!(accepted, 10);
}

/// Damaged raw `.gsm` files: open, read to the end, seek, read again.
#[test]
fn damaged_raw_files_never_panic() {
    let path = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("codec-gsm-robustness.gsm");
    let out = std::process::Command::new(refcheck::pinned_ffmpeg())
        .args(["-v", "error", "-nostdin", "-y", "-i"])
        .arg(fate("gsm/sample-gsm-8000.mov"))
        .args(["-map", "0:a:0", "-t", "20", "-c:a", "copy", "-f", "gsm"])
        .arg(&path)
        .output()
        .expect("pinned ffmpeg runs");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let file = std::fs::read(&path).expect("read fixture");
    let mut ctx = RuntimeContext::new();
    codec_gsm::register(&mut ctx);
    let mut rng = Rng(0xF11E);
    for _ in 0..2000 {
        let data = mutate(&mut rng, &file);
        let Ok(mut demuxer) =
            ctx.containers.open_demuxer("gsm", Box::new(std::io::Cursor::new(data)), &ctx.codecs)
        else {
            continue;
        };
        let params = demuxer.streams()[0].params.clone();
        let mut decoder = codec_gsm::GsmDecoder::new(&params).expect("decoder");
        while let Ok(p) = demuxer.next_packet() {
            let _ = decoder.send_packet(&p);
            drain(&mut decoder);
        }
        let target = rng.next() as i64;
        let _ = demuxer.seek_to(0, target);
        while let Ok(p) = demuxer.next_packet() {
            let _ = decoder.send_packet(&p);
            drain(&mut decoder);
        }
    }
}
