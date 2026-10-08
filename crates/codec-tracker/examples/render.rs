//! Render a module through the registered demuxer and decoder to raw f32le.
use std::io::{BufWriter, Write};
use oxideav_core::{CodecRegistry, ContainerRegistry, Error, Frame};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let input = args.next().ok_or("usage: render MODULE OUTPUT.f32 [seek-frame]")?;
    let output = args.next().ok_or("missing output path")?;
    let seek = args.next().map(|v| v.to_string_lossy().parse::<i64>()).transpose()?;
    let bytes = std::fs::read(&input)?;
    let format = codec_tracker::probe(&bytes).ok_or("unrecognized module")?;
    let mut codecs = CodecRegistry::new();
    let mut containers = ContainerRegistry::new();
    codec_tracker::register_codecs(&mut codecs);
    codec_tracker::register_containers(&mut containers);
    let mut demux = containers.open_demuxer(format.name(), Box::new(std::io::Cursor::new(bytes)), &codecs)?;
    let params = demux.streams()[0].params.clone();
    let mut decoder = codecs.first_decoder(&params)?;
    if let Some(pts) = seek {
        let landed = demux.seek_to(0, pts)?;
        eprintln!("seek={pts} landed={landed}");
    }
    let mut out = BufWriter::new(std::fs::File::create(output)?);
    let mut frames = 0u64;
    loop {
        match demux.next_packet() {
            Ok(packet) => decoder.send_packet(&packet)?,
            Err(Error::Eof) => break,
            Err(error) => return Err(error.into()),
        }
        loop {
            match decoder.receive_frame() {
                Ok(Frame::Audio(frame)) => {
                    out.write_all(&frame.data[0])?;
                    frames += frame.samples as u64;
                }
                Err(Error::NeedMore | Error::Eof) => break,
                Ok(_) => return Err("non-audio frame".into()),
                Err(error) => return Err(error.into()),
            }
        }
    }
    out.flush()?;
    eprintln!("format={} rate=48000 channels=2 frames={frames}", format.name());
    Ok(())
}
