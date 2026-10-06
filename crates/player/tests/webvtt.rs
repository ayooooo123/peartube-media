//! The WebM subtitle track must resolve through the player's registry, retain
//! FFmpeg's text and cue timing, and reach the actual subtitle sink.
use std::{path::PathBuf, process::Command, sync::Arc, time::Duration};
use oxideav_core::{Frame, MediaType};
use player::{Event, Headless, Player, PlayerOptions};

#[test]
fn generated_webvtt_matches_ffmpeg_and_reaches_player_sink() {
    let root = std::env::var_os("PEARTUBE_CORPUS_DIR").map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap()).join("projects/peartube-media-corpus"));
    let path = root.join("vp9_opus_vtt.webm");
    let output = Command::new("ffmpeg").args(["-v", "error", "-nostdin", "-i"])
        .arg(&path).args(["-map", "0:s:0", "-c:s", "webvtt", "-f", "webvtt", "-"])
        .output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let reference = oxideav_subtitle::webvtt::parse(&output.stdout).unwrap();
    assert_eq!(reference.cues.len(), 3);
    let decoded = refcheck::decode(&path, &[codecs::register_all], MediaType::Subtitle, 0);
    assert_eq!(decoded.frames.len(), reference.cues.len());
    for (frame, expected) in decoded.frames.iter().zip(&reference.cues) {
        let Frame::Subtitle(cue) = frame else { panic!("non-subtitle frame") };
        assert_eq!((cue.start_us, cue.end_us), (expected.start_us, expected.end_us));
        assert_eq!(oxideav_subtitle::srt::render_segments(&cue.segments), oxideav_subtitle::srt::render_segments(&expected.segments));
    }
    let backend = Headless::new();
    let (tx, rx) = std::sync::mpsc::channel();
    let player = Player::open(path.to_str().unwrap(), backend.clone(), Arc::new(codecs::context()),
        PlayerOptions { realtime: false, subtitle: Some(2), ..PlayerOptions::default() },
        move |event| { let _ = tx.send(event); });
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
            Ok(Event::Ended) => break,
            Ok(Event::Error(error)) => panic!("{error}"),
            Ok(Event::Changed) => {},
            Err(error) => panic!("player failed to end: {error}: {:?}", player.state()),
        }
    }
    assert!(player.state().error.is_none(), "{:?}", player.state());
    drop(player);
    let capture = backend.capture();
    let subtitles = capture.subtitles.iter().find(|s| s.stream == 2).expect("subtitle sink opened");
    assert_eq!(subtitles.codec, "webvtt");
    assert_eq!(subtitles.shows.iter().filter(|(_, images)| *images > 0).count(), reference.cues.len());
}
