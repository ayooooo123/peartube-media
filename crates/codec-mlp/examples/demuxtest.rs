use oxideav_core::{MediaType, ProbeData, RuntimeContext};
use std::fs::File;
use std::io::Read;

fn main() {
    for path in [
        "/Users/jd/projects/fate-suite/truehd/atmos.thd",
        "/Users/jd/projects/fate-suite/lossless-audio/luckynight-partial.mlp",
    ] {
        let mut ctx = RuntimeContext::new();
        codec_mlp::register(&mut ctx);
        let mut head = vec![0u8; 256 * 1024];
        let n = File::open(path).and_then(|mut f| f.read(&mut head)).unwrap();
        let ext = std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        let probe = ProbeData { buf: &head[..n], ext: ext.as_deref() };
        let candidates = ctx.containers.probe_candidates(&probe);
        println!("{}: candidates {:?}", path.rsplit('/').next().unwrap(),
            candidates.iter().map(|c| (c.name, c.score)).collect::<Vec<_>>());
        // Probe rule: extension first when the content probe scored < 25.
        let name = match candidates.first() {
            Some(c) if c.score >= oxideav_core::PROBE_SCORE_EXTENSION => c.name,
            _ => {
                let ext = ext.as_deref().expect("no ext");
                ctx.containers.container_for_extension(ext).expect("no container for ext")
            }
        };
        println!("  opened as {}", name);
        let file = File::open(path).unwrap();
        let mut demuxer = ctx.containers.open_demuxer(name, Box::new(file), &ctx.codecs).unwrap();
        let stream = demuxer.streams()[0].clone();
        println!("  stream: codec={} rate={:?}", stream.params.codec_id, stream.params.sample_rate);
        let mut count = 0;
        let mut pts_first = None;
        let mut pts_last = 0i64;
        while let Ok(p) = demuxer.next_packet() {
            if pts_first.is_none() { pts_first = p.pts; }
            pts_last = p.pts.unwrap_or(0);
            count += 1;
        }
        println!("  packets: {} first pts {:?} last {}", count, pts_first, pts_last);
        let _ = MediaType::Audio;
    }
}
