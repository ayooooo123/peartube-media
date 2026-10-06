use oxideav_core::{Frame, ProbeData, RuntimeContext};
use std::io::Read;

fn main() {
    let path = std::env::args().nth(1).expect("path");
    let fmt = std::env::args().nth(2).expect("fmt");
    let mut data = Vec::new();
    std::fs::File::open(&path).unwrap().read_to_end(&mut data).unwrap();
    let mut ctx = RuntimeContext::new();
    codec_dca::register(&mut ctx);
    let ext = std::env::args().nth(3).unwrap_or_else(|| fmt.clone());
    let probe = ProbeData { buf: &data[..data.len().min(256 * 1024)], ext: Some(ext.as_str()) };
    let _ = ctx.containers.probe_candidates(&probe);
    let mut dem = ctx
        .containers
        .open_demuxer(&fmt, Box::new(std::io::Cursor::new(data)), &ctx.codecs)
        .expect("open");
    let stream = dem.streams()[0].clone();
    let mut dec = ctx.codecs.first_decoder(&stream.params).unwrap();
    let mut out: Vec<u8> = Vec::new();
    loop {
        match dem.next_packet() {
            Ok(p) => {
                let _ = dec.send_packet(&p);
                loop {
                    match dec.receive_frame() {
                        Ok(Frame::Audio(a)) => {
                            for plane in &a.data {
                                out.extend_from_slice(plane);
                            }
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
            }
            Err(_) => break,
        }
    }
    let dst = std::env::args().nth(4).expect("out");
    std::fs::write(dst, &out).unwrap();
    println!("bytes={}", out.len());
}
