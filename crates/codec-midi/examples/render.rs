//! Run the real Player with a local SF2 and write interleaved f32le PCM.
//! cargo run -p codec-midi --example render -- song.mid bank.sf2 output.f32
use std::{io::Write, path::PathBuf, sync::Arc};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let input = args
        .next()
        .ok_or("usage: render song.mid bank.sf2 output.f32")?;
    let font = PathBuf::from(args.next().ok_or("missing SoundFont path")?);
    let output = args.next().ok_or("missing output path")?;
    let mut ctx = oxideav_core::RuntimeContext::new();
    demux_misc::register(&mut ctx);
    codec_midi::register(&mut ctx);
    let backend = player::Headless::new();
    let options = player::PlayerOptions {
        realtime: false,
        soundfont: Some(font),
        ..Default::default()
    };
    let p = player::Player::open(
        input.to_str().ok_or("input path is not UTF-8")?,
        backend.clone(),
        Arc::new(ctx),
        options,
        |_| {},
    );
    let state = p.wait();
    if let Some(error) = state.error {
        return Err(error.into());
    }
    if !state.ended {
        return Err("playback did not end".into());
    }
    drop(p);
    let capture = backend.capture();
    let audio = capture.audio.first().ok_or("no audio track")?;
    let mut file = std::io::BufWriter::new(std::fs::File::create(output)?);
    for sample in &audio.pcm {
        file.write_all(&sample.to_le_bytes())?;
    }
    file.flush()?;
    println!(
        "MIDI Player: {} frames, {} Hz, {} channels, ended without error",
        audio.pcm.len() / usize::from(audio.channels),
        audio.sample_rate,
        audio.channels
    );
    Ok(())
}
