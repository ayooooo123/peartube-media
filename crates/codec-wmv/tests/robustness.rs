//! Untrusted-input robustness: every decoder and demuxer of the crate must
//! survive truncated, bit-flipped and overwritten copies of the reference
//! samples' packets (and files) without panicking or hanging.
//! Deterministic: fixed seeds, at least `MUTATIONS` mutations per target.

mod common;

use std::fs::File;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use common::encoded_sample;
use oxideav_core::{CodecParameters, MediaType, Packet, RuntimeContext};
use refcheck::{Registrar, fate};

/// xorshift64*: a fixed seed gives the same mutations on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A value in `0..n` (0 when `n` is 0).
    fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }
}

const MUTATIONS: usize = 2000;

/// No single packet or file may take this long to process: that is a hang.
const HANG_LIMIT: Duration = Duration::from_secs(30);

/// Truncates, bit-flips, overwrites, or truncates and flips `data`.
fn mutate(rng: &mut Rng, data: &mut Vec<u8>) {
    match rng.below(4) {
        0 => {
            let len = rng.below(data.len() + 1);
            data.truncate(len);
        }
        1 => {
            for _ in 0..1 + rng.below(8) {
                if !data.is_empty() {
                    let i = rng.below(data.len());
                    data[i] ^= 1 << rng.below(8);
                }
            }
        }
        2 => {
            for _ in 0..1 + rng.below(16) {
                if !data.is_empty() {
                    let i = rng.below(data.len());
                    data[i] = rng.next() as u8;
                }
            }
        }
        _ => {
            let len = rng.below(data.len() + 1);
            data.truncate(len);
            if !data.is_empty() {
                let i = rng.below(data.len());
                data[i] ^= 1 << rng.below(8);
            }
        }
    }
}

/// Runs `work` on its own thread. `work` reports progress through its
/// argument; no report for `HANG_LIMIT` fails the test as a hang, and a
/// panic on the worker fails it with the worker's message.
fn guarded(name: &'static str, work: impl FnOnce(&dyn Fn(usize)) + Send + 'static) {
    let (tx, rx) = mpsc::channel::<usize>();
    let worker = std::thread::spawn(move || {
        work(&|step| {
            let _ = tx.send(step);
        })
    });
    let mut last = 0;
    loop {
        match rx.recv_timeout(HANG_LIMIT) {
            Ok(step) => last = step,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => panic!("{name}: no progress for {HANG_LIMIT:?} after step {last}"),
        }
    }
    if let Err(panic) = worker.join() {
        std::panic::resume_unwind(panic);
    }
}

/// The first video stream of `path` (opened as `container`) and its packets.
fn video_packets(path: &Path, container: &str, registrars: &[Registrar]) -> (CodecParameters, Vec<Packet>) {
    let mut ctx = RuntimeContext::new();
    for register in registrars {
        register(&mut ctx);
    }
    let file = File::open(path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    let mut demuxer = ctx
        .containers
        .open_demuxer(container, Box::new(file), &ctx.codecs)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let stream = demuxer
        .streams()
        .iter()
        .find(|s| s.params.media_type == MediaType::Video)
        .unwrap_or_else(|| panic!("{}: no video stream", path.display()))
        .clone();
    let mut packets = Vec::new();
    while let Ok(packet) = demuxer.next_packet() {
        if packet.stream_index == stream.index {
            packets.push(packet);
        }
    }
    assert!(!packets.is_empty(), "{}: no video packets", path.display());
    (stream.params, packets)
}

fn wmv_decoder(params: &CodecParameters) -> oxideav_core::Result<Box<dyn oxideav_core::Decoder>> {
    let mut ctx = RuntimeContext::new();
    codec_wmv::register(&mut ctx);
    ctx.codecs.first_decoder(params)
}

/// Feeds the stream's packets in order, cycling, to one decoder and mutates
/// about half of them until `MUTATIONS` mutated packets went through, so
/// pictures predict from damaged references. Now and then the decoder is
/// flushed and decoding restarts from wherever the stream is.
fn fuzz_packets(name: &'static str, params: CodecParameters, packets: Vec<Packet>, seed: u64) {
    guarded(name, move |tick| {
        let mut decoder = wmv_decoder(&params).unwrap_or_else(|e| panic!("{name}: no decoder: {e}"));
        let mut rng = Rng(seed);
        let mut mutated = 0;
        let mut fed = 0;
        while mutated < MUTATIONS {
            let mut packet = packets[fed % packets.len()].clone();
            fed += 1;
            if rng.below(2) == 0 {
                mutate(&mut rng, &mut packet.data);
                mutated += 1;
            }
            let _ = decoder.send_packet(&packet);
            while decoder.receive_frame().is_ok() {}
            if rng.below(97) == 0 {
                let _ = decoder.flush();
                while decoder.receive_frame().is_ok() {}
            }
            tick(fed);
        }
    });
}

/// Opens `MUTATIONS` decoders on mutated extradata (and, for a quarter of
/// them, random frame sizes) and decodes the stream's first packets.
fn fuzz_params(name: &'static str, params: CodecParameters, packets: Vec<Packet>, seed: u64) {
    guarded(name, move |tick| {
        let mut rng = Rng(seed);
        for step in 0..MUTATIONS {
            let mut p = params.clone();
            if p.extradata.is_empty() || rng.below(2) == 0 {
                p.extradata = (0..rng.below(8)).map(|_| rng.next() as u8).collect();
            } else {
                mutate(&mut rng, &mut p.extradata);
            }
            if rng.below(4) == 0 {
                p.width = Some(rng.below(800) as u32);
                p.height = Some(rng.below(600) as u32);
            }
            if let Ok(mut decoder) = wmv_decoder(&p) {
                for packet in packets.iter().take(3) {
                    let _ = decoder.send_packet(packet);
                    while decoder.receive_frame().is_ok() {}
                }
            }
            tick(step);
        }
    });
}

/// Demuxes `MUTATIONS` mutated copies of a file with `container`.
fn fuzz_container(name: &'static str, sample: &str, container: &'static str, seed: u64) {
    let original = std::fs::read(fate(sample)).unwrap_or_else(|e| panic!("read {sample}: {e}"));
    guarded(name, move |tick| {
        let mut ctx = RuntimeContext::new();
        codec_wmv::register(&mut ctx);
        let mut rng = Rng(seed);
        for step in 0..MUTATIONS {
            let mut data = original.clone();
            mutate(&mut rng, &mut data);
            if let Ok(mut demuxer) =
                ctx.containers.open_demuxer(container, Box::new(std::io::Cursor::new(data)), &ctx.codecs)
            {
                while demuxer.next_packet().is_ok() {}
            }
            tick(step);
        }
    });
}

#[test]
fn wmv2_packets_never_panic() {
    let (params, packets) =
        video_packets(&fate("wmv8/wmv8_x8intra.wmv"), "asf", &[codec_wmv::register, demux_asf::register]);
    fuzz_packets("wmv2 packets", params, packets, 0x5747_0002_0000_0001);
}

#[test]
fn wmv2_extradata_never_panics() {
    let (params, packets) =
        video_packets(&fate("wmv8/wmv8_x8intra.wmv"), "asf", &[codec_wmv::register, demux_asf::register]);
    fuzz_params("wmv2 extradata", params, packets, 0x5747_0002_0000_0002);
}

#[test]
fn msmpeg4v1_packets_never_panic() {
    let (params, packets) = video_packets(
        &fate("msmpeg4v1/mpg4.avi"),
        "avi",
        &[codec_wmv::register, oxideav_avi::__oxideav_entry],
    );
    fuzz_packets("msmpeg4v1 packets", params, packets, 0x4D50_0001_0000_0001);
}

#[test]
fn msmpeg4v1_params_never_panic() {
    let (params, packets) = video_packets(
        &fate("msmpeg4v1/mpg4.avi"),
        "avi",
        &[codec_wmv::register, oxideav_avi::__oxideav_entry],
    );
    fuzz_params("msmpeg4v1 params", params, packets, 0x4D50_0001_0000_0002);
}

#[test]
fn msmpeg4v3_packets_never_panic() {
    let (params, packets) =
        video_packets(&fate("asf/bug821-2.asf"), "asf", &[codec_wmv::register, demux_asf::register]);
    fuzz_packets("msmpeg4v3 packets", params, packets, 0x4D50_0003_0000_0001);
}

#[test]
fn msmpeg4v2_packets_never_panic() {
    let path = encoded_sample("fuzz_msmpeg4v2", "176x144", &["-c:v", "msmpeg4v2", "-qscale:v", "10"]);
    let (params, packets) = video_packets(&path, "avi", &[codec_wmv::register, oxideav_avi::__oxideav_entry]);
    fuzz_packets("msmpeg4v2 packets", params, packets, 0x4D50_0002_0000_0001);
}

#[test]
fn wmv1_packets_never_panic() {
    let path = encoded_sample("fuzz_wmv1", "176x144", &["-c:v", "wmv1", "-b:v", "100k"]);
    let (params, packets) = video_packets(&path, "avi", &[codec_wmv::register, oxideav_avi::__oxideav_entry]);
    fuzz_packets("wmv1 packets", params, packets, 0x5747_0001_0000_0001);
}

#[test]
fn wmv3_packets_never_panic() {
    let (params, packets) = video_packets(&fate("vc1/SMM0005.rcv"), "vc1test", &[codec_wmv::register]);
    fuzz_packets("wmv3 packets", params, packets, 0x5747_0003_0000_0001);
}

/// The WMV3 sequence header travels as extradata.
#[test]
fn wmv3_extradata_never_panics() {
    let (params, packets) = video_packets(&fate("vc1/SMM0005.rcv"), "vc1test", &[codec_wmv::register]);
    fuzz_params("wmv3 extradata", params, packets, 0x5747_0003_0000_0002);
}

/// Advanced profile, progressive; the sequence header and entry point are
/// in-band, in the first packet.
#[test]
fn vc1_progressive_packets_never_panic() {
    let (params, packets) = video_packets(&fate("vc1/SA00040.vc1"), "vc1", &[codec_wmv::register]);
    fuzz_packets("vc1 progressive packets", params, packets, 0x5643_0001_0000_0001);
}

#[test]
fn vc1_slice_packets_never_panic() {
    let (params, packets) = video_packets(&fate("vc1/SA10091.vc1"), "vc1", &[codec_wmv::register]);
    fuzz_packets("vc1 slice packets", params, packets, 0x5643_0001_0000_0002);
}

#[test]
fn vc1_field_packets_never_panic() {
    let (params, packets) = video_packets(&fate("vc1/SA10143.vc1"), "vc1", &[codec_wmv::register]);
    fuzz_packets("vc1 field packets", params, packets, 0x5643_0001_0000_0003);
}

#[test]
fn vc1_interlaced_frame_packets_never_panic() {
    let (params, packets) = video_packets(&fate("vc1/ilaced_twomv.vc1"), "vc1", &[codec_wmv::register]);
    fuzz_packets("vc1 interlaced frame packets", params, packets, 0x5643_0001_0000_0004);
}

/// VC-1 in MP4: OxideAV's mov demuxer hands over the `dvc1` box, whose
/// sequence header and entry point the decoder parses at open.
#[test]
fn vc1_extradata_never_panics() {
    let (params, packets) =
        video_packets(&fate("isom/vc1-wmapro.ism"), "mov", &[codec_wmv::register, oxideav_mov::registry::register]);
    assert!(!params.extradata.is_empty(), "vc1-wmapro.ism: no dvc1 extradata");
    fuzz_params("vc1 extradata", params, packets, 0x5643_0001_0000_0005);
}

#[test]
fn vc1test_container_never_panics() {
    fuzz_container("vc1test container", "vc1/SMM0005.rcv", "vc1test", 0x1234_5678_9ABC_DEF0);
}

#[test]
fn vc1_container_never_panics() {
    fuzz_container("vc1 container", "vc1/SA00040.vc1", "vc1", 0xCAFE_BABE_DEAD_BEEF);
}
