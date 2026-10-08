//! Source-level oracle companion. The regular render example exercises registration.
use std::io::{BufWriter, Write};
use codec_tracker::{READ_FRAMES, Song};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let input = args.next().ok_or("usage: render_seeded MODULE OUTPUT.f32 SEED [FRAME]")?;
    let output = args.next().ok_or("missing output path")?;
    let seed = args.next().ok_or("missing seed")?;
    let seed: u32 = seed.to_str().ok_or("seed is not UTF-8")?.parse()?;
    let frame = args.next().map(|v| v.to_string_lossy().parse::<u64>()).transpose()?.unwrap_or(0);
    let song = Song::load(&std::fs::read(input)?).ok_or("unsupported module")?.with_seed(seed);
    let mut renderer = song.renderer_at(frame);
    let mut output = BufWriter::new(std::fs::File::create(output)?);
    let mut pcm = [0.0f32; READ_FRAMES * 2];
    let mut bytes = [0u8; READ_FRAMES * 8];
    let mut frames = 0u64;
    loop {
        let n = renderer.read(&mut pcm);
        if n == 0 { break; }
        for (sample, dest) in pcm[..n * 2].iter().zip(bytes.chunks_exact_mut(4)) {
            dest.copy_from_slice(&sample.to_le_bytes());
        }
        output.write_all(&bytes[..n * 8])?;
        frames += n as u64;
    }
    output.flush()?;
    eprintln!("seed={seed} frames={frames} format={}", song.format().name());
    Ok(())
}
