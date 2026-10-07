//! Decode speed of the AAC fork as a multiple of real time.
//!
//! `cargo run --release -p check-aac --example speed [fate-relative paths…]`
//!
//! Each sample is demuxed into memory first (outside the timed region) by
//! the same OxideAV containers the reference tests use; the timed region is
//! decoder construction plus every `send_packet` / `receive_frame` and the
//! final flush, repeated until at least one second of wall time has passed.
//! Real time is the decoded duration: samples per channel over the
//! decoder's reported output rate.

use oxideav_core::{Error, Frame, MediaType, PROBE_SCORE_EXTENSION, Packet, ProbeData, RuntimeContext};
use std::fs::File;
use std::io::Read;
use std::time::{Duration, Instant};

// al04_44 is mono despite its frequent description as a stereo sample;
// al05_44 also measures the actual stereo LC path.
const DEFAULT_SAMPLES: &[&str] = &[
    "aac/al04_44.mp4",
    "aac/al05_44.mp4",
    "aac/al_sbr_ps_06_new.mp4",
    "aac/al07_96.mp4",
    "aac/ap05_48.mp4",
    "aac/er_eld2100np_48_ep0.mp4",
];

fn context() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    oxideav_aac::__oxideav_entry(&mut ctx);
    oxideav_mov::registry::register(&mut ctx);
    oxideav_mp4::__oxideav_entry(&mut ctx);
    oxideav_mpegts::__oxideav_entry(&mut ctx);
    ctx
}

fn demux(ctx: &RuntimeContext, rel: &str) -> (oxideav_core::CodecParameters, Vec<Packet>) {
    let path = refcheck::fate(rel);
    let mut head = vec![0; 256 * 1024];
    let n = File::open(&path).and_then(|mut f| f.read(&mut head)).expect("read sample");
    let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    let probe = ProbeData { buf: &head[..n], ext: ext.as_deref() };
    let candidates = ctx.containers.probe_candidates(&probe);
    let by_extension = ext.as_deref().and_then(|e| ctx.containers.container_for_extension(e));
    let format = match (candidates.first(), by_extension) {
        (Some(c), _) if c.score >= PROBE_SCORE_EXTENSION => c.name.to_string(),
        (_, Some(name)) => name.to_string(),
        _ => panic!("{rel}: no container claims it"),
    };
    let file = File::open(&path).expect("open sample");
    let mut demuxer = ctx.containers.open_demuxer(&format, Box::new(file), &ctx.codecs).expect("open demuxer");
    let stream = demuxer
        .streams()
        .iter()
        .find(|s| s.params.media_type == MediaType::Audio)
        .expect("audio stream")
        .clone();
    let mut packets = Vec::new();
    loop {
        match demuxer.next_packet() {
            Ok(p) if p.stream_index == stream.index => packets.push(p),
            Ok(_) => {}
            Err(Error::Eof) => break,
            Err(e) => panic!("{rel}: demux: {e}"),
        }
    }
    (stream.params, packets)
}

/// One full decode; returns (samples per channel, sample rate, channels).
fn decode_once(ctx: &RuntimeContext, params: &oxideav_core::CodecParameters, packets: &[Packet]) -> (u64, u32, u16) {
    let mut decoder = ctx.codecs.first_decoder(params).expect("decoder");
    let mut samples = 0u64;
    let mut drain = |decoder: &mut Box<dyn oxideav_core::Decoder>| loop {
        match decoder.receive_frame() {
            Ok(Frame::Audio(a)) => samples += u64::from(a.samples),
            Ok(_) => {}
            Err(Error::NeedMore) | Err(Error::Eof) => break,
            Err(e) => panic!("decode: {e}"),
        }
    };
    for p in packets {
        decoder.send_packet(p).expect("send_packet");
        drain(&mut decoder);
    }
    decoder.flush().expect("flush");
    drain(&mut decoder);
    let fmt = decoder.output_audio_format().expect("output_audio_format");
    (samples, fmt.sample_rate, fmt.channels)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let samples: Vec<&str> =
        if args.is_empty() { DEFAULT_SAMPLES.to_vec() } else { args.iter().map(String::as_str).collect() };
    let ctx = context();
    for rel in samples {
        let (params, packets) = demux(&ctx, rel);
        let mut runs = 0u32;
        let mut elapsed = Duration::ZERO;
        let mut last = (0, 0, 0);
        while elapsed < Duration::from_secs(1) {
            let t = Instant::now();
            last = std::hint::black_box(decode_once(&ctx, &params, &packets));
            elapsed += t.elapsed();
            runs += 1;
        }
        let (samples, rate, channels) = last;
        let audio = samples as f64 / f64::from(rate);
        let per_run = elapsed.as_secs_f64() / f64::from(runs);
        println!(
            "{rel}: {channels} ch @ {rate} Hz, {audio:.2} s audio, {:.2} ms/decode ({runs} runs), {:.1}x real time",
            per_run * 1e3,
            audio / per_run
        );
    }
}
