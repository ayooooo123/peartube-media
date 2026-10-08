//! Untimed full-stream PCM comparison. Keep integer PCM as bytes: converting
//! s32 to f32 would discard the low eight bits and could hide decode errors.

use oxideav_core::{AudioFormat, AudioFrame, CodecParameters, Decoder, Frame, SampleFormat};
use refcheck::Decoded;
use std::path::Path;
use std::process::Command;

/// Mirrors the private player::engine::audio_layout rule. Most upstream
/// decoders do not implement output_audio_format; do not invent a different
/// interpretation from the actual player when comparing their PCM.
pub fn layout(decoder: &dyn Decoder, params: &CodecParameters, frame: &AudioFrame) -> AudioFormat {
    if let Some(format) = decoder.output_audio_format() {
        return format;
    }
    let declared = params.sample_format;
    let planar = frame.data.len() > 1;
    let channels = if planar { frame.data.len() as u16 } else { params.channels.unwrap_or(1).max(1) };
    let per_plane = if planar { 1 } else { channels as usize };
    let samples = (frame.samples as usize).max(1);
    let width = frame.data.first().map_or(0, Vec::len) / (samples * per_plane);
    let fits = |f: SampleFormat| f.is_planar() == planar && f.bytes_per_sample() == width;
    let sample_format = match declared {
        Some(f) if fits(f) => f,
        _ => {
            let float = declared.map_or(true, |f| {
                matches!(f, SampleFormat::F32 | SampleFormat::F32P | SampleFormat::F64 | SampleFormat::F64P)
            });
            match (width, planar, float) {
                (1, false, _) => SampleFormat::U8,
                (1, true, _) => SampleFormat::U8P,
                (2, false, _) => SampleFormat::S16,
                (2, true, _) => SampleFormat::S16P,
                (3, false, _) => SampleFormat::S24,
                (4, false, true) => SampleFormat::F32,
                (4, false, false) => SampleFormat::S32,
                (4, true, true) => SampleFormat::F32P,
                (4, true, false) => SampleFormat::S32P,
                (8, false, _) => SampleFormat::F64,
                (8, true, _) => SampleFormat::F64P,
                _ => declared.unwrap_or(SampleFormat::F32),
            }
        }
    };
    AudioFormat { sample_format, sample_rate: params.sample_rate.unwrap_or(48000), channels }
}

pub fn exact(decoded: &Decoded, path: &Path, nth: usize) -> Result<bool, String> {
    let first = decoded.frame_formats.first().copied().flatten();
    let format = first.map(|f| f.sample_format).or(decoded.params.sample_format).ok_or("unknown sample format")?;
    let channels = first.map(|f| f.channels).or(decoded.params.channels).ok_or("unknown channel count")? as usize;
    if channels == 0 { return Err("zero channels".into()); }
    let (muxer, codec) = match format {
        SampleFormat::U8 | SampleFormat::U8P => ("u8", "pcm_u8"),
        SampleFormat::S16 | SampleFormat::S16P => ("s16le", "pcm_s16le"),
        SampleFormat::S24 => ("s24le", "pcm_s24le"),
        SampleFormat::S32 | SampleFormat::S32P => ("s32le", "pcm_s32le"),
        SampleFormat::F32 | SampleFormat::F32P => ("f32le", "pcm_f32le"),
        SampleFormat::F64 | SampleFormat::F64P => ("f64le", "pcm_f64le"),
        _ => return Err(format!("unsupported PCM layout {format:?}")),
    };
    let out = Command::new(refcheck::pinned_ffmpeg())
        .args(["-v", "error", "-nostdin", "-cpuflags", "0", "-i"])
        .arg(path)
        .args(["-map", &format!("0:a:{nth}"), "-f", muxer, "-c:a", codec, "-"])
        .output().map_err(|e| e.to_string())?;
    if !out.status.success() { return Err(String::from_utf8_lossy(&out.stderr).into_owned()); }
    if out.stdout.is_empty() { return Err("FFmpeg decoded no samples".into()); }
    let width = format.bytes_per_sample();
    let mut cursor = 0usize;
    let mut equal = true;
    for (index, frame) in decoded.frames.iter().enumerate() {
        let Frame::Audio(a) = frame else { continue };
        if let Some(f) = decoded.frame_formats.get(index).copied().flatten() {
            if f.sample_format != format || f.channels as usize != channels {
                return Err("output layout changed during the stream".into());
            }
        }
        if !format.is_planar() {
            let n = a.samples as usize * channels * width;
            let data = a.data.first().and_then(|p| p.get(..n)).ok_or("short PCM frame")?;
            equal &= out.stdout.get(cursor..cursor + n) == Some(data);
            cursor += n;
        } else {
            for i in 0..a.samples as usize {
                for c in 0..channels {
                    let sample = a.data.get(c).and_then(|p| p.get(i * width..(i + 1) * width)).ok_or("short PCM plane")?;
                    equal &= out.stdout.get(cursor..cursor + width) == Some(sample);
                    cursor += width;
                }
            }
        }
    }
    Ok(equal && cursor == out.stdout.len())
}

pub fn reference_f32(path: &Path, nth: usize) -> Result<Vec<f32>, String> {
    let out = Command::new(refcheck::pinned_ffmpeg())
        .args(["-v", "error", "-nostdin", "-cpuflags", "0", "-i"])
        .arg(path)
        .args(["-map", &format!("0:a:{nth}"), "-f", "f32le", "-c:a", "pcm_f32le", "-"])
        .output().map_err(|e| e.to_string())?;
    if !out.status.success() { return Err(String::from_utf8_lossy(&out.stderr).into_owned()); }
    Ok(out.stdout.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect())
}
