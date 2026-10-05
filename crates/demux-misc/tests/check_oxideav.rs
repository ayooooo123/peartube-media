use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use oxideav_core::ProbeData;

fn fate(rel: &str) -> Option<PathBuf> {
    let p = Path::new("/Users/jd/projects/fate-suite").join(rel);
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

fn check_sample(ctx: &oxideav_core::RuntimeContext, name: &str, rel_path: &str) {
    let Some(path) = fate(rel_path) else {
        println!("SAMPLE MISSING: {name} ({rel_path})");
        return;
    };
    let mut head = vec![0u8; 256 * 1024];
    let n = File::open(&path).and_then(|mut f| f.read(&mut head)).unwrap();
    let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    let probe = ProbeData { buf: &head[..n], ext: ext.as_deref() };
    let candidates = ctx.containers.probe_candidates(&probe);
    let by_ext = ext.as_deref().and_then(|e| ctx.containers.container_for_extension(e));

    println!("=== Format: {name} ({}) ===", path.display());
    println!("  Probe candidates: {:?}", candidates.iter().map(|c| (c.name, c.score)).collect::<Vec<_>>());
    println!("  By extension ({:?}): {:?}", ext, by_ext);

    // Try opening if candidate or by_ext found
    let format_to_try = if let Some(c) = candidates.first() {
        if c.score >= 25 {
            Some(c.name)
        } else {
            by_ext
        }
    } else {
        by_ext
    };

    if let Some(fmt) = format_to_try {
        let f = File::open(&path).unwrap();
        match ctx.containers.open_demuxer(fmt, Box::new(f), &ctx.codecs) {
            Ok(demuxer) => {
                let streams = demuxer.streams();
                println!("  SUCCESS: opened with '{fmt}', streams: {}", streams.len());
                for s in streams {
                    println!("    stream {}: {:?} codec={:?}", s.index, s.params.media_type, s.params.codec_id);
                }
            }
            Err(e) => {
                println!("  FAILED to open with '{fmt}': {e}");
            }
        }
    } else {
        println!("  NO container claimed it");
    }
}

#[test]
fn test_oxideav_capabilities() {
    let ctx = codecs::context();
    println!("Registered containers: {:?}", ctx.containers.demuxer_names().collect::<Vec<_>>());

    check_sample(&ctx, "ac3", "ac3/monsters_inc_2.0_192_small.ac3");
    check_sample(&ctx, "eac3", "eac3/csi_miami_5.1_256_spx_small.eac3");
    check_sample(&ctx, "aac", "aac/ct_faac-adts.aac");
    check_sample(&ctx, "mp3", "gapless/gapless.mp3");
    check_sample(&ctx, "flac", "cover_art/cover_art.flac");
    check_sample(&ctx, "mpegvideo", "mpeg2/sony-ct3.bs");
    check_sample(&ctx, "h264", "h264/intra_refresh.h264");
    check_sample(&ctx, "hevc", "hevc-conformance/WPP_A_ericsson_MAIN_2.bit");
    check_sample(&ctx, "mpegps_mpg", "mpegps/pcm_aud.mpg");
    check_sample(&ctx, "mpegps_vob", "mpeg2/dvd_single_frame.vob");
    check_sample(&ctx, "pva", "pva/PVA_test-partial.pva");
    check_sample(&ctx, "voc", "creative/BBC_2BIT.VOC");
    check_sample(&ctx, "caf", "caf/caf-pcm16.caf");
    check_sample(&ctx, "ivf", "av1/seq_hdr_op_param_info.ivf");
}
