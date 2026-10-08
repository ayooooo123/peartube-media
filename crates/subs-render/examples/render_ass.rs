//! Render one ASS/SSA frame to a PPM. No fonts are bundled.
use std::io::Write;
use std::path::PathBuf;
use subs_render::{FontOptions, Renderer, Track};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if !(6..=7).contains(&args.len()) {
        return Err("usage: render_ass script.ass milliseconds width height output.ppm [font-directory]".into());
    }
    let (time, width, height) = (args[2].parse::<i64>()?, args[3].parse::<u32>()?, args[4].parse::<u32>()?);
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > 4096 * 4096 { return Err("invalid image size".into()); }
    let options = FontOptions { directories: args.get(6).map(|p| vec![PathBuf::from(p)]), default_family: None };
    let mut renderer = Renderer::new(&options);
    let mut track = Track::new();
    track.process_data(&std::fs::read(&args[1])?);
    let frame = renderer.render(&mut track, time, width, height);
    let image = frame.image;
    let mut rgb = vec![0; width as usize * height as usize * 3];
    for y in 0..image.height { for x in 0..image.width {
        let (dx,dy) = (image.x + x as i32,image.y + y as i32);
        if dx < 0 || dy < 0 || dx >= width as i32 || dy >= height as i32 { continue; }
        let src = &image.rgba[(y as usize * image.width as usize + x as usize) * 4..][..4];
        let dst = &mut rgb[(dy as usize * width as usize + dx as usize) * 3..][..3];
        for c in 0..3 { dst[c] = ((u16::from(src[c]) * u16::from(src[3]) + 127) / 255) as u8; }
    } }
    let mut file = std::fs::File::create(&args[5])?;
    write!(file,"P6\n{width} {height}\n255\n")?;
    file.write_all(&rgb)?;
    println!("{} events; image {},{} {}x{}; animated={}; next={:?}; {}",track.events.len(),image.x,image.y,image.width,image.height,frame.animated,frame.next_time,args[5]);
    Ok(())
}
