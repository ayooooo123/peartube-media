//! Throwaway smoke: demux each FATE sample, print per-stream codec + packet stats.
use std::fs::File;

fn main() {
    let samples = [
        "real/ra_288.rm",
        "real/ra_cook.rm",
        "real/rv30.rm",
        "real/spygames-2MB.rmvb",
        "real/G2_with_SVT_320_240.rm",
        "sipr/sipr_5k0.rm",
        "sipr/sipr_6k5.rm",
        "sipr/sipr_8k5.rm",
        "sipr/sipr_16k.rm",
        "lossless-audio/luckynight-partial.rmvb",
        "real/ra3_in_rm_file.rm",
        "realaudio/ra3.ra",
    ];
    let root = std::env::var("HOME").unwrap();
    for s in samples {
        let path = format!("{root}/projects/fate-suite/{s}");
        let mut ctx = oxideav_core::RuntimeContext::new();
        demux_rm::register(&mut ctx);
        let probe = std::fs::read(&path).unwrap();
        let mut head = vec![0u8; 256 * 1024];
        let n = head.len().min(probe.len());
        head[..n].copy_from_slice(&probe[..n]);
        let ext = path.rsplit('.').next().map(str::to_string);
        let pd = oxideav_core::ProbeData { buf: &head[..n], ext: ext.as_deref() };
        let cands = ctx.containers.probe_candidates(&pd);
        let fmt = cands.first().map(|c| c.name.to_string());
        let file = File::open(&path).unwrap();
        match ctx.containers.open_demuxer(&fmt.unwrap(), Box::new(file), &ctx.codecs) {
            Ok(mut d) => {
                let streams: Vec<String> = d
                    .streams()
                    .iter()
                    .map(|s| {
                        format!(
                            "#{} {:?} {:?} sr={:?} ch={:?} {}x{}",
                            s.index,
                            s.params.media_type,
                            s.params.codec_id,
                            s.params.sample_rate,
                            s.params.channels,
                            s.params.width.unwrap_or(0),
                            s.params.height.unwrap_or(0)
                        )
                    })
                    .collect();
                let mut counts = std::collections::HashMap::new();
                let mut first_pts = std::collections::HashMap::new();
                let mut last_pts = std::collections::HashMap::new();
                loop {
                    match d.next_packet() {
                        Ok(p) => {
                            let c = counts.entry(p.stream_index).or_insert(0);
                            *c += 1;
                            if let Some(pts) = p.pts {
                                first_pts.entry(p.stream_index).or_insert(pts);
                                last_pts.insert(p.stream_index, pts);
                            }
                        }
                        Err(oxideav_core::Error::Eof) => break,
                        Err(e) => {
                            println!("{s}: demux error after {:?}: {e}", counts);
                            break;
                        }
                    }
                }
                println!("{s}: OK streams=[{streams:?}] packets={counts:?} first={first_pts:?} last={last_pts:?}");
            }
            Err(e) => println!("{s}: OPEN FAILED: {e}"),
        }
    }
}
