// Throwaway smoke harness: decode the first TrueHD access unit with the
// ported decoder and compare with FFmpeg's output for the same packet.

use codec_mlp::crc;
use codec_mlp::decoder::MlpDecoder;
use oxideav_core::Decoder;
use oxideav_core::{CodecId, CodecParameters, Packet, TimeBase};

fn main() {
    let path = std::env::args().nth(1).expect("usage: smoke <file> [max-aus]");
    let max_aus: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(50);
    let dump_frames: bool = std::env::args().nth(3).map(|s| s == "dump").unwrap_or(false);

    let data = std::fs::read(&path).expect("read sample");
    println!("file {} ({} bytes)", path, data.len());

    // Find the first major sync to locate the AU start.
    let mut sync_off = None;
    let mut is_mlp = false;
    for off in 0..data.len().saturating_sub(8) {
        if data[off + 4..off + 8] == [0xf8, 0x72, 0x6f, 0xba] {
            sync_off = Some(off);
            break;
        }
        if data[off + 4..off + 8] == [0xf8, 0x72, 0x6f, 0xbb] {
            sync_off = Some(off);
            is_mlp = true;
            break;
        }
    }
    let first_sync = sync_off.expect("no mlp/truehd sync found");
    let mut sync_off = first_sync;
    println!("first major sync at offset {} (mlp={})", first_sync, is_mlp);

    let mut dec = MlpDecoder::new(
        &CodecParameters::audio(CodecId::new(if is_mlp { "mlp" } else { "truehd" })),
        is_mlp,
    );

    let mut total_samples = 0usize;
    let mut aus = 0usize;
    let mut first_frame: Option<Vec<u8>> = None;

    while sync_off + 4 <= data.len() && aus < max_aus {
        let length = ((u16::from_be_bytes([data[sync_off], data[sync_off + 1]]) & 0xfff) * 2)
            as usize;
        if length < 4 || sync_off + length > data.len() {
            println!("AU at {}: length {} out of range", sync_off, length);
            break;
        }
        let au = &data[sync_off..sync_off + length];
        let pkt = Packet::new(0, TimeBase::new(1, 48000), au.to_vec());
        match dec.send_packet(&pkt) {
            Ok(()) => {
                loop {
                    match dec.receive_frame() {
                        Ok(frame) => {
                            let oxideav_core::Frame::Audio(a) = frame else { continue };
                            if first_frame.is_none() {
                                first_frame = Some(a.data[0].clone());
                                println!(
                                    "first frame: {} samples/ch, {} bytes",
                                    a.samples,
                                    a.data[0].len()
                                );
                            }
                            if dump_frames {
                                println!("FRAME {:x}", md5::compute(&a.data[0]));
                                let name = format!("/tmp/frame_{}.bin", total_samples);
                                std::fs::write(&name, &a.data[0]).ok();
                            }
                            total_samples += a.samples as usize;
                        }
                        Err(oxideav_core::Error::NeedMore) | Err(oxideav_core::Error::Eof) => break,
                        Err(e) => {
                            println!("receive_frame error: {e}");
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                println!("send_packet error at AU {aus}: {e}");
                break;
            }
        }
        aus += 1;
        sync_off += length;
    }

    println!("decoded {aus} AUs, {total_samples} samples/ch");
    if let Some(f) = first_frame {
        println!("first frame md5: {:x}", md5::compute(&f));
        println!("first 32 bytes: {}", f[..32].iter().map(|b| format!("{b:02x}")).collect::<String>());
    }
    let _ = crc::checksum8(&[0]);

    // Full-stream md5 (interleaved s32le like FFmpeg's reference).
    let mut all = Vec::new();
    let mut sync_off = first_sync;
    let mut dec = MlpDecoder::new(
        &CodecParameters::audio(CodecId::new(if is_mlp { "mlp" } else { "truehd" })),
        is_mlp,
    );
    let mut aus2 = 0usize;
    while sync_off + 4 <= data.len() {
        let length = ((u16::from_be_bytes([data[sync_off], data[sync_off + 1]]) & 0xfff) * 2)
            as usize;
        if length < 4 || sync_off + length > data.len() {
            break;
        }
        let au = &data[sync_off..sync_off + length];
        let pkt = Packet::new(0, TimeBase::new(1, 48000), au.to_vec());
        if dec.send_packet(&pkt).is_ok() {
            while let Ok(frame) = dec.receive_frame() {
                let oxideav_core::Frame::Audio(a) = frame else { continue };
                all.extend_from_slice(&a.data[0]);
            }
        } else {
            break;
        }
        aus2 += 1;
        sync_off += length;
    }
    println!("stream md5: {:x} ({} bytes, {} AUs)", md5::compute(&all), all.len(), aus2);
}
