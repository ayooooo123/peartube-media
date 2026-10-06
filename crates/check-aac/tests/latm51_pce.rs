use oxideav_core::{RuntimeContext, Packet, TimeBase};
use std::fs::File;

#[test]
fn latm51_pce_dump() {
    let path = refcheck::fate("aac/latm_stereo_to_51.ts");
    let mut ctx = RuntimeContext::new();
    oxideav_aac::__oxideav_entry(&mut ctx);
    oxideav_mpegts::__oxideav_entry(&mut ctx);
    let mut demuxer = ctx.containers.open_demuxer("mpegts", Box::new(File::open(&path).unwrap()), &ctx.codecs).unwrap();
    let stream = demuxer.streams()[0].clone();
    let mut dec = ctx.codecs.first_decoder(&stream.params).unwrap();
    let mut n = 0usize;
    loop {
        match demuxer.next_packet() {
            Ok(pkt) => {
                if pkt.stream_index != 0 { continue; }
                let mut pkt = Packet::new(pkt.stream_index, TimeBase::new(1, 48000), pkt.data);
                if dec.send_packet(&pkt).is_err() { break; }
                while let Ok(oxideav_core::Frame::Audio(a)) = dec.receive_frame() {
                    let ch = a.data[0].len() / 4 / a.samples.max(1) as usize;
                    if ch == 6 && n == 124 {
                        let d = &a.data[0];
                        // per-channel RMS over the frame
                        let ch_n = a.samples as usize;
                        for c in 0..6 {
                            let mut acc = 0.0f64;
                            for i in 0..ch_n {
                                let v = f32::from_le_bytes(d[(i * 6 + c) * 4..(i * 6 + c) * 4 + 4].try_into().unwrap());
                                acc += (v * v) as f64;
                            }
                            println!("frame {n} ch{c}: rms={:.5}", (acc / ch_n as f64).sqrt());
                        }
                    }
                    n += 1;
                }
            }
            Err(_) => break,
        }
    }
}
