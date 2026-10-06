//! FFmpeg's side of every comparison: its view of the streams, and its
//! decode of one stream, addressed by `-map` specifier.

use std::path::Path;
use std::time::Duration;

use crate::tool;

/// How long one FFmpeg reference may take. Real samples decode in seconds;
/// a hang means a parser loop.
const TIMEOUT: Duration = Duration::from_secs(180);

fn path_arg(path: &Path) -> Result<String, String> {
    path.to_str().map(str::to_string).ok_or_else(|| format!("{} is not UTF-8", path.display()))
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

/// One stream as `ffprobe -show_streams` reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct FfStream {
    /// Absolute stream index (`-map 0:<index>`).
    pub index: u32,
    /// `video`, `audio`, `subtitle`, `data`, `attachment`.
    pub codec_type: String,
    pub codec_name: String,
    /// The container tag, e.g. `XVID` or `[0][0][0][0]`.
    pub codec_tag: String,
    pub sample_fmt: Option<String>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u16>,
    /// Cover art: FFmpeg lists it as a video stream, OxideAV as an attached
    /// picture of the container.
    pub attached_pic: bool,
}

impl FfStream {
    /// `-map` specifier of this stream.
    pub fn map(&self) -> String {
        format!("0:{}", self.index)
    }
}

/// Every stream of `path`, in FFmpeg's order.
pub fn streams(path: &Path) -> Result<Vec<FfStream>, String> {
    let out = tool::ffprobe(
        &[
            "-show_entries".to_string(),
            "stream=index,codec_type,codec_name,codec_tag_string,sample_fmt,sample_rate,channels:stream_disposition=attached_pic"
                .into(),
            "-of".into(),
            "json".into(),
            path_arg(path)?,
        ],
        Duration::from_secs(60),
    )?;
    let json: serde_json::Value =
        serde_json::from_slice(&out).map_err(|e| format!("ffprobe streams output: {e}"))?;
    let list = json["streams"].as_array().cloned().unwrap_or_default();
    Ok(list
        .iter()
        .map(|s| FfStream {
            index: s["index"].as_u64().unwrap_or(0) as u32,
            codec_type: s["codec_type"].as_str().unwrap_or("").to_string(),
            codec_name: s["codec_name"].as_str().unwrap_or("").to_string(),
            codec_tag: s["codec_tag_string"].as_str().unwrap_or("").to_string(),
            sample_fmt: s["sample_fmt"].as_str().map(str::to_string),
            sample_rate: s["sample_rate"].as_str().and_then(|r| r.parse().ok()),
            channels: s["channels"].as_u64().map(|c| c as u16),
            attached_pic: s["disposition"]["attached_pic"].as_u64() == Some(1),
        })
        .collect())
}

/// The streams of `codec_type` a player can select: FFmpeg's streams of that
/// type without cover art, in order.
pub fn of_type<'a>(streams: &'a [FfStream], codec_type: &str) -> Vec<&'a FfStream> {
    streams.iter().filter(|s| s.codec_type == codec_type && !s.attached_pic).collect()
}

/// MD5 of every frame FFmpeg decodes from stream `map`, through refcheck's
/// video oracle. FFmpeg's C IDCT is pinned for every stream (`-idct simple`,
/// 6ac540e): the IDCT codecs port FFmpeg's C `simple_idct`, which arm64
/// FFmpeg replaces with NEON assembly that rounds differently by default,
/// and decoders without an IDCT ignore the option.
pub fn video_md5s(path: &Path, map: &str, pix_fmt: &str) -> Result<Vec<String>, String> {
    let args = refcheck::ffmpeg_video_md5_args(path, map, pix_fmt, &["-idct", "simple"]);
    let out = tool::ffmpeg(&args, TIMEOUT)?;
    Ok(refcheck::parse_framemd5(&String::from_utf8_lossy(&out)))
}

/// A canonical interleaved PCM encoding FFmpeg can write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pcm {
    U8,
    S16,
    S32,
    F32,
    F64,
}

impl Pcm {
    /// The encoding of FFmpeg's decoder output `sample_fmt` (packed or
    /// planar), interleaved.
    pub fn of_sample_fmt(sample_fmt: &str) -> Option<Pcm> {
        Some(match sample_fmt.trim_end_matches('p') {
            "u8" => Pcm::U8,
            "s16" => Pcm::S16,
            "s32" => Pcm::S32,
            "flt" => Pcm::F32,
            "dbl" => Pcm::F64,
            _ => return None,
        })
    }

    pub fn bytes(self) -> usize {
        match self {
            Pcm::U8 => 1,
            Pcm::S16 => 2,
            Pcm::S32 | Pcm::F32 => 4,
            Pcm::F64 => 8,
        }
    }

    fn ffmpeg(self) -> (&'static str, &'static str) {
        match self {
            Pcm::U8 => ("u8", "pcm_u8"),
            Pcm::S16 => ("s16le", "pcm_s16le"),
            Pcm::S32 => ("s32le", "pcm_s32le"),
            Pcm::F32 => ("f32le", "pcm_f32le"),
            Pcm::F64 => ("f64le", "pcm_f64le"),
        }
    }
}

/// FFmpeg's decode of stream `map` as interleaved little-endian `pcm`.
pub fn audio_pcm(path: &Path, map: &str, pcm: Pcm) -> Result<Vec<u8>, String> {
    let (format, codec) = pcm.ffmpeg();
    let p = path_arg(path)?;
    tool::ffmpeg(&strings(&["-i", &p, "-map", map, "-f", format, "-c:a", codec, "-"]), TIMEOUT)
}

/// FFmpeg's decode of stream `map` as interleaved f32.
pub fn audio_f32(path: &Path, map: &str) -> Result<Vec<f32>, String> {
    let bytes = audio_pcm(path, map, Pcm::F32)?;
    Ok(bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect())
}

/// One frame of FFmpeg's decoder output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioFrameInfo {
    pub nb_samples: u32,
    pub channels: u16,
}

/// The frames FFmpeg's decoder emits for stream `index` (`ffprobe
/// -show_frames`): their sizes bound how far a lossy decode may run long
/// or short.
pub fn audio_frames(path: &Path, index: u32) -> Result<Vec<AudioFrameInfo>, String> {
    let p = path_arg(path)?;
    let index = index.to_string();
    let out = tool::ffprobe(
        &strings(&["-select_streams", &index, "-show_entries", "frame=nb_samples,channels", "-of", "csv=p=0", &p]),
        TIMEOUT,
    )?;
    let text = String::from_utf8_lossy(&out);
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let mut it = l.split(',').map(str::trim);
            let nb = it.next().and_then(|v| v.parse().ok());
            let ch = it.next().and_then(|v| v.parse().ok());
            match (nb, ch) {
                (Some(nb_samples), Some(channels)) => Ok(AudioFrameInfo { nb_samples, channels }),
                _ => Err(format!("ffprobe frame line {l:?}")),
            }
        })
        .collect()
}

/// Whether FFmpeg decodes stream `map` without error: a `decodes` policy is
/// only for streams FFmpeg cannot produce a reference for.
pub fn decodes(path: &Path, map: &str) -> bool {
    let Ok(p) = path_arg(path) else { return false };
    tool::ffmpeg(&strings(&["-i", &p, "-map", map, "-f", "null", "-"]), TIMEOUT).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streams_lists_fate_sample_streams_in_ffmpeg_order() {
        let s = streams(&refcheck::fate("wmv8/wmv8_x8intra.wmv")).unwrap();
        let summary: Vec<(u32, &str, &str)> =
            s.iter().map(|s| (s.index, s.codec_type.as_str(), s.codec_name.as_str())).collect();
        assert_eq!(summary, [(0, "audio", "wmav2"), (1, "video", "wmv2")]);
        assert_eq!(s[0].sample_fmt.as_deref(), Some("fltp"));
        assert_eq!(s[0].channels, Some(1));
    }

    #[test]
    fn pcm_follows_the_decoder_sample_format() {
        assert_eq!(Pcm::of_sample_fmt("s16p"), Some(Pcm::S16));
        assert_eq!(Pcm::of_sample_fmt("s32"), Some(Pcm::S32));
        assert_eq!(Pcm::of_sample_fmt("fltp"), Some(Pcm::F32));
        assert_eq!(Pcm::of_sample_fmt("u8"), Some(Pcm::U8));
        assert_eq!(Pcm::of_sample_fmt("s64"), None);
    }

    #[test]
    fn audio_frames_reports_decoder_frame_sizes() {
        // cook: 1024-sample stereo frames.
        let frames = audio_frames(&refcheck::fate("real/ra_cook.rm"), 0).unwrap();
        assert!(!frames.is_empty());
        assert!(frames.iter().all(|f| f.nb_samples == 1024 && f.channels == 2), "{:?}", &frames[..3]);
    }
}
