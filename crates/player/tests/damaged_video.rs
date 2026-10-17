//! A damaged video packet costs its own pictures, not the playback. FATE's
//! `flv/Enigma_Principles_of_Lust-part.flv` has an H.264 packet whose NAL
//! length runs past its end (FFmpeg: "Invalid NAL unit size (2771 > 1186)");
//! FFmpeg drops that packet and decodes on. The Player must too: video
//! keeps going after the bad packet, through to the end, with no error.

use std::{sync::Arc, time::Duration};

use player::{Event, Headless, Player, PlayerOptions};

#[test]
fn h264_packet_with_truncated_nal_is_dropped_not_fatal() {
    let path = refcheck::fate("flv/Enigma_Principles_of_Lust-part.flv");
    let expected = refcheck::ffmpeg_video_md5s(&path, 0, "yuv420p");
    let backend = Headless::new();
    let (tx, rx) = std::sync::mpsc::channel();
    let player = Player::open(path.to_str().unwrap(), backend.clone(), Arc::new(codecs::context()),
        PlayerOptions { realtime: false, ..PlayerOptions::default() },
        move |event| { let _ = tx.send(event); });
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
            Ok(Event::Ended) => break,
            Ok(Event::Error(error)) => panic!("playback failed: {error}"),
            Ok(Event::Changed) => {}
            Err(error) => panic!("player did not end: {error}: {:?}", player.state()),
        }
    }
    let state = player.state();
    drop(player);
    assert!(state.error.is_none(), "{:?}", state.error);
    assert!(state.video.is_some(), "video stream was retired");
    let capture = backend.capture();
    let shown = capture.video.first().map(|v| v.frame_md5.len()).unwrap_or(0);
    // FFmpeg loses the damaged packet's picture too: 350 either way.
    assert_eq!(shown, expected.len(), "pictures shown against FFmpeg's");
}
