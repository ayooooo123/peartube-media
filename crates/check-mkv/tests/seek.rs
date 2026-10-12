//! Seeking to 30 s lands on the keyframe FFmpeg lands on (`ffprobe
//! -read_intervals 30%+#1`): through the Cues of FFmpeg's default Matroska
//! muxing, and by scanning the Clusters of the same streams written to a
//! pipe (no Cues).

use std::fs::File;
use std::path::Path;

use check_mkv::{Counting, Generated, Pkt, ReadLog, ffprobe_packets, generated};
use oxideav_core::MediaType;

fn seek_lands_like_ffprobe(which: Generated) {
    let path = generated(Path::new(env!("CARGO_TARGET_TMPDIR")), which);
    let want = ffprobe_packets(&path, &["-select_streams", "v", "-read_intervals", "30%+#1"]);
    let want = want.first().expect("ffprobe's packet after seeking to 30 s");
    assert!(want.keyframe, "ffprobe landed on {want:?}");

    let log = ReadLog::default();
    let file = File::open(&path).expect("open file");
    let mut dmx = check_mkv::open(Box::new(Counting::new(file, log.clone()))).expect("open demuxer");
    let video = dmx
        .streams()
        .iter()
        .find(|s| s.params.media_type == MediaType::Video)
        .expect("video stream")
        .clone();
    let tb = video.time_base.as_rational();
    let target = 30 * tb.den / tb.num;
    log.clear();
    let landed = dmx.seek_to(video.index, target).expect("seek");
    let first = loop {
        let p = dmx.next_packet().expect("packet after the seek");
        if p.stream_index == video.index {
            break Pkt::of(&p);
        }
    };
    println!(
        "{which:?}: seek_to(30 s) returned {landed}; first video packet pts {:?}, {} bytes \
         (ffprobe: pts {:?}, {} bytes); the seek and that packet read {} bytes",
        first.pts,
        first.size,
        want.pts,
        want.size,
        log.total()
    );
    assert_eq!(
        (first.pts, first.size, &first.md5),
        (want.pts, want.size, &want.md5),
        "first video packet after seeking to 30 s"
    );
}

#[test]
fn seek_with_cues_lands_on_ffprobes_keyframe() {
    seek_lands_like_ffprobe(Generated::CuesAtEnd);
}

#[test]
fn seek_without_cues_lands_on_ffprobes_keyframe() {
    seek_lands_like_ffprobe(Generated::NoCues);
}
