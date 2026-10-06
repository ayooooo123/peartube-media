use oxideav_core::{ProbeData, RuntimeContext};
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
    let mut count = 0usize;
    let mut sizes: Vec<usize> = Vec::new();
    while let Ok(p) = dem.next_packet() {
        count += 1;
        if sizes.len() < 8 {
            sizes.push(p.data.len());
        }
    }
    println!("packets={count} first_sizes={sizes:?}");
}
