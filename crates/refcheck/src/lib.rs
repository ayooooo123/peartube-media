//! Test helper for the codec crates: decodes a sample through OxideAV's
//! registries and FFmpeg's command line, and compares the two.
//!
//! Samples come from FFmpeg's FATE suite. Set `FATE_SUITE` to its directory
//! (default `~/projects/fate-suite`); fetch it with
//! `rsync -rlt rsync://fate-suite.ffmpeg.org/fate-suite/ ~/projects/fate-suite/`.
//! A missing sample fails the test: a reference test that skips proves nothing.

use audio_trim::{Pcm, Trimmer};
use oxideav_core::{
    AudioFormat, AudioFrame, CodecParameters, Error, Frame, MediaType, PROBE_SCORE_EXTENSION, PixelFormat, ProbeData,
    RuntimeContext, SampleFormat, StreamInfo, TimeBase, VideoFrame,
};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

pub mod trim_fixture;

/// Registers codecs and containers into a context, e.g. `oxideav_mkv::register`.
pub type Registrar = fn(&mut RuntimeContext);

/// Absolute path of a FATE sample, e.g. `fate("truehd/atmos.thd")`. Panics
/// with instructions when the suite or the file is missing.
pub fn fate(relative: &str) -> PathBuf {
    let root = std::env::var_os("FATE_SUITE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap()).join("projects/fate-suite"));
    let path = root.join(relative);
    assert!(
        path.is_file(),
        "missing FATE sample {} (set FATE_SUITE or rsync rsync://fate-suite.ffmpeg.org/fate-suite/ into {})",
        path.display(),
        root.display()
    );
    path
}

/// One decoded stream: its parameters, the decoder's own report of its audio
/// layout (when it gives one), and every frame, in output order.
pub struct Decoded {
    pub params: CodecParameters,
    /// `Decoder::output_audio_format` after the last frame.
    pub audio_format: Option<AudioFormat>,
    /// `Decoder::output_audio_format` as reported right after each frame of
    /// `frames` was received: one entry per frame, in the same order. A
    /// stream can change layout mid-way (LATM stereo to 5.1, HE-AAC mono
    /// until parametric stereo starts), so each frame is read in its own.
    pub frame_formats: Vec<Option<AudioFormat>>,
    /// `Decoder::output_video_dimensions` and `output_pixel_format` as
    /// reported right after each frame of `frames` was received: one entry
    /// per frame, in the same order.
    pub frame_video_layouts: Vec<VideoLayout>,
    pub frames: Vec<Frame>,
    /// Trims that could not be applied. Nonzero counts report a mismatch
    /// even when untrimmed fallback output happens to match a reference.
    pub trim_fallbacks: audio_trim::Fallbacks,
}

/// A decoder's report of a video frame's visible size and pixel format.
pub type VideoLayout = (Option<(u32, u32)>, Option<PixelFormat>);

/// The container the player's probe rule picks for `path` (engine-api.md):
/// the best content probe when it scores at least an extension match,
/// else the container registered for the file extension. Reads the first
/// 256 KiB, as the engine does.
pub fn probe_container(ctx: &RuntimeContext, path: &Path) -> Result<String, String> {
    let mut head = vec![0; 256 * 1024];
    let mut file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut n = 0;
    while n < head.len() {
        match file.read(&mut head[n..]) {
            Ok(0) => break,
            Ok(read) => n += read,
            Err(e) => return Err(format!("read {}: {e}", path.display())),
        }
    }
    let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    let probe = ProbeData { buf: &head[..n], ext: ext.as_deref() };
    let candidates = ctx.containers.probe_candidates(&probe);
    // Below an extension match a probe is guessing (EBU STL scores 5 on any
    // input); the player rejects those too.
    let by_extension = ext.as_deref().and_then(|e| ctx.containers.container_for_extension(e));
    match (candidates.first(), by_extension) {
        (Some(c), _) if c.score >= PROBE_SCORE_EXTENSION => Ok(c.name.to_string()),
        (_, Some(name)) => Ok(name.to_string()),
        _ => Err(format!(
            "no container claims this input (candidates: {:?})",
            candidates.iter().map(|c| (c.name, c.score)).collect::<Vec<_>>()
        )),
    }
}

/// Opens `path` with the containers and codecs that `registrars` install,
/// picks the `nth` stream of `kind`, and decodes all of it. Audio loses the
/// encoder delay and end padding its container declares
/// (`PacketMetadata::audio_trim`) and the decoder's own start delay, as the
/// player's does and as FFmpeg's decode does, so the frames compare with
/// FFmpeg's output.
pub fn decode(path: &Path, registrars: &[Registrar], kind: MediaType, nth: usize) -> Decoded {
    let mut ctx = RuntimeContext::new();
    for register in registrars {
        register(&mut ctx);
    }
    let format = probe_container(&ctx, path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let file = File::open(path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    let mut demuxer = ctx
        .containers
        .open_demuxer(&format, Box::new(file), &ctx.codecs)
        .unwrap_or_else(|e| panic!("open {format} demuxer: {e}"));
    let stream = demuxer
        .streams()
        .iter()
        .filter(|s| s.params.media_type == kind)
        .nth(nth)
        .unwrap_or_else(|| panic!("{}: no {kind:?} stream #{nth}", path.display()))
        .clone();
    let mut decoder_params = stream.params.clone();
    let decoder_delay = audio_trim::take_decoder_delay(&mut decoder_params);
    let mut decoder = ctx
        .codecs
        .first_decoder(&decoder_params)
        .unwrap_or_else(|e| panic!("no decoder for {:?}: {e}", stream.params.codec_id));
    let trimmer = Trimmer::with_decoder_delay(decoder_delay);
    let mut out = Output {
        frames: Vec::new(),
        frame_formats: Vec::new(),
        frame_video_layouts: Vec::new(),
        trimmer,
        kept: Vec::new(),
    };
    loop {
        match demuxer.next_packet() {
            Ok(packet) if packet.stream_index == stream.index => {
                let metadata = demuxer.packet_metadata();
                decoder.send_packet(&packet).unwrap_or_else(|e| panic!("send_packet: {e}"));
                if kind == MediaType::Audio {
                    out.trimmer.packet(&packet, metadata.audio_trim);
                }
                out.drain(&mut decoder, &stream);
            }
            Ok(_) => {}
            Err(Error::Eof) => break,
            Err(e) => panic!("demux: {e}"),
        }
    }
    // The last packet's span ends before the drain: drained frames take that
    // packet's trims frame by frame, as libavcodec's do.
    out.trimmer.drain(&mut out.kept);
    decoder.flush().unwrap_or_else(|e| panic!("flush: {e}"));
    out.drain(&mut decoder, &stream);
    out.trimmer.finish(&mut out.kept);
    out.save_kept();
    let trim_fallbacks = out.trimmer.take_fallbacks();
    if !trim_fallbacks.is_empty() {
        eprintln!("{}: audio trim mismatch: {trim_fallbacks:?}", path.display());
    }
    let Output { frames, frame_formats, frame_video_layouts, .. } = out;
    Decoded {
        params: stream.params,
        audio_format: decoder.output_audio_format(),
        frame_formats,
        frame_video_layouts,
        frames,
        trim_fallbacks,
    }
}

/// Asserts that every frame of `decoded` has the size and layout its decoder
/// reported right after returning it: the luma plane holds at least
/// `width` samples per row and `height` rows, and every plane fits the
/// format's plane geometry. Returns the distinct reported layouts, in order.
pub fn assert_reports_match_frames(decoded: &Decoded, name: &str) -> Vec<((u32, u32), PixelFormat)> {
    let mut layouts: Vec<((u32, u32), PixelFormat)> = Vec::new();
    for (i, (frame, report)) in decoded.frames.iter().zip(&decoded.frame_video_layouts).enumerate() {
        let Frame::Video(vf) = frame else { panic!("{name}: frame {i} is not video") };
        let (Some((w, h)), Some(format)) = *report else {
            panic!("{name}: frame {i}: decoder reported {report:?}");
        };
        let planes = vf.image_planes();
        assert_eq!(planes.len(), format.plane_count(), "{name}: frame {i} planes");
        for (p, plane) in planes.iter().enumerate() {
            let (_, rows) = format.plane_dimensions(p, w, h).expect("plane geometry");
            let row = format.plane_row_bytes(p, w).expect("plane row bytes");
            assert!(plane.stride >= row, "{name}: frame {i} plane {p}: stride {} < {row}", plane.stride);
            assert!(
                plane.data.len() >= plane.stride * (rows as usize - 1) + row,
                "{name}: frame {i} plane {p}: {} bytes for {rows} rows of {row}",
                plane.data.len()
            );
        }
        if layouts.last() != Some(&((w, h), format)) {
            layouts.push(((w, h), format));
        }
    }
    layouts
}

/// What `decode` keeps: frames (with the decoder's layout report for each)
/// after the trimmer.
struct Output {
    frames: Vec<Frame>,
    frame_formats: Vec<Option<AudioFormat>>,
    /// The decoder's video report right after each frame of `frames`
    /// (`(None, None)` for audio frames).
    frame_video_layouts: Vec<VideoLayout>,
    trimmer: Trimmer<Piece>,
    kept: Vec<Piece>,
}

impl Output {
    /// Receives every frame the decoder has ready.
    fn drain(&mut self, decoder: &mut Box<dyn oxideav_core::Decoder>, stream: &StreamInfo) {
        loop {
            let frame = match decoder.receive_frame() {
                Ok(frame) => frame,
                Err(Error::NeedMore) | Err(Error::Eof) => break,
                Err(e) => panic!("decode: {e}"),
            };
            let reported = decoder.output_audio_format();
            match frame {
                Frame::Audio(frame) => {
                    let layout = read_layout(reported, &stream.params, &frame);
                    let pts = frame.pts;
                    let piece = Piece {
                        frame,
                        reported,
                        format: layout.sample_format,
                        channels: layout.channels as usize,
                        rate: layout.sample_rate,
                        time_base: stream.time_base,
                    };
                    self.trimmer.frame(piece, pts, &mut self.kept);
                    self.save_kept();
                }
                frame => {
                    self.frames.push(frame);
                    self.frame_formats.push(reported);
                    self.frame_video_layouts.push((decoder.output_video_dimensions(), decoder.output_pixel_format()));
                }
            }
        }
    }

    fn save_kept(&mut self) {
        for piece in self.kept.drain(..) {
            self.frames.push(Frame::Audio(piece.frame));
            self.frame_formats.push(piece.reported);
            self.frame_video_layouts.push((None, None));
        }
    }
}

/// A decoded audio frame, or part of one, in the layout it is read in.
struct Piece {
    frame: AudioFrame,
    reported: Option<AudioFormat>,
    format: SampleFormat,
    channels: usize,
    rate: u32,
    time_base: TimeBase,
}

impl Piece {
    /// Bytes `n` samples take in each plane.
    fn bytes(&self, n: usize) -> usize {
        let per_sample = if self.format.is_planar() { 1 } else { self.channels };
        n.saturating_mul(per_sample).saturating_mul(self.format.bytes_per_sample())
    }

    /// The pts `n` samples later.
    fn pts_after(&self, n: usize) -> Option<i64> {
        let ticks = TimeBase::from_rate(self.rate.max(1)).rescale_checked(i64::try_from(n).ok()?, self.time_base)?;
        self.frame.pts?.checked_add(ticks)
    }
}

impl Pcm for Piece {
    fn samples(&self) -> usize {
        self.frame.samples as usize
    }

    fn rate(&self) -> u32 {
        self.rate
    }

    fn retained_bytes(&self) -> usize {
        self.frame.data.iter().map(Vec::capacity).sum::<usize>()
            + self.frame.data.capacity() * std::mem::size_of::<Vec<u8>>()
    }

    fn drop_front(&mut self, n: usize) {
        let bytes = self.bytes(n);
        for plane in &mut self.frame.data {
            plane.drain(..bytes.min(plane.len()));
        }
        self.frame.pts = self.pts_after(n);
        self.frame.samples -= n as u32;
    }

    fn split_off(&mut self, n: usize) -> Self {
        let bytes = self.bytes(n);
        let data = self.frame.data.iter_mut().map(|plane| plane.split_off(bytes.min(plane.len()))).collect();
        let samples = self.frame.samples - n as u32;
        let rest = AudioFrame { samples, pts: self.pts_after(n), data };
        self.frame.samples = n as u32;
        Piece { frame: rest, reported: self.reported, time_base: self.time_base, ..*self }
    }
}

/// FFmpeg's name for a pixel format, for `-pix_fmt`, or `None` when FFmpeg
/// has no equivalent.
pub fn ffmpeg_pix_fmt_name(format: PixelFormat) -> Option<&'static str> {
    Some(match format {
        PixelFormat::Yuv420P => "yuv420p",
        PixelFormat::Yuv422P => "yuv422p",
        PixelFormat::Yuv444P => "yuv444p",
        PixelFormat::Yuv440P => "yuv440p",
        PixelFormat::Yuv411P => "yuv411p",
        PixelFormat::YuvJ420P => "yuvj420p",
        PixelFormat::YuvJ422P => "yuvj422p",
        PixelFormat::YuvJ444P => "yuvj444p",
        PixelFormat::Yuv420P10Le => "yuv420p10le",
        PixelFormat::Yuv422P10Le => "yuv422p10le",
        PixelFormat::Yuv444P10Le => "yuv444p10le",
        PixelFormat::Yuv420P12Le => "yuv420p12le",
        PixelFormat::Yuv422P12Le => "yuv422p12le",
        PixelFormat::Yuv444P12Le => "yuv444p12le",
        PixelFormat::Yuv420P16Le => "yuv420p16le",
        PixelFormat::Yuv422P16Le => "yuv422p16le",
        PixelFormat::Yuv444P16Le => "yuv444p16le",
        PixelFormat::Yuva420P => "yuva420p",
        PixelFormat::Yuva422P => "yuva422p",
        PixelFormat::Yuva444P => "yuva444p",
        PixelFormat::Gray8 => "gray",
        PixelFormat::Gray10Le => "gray10le",
        PixelFormat::Gray12Le => "gray12le",
        PixelFormat::Gray16Le => "gray16le",
        PixelFormat::Ya8 => "ya8",
        PixelFormat::Rgb24 => "rgb24",
        PixelFormat::Bgr24 => "bgr24",
        PixelFormat::Rgba => "rgba",
        PixelFormat::Bgra => "bgra",
        PixelFormat::Argb => "argb",
        PixelFormat::Abgr => "abgr",
        PixelFormat::Rgb48Le => "rgb48le",
        PixelFormat::Rgba64Le => "rgba64le",
        PixelFormat::Gbrp8 => "gbrp",
        PixelFormat::Gbrap8 => "gbrap",
        PixelFormat::Pal8 => "pal8",
        PixelFormat::Nv12 => "nv12",
        PixelFormat::Nv21 => "nv21",
        PixelFormat::Yuyv422 => "yuyv422",
        PixelFormat::Uyvy422 => "uyvy422",
        PixelFormat::MonoBlack => "monob",
        PixelFormat::MonoWhite => "monow",
        _ => return None,
    })
}

/// FFmpeg's name for a pixel format, for `-pix_fmt`.
pub fn ffmpeg_pix_fmt(format: PixelFormat) -> &'static str {
    ffmpeg_pix_fmt_name(format).unwrap_or_else(|| panic!("refcheck: add FFmpeg's name for {format:?}"))
}

/// The frame's image planes packed row by row without stride padding, the
/// layout FFmpeg's rawvideo and framemd5 use. `plane_dims` gives each
/// plane's (bytes per row, rows).
pub fn pack(frame: &VideoFrame, plane_dims: &[(usize, usize)]) -> Vec<u8> {
    let planes = frame.image_planes();
    assert_eq!(planes.len(), plane_dims.len(), "plane count");
    let mut out = Vec::new();
    for (plane, &(row_bytes, rows)) in planes.iter().zip(plane_dims) {
        for row in 0..rows {
            let start = row * plane.stride;
            out.extend_from_slice(&plane.data[start..start + row_bytes]);
        }
    }
    out
}

/// MD5 of every video frame [`pinned_ffmpeg`] decodes from stream `0:v:nth`,
/// in the given pixel format, in output order. Cropping the bitstream
/// signals (SPS cropping) applies, as decoders output it; container
/// cropping (MOV `clap`) does not, because the player applies that at
/// presentation. `-fps_mode passthrough` keeps FFmpeg from duplicating or
/// dropping frames.
pub fn ffmpeg_video_md5s(path: &Path, nth: usize, pix_fmt: &str) -> Vec<String> {
    ffmpeg_video_md5s_with(path, nth, pix_fmt, &[])
}

/// [`ffmpeg_video_md5s`] with extra decoder options placed before `-i`, e.g.
/// `&["-idct", "simple"]` to pin FFmpeg's C IDCT: on arm64 its default picks
/// NEON assembly whose rounding differs from the C reference.
pub fn ffmpeg_video_md5s_with(path: &Path, nth: usize, pix_fmt: &str, input_args: &[&str]) -> Vec<String> {
    video_md5s(&pinned_ffmpeg(), path, nth, pix_fmt, input_args)
}

/// [`ffmpeg_video_md5s`] for AV1: libdav1d's pictures, through
/// [`system_ffmpeg`]. The pinned build has no software AV1 decoder: it has
/// no libdav1d, and its native `av1` decoder needs hardware acceleration.
pub fn dav1d_video_md5s(path: &Path, nth: usize, pix_fmt: &str) -> Vec<String> {
    video_md5s(&system_ffmpeg(), path, nth, pix_fmt, &["-c:v", "libdav1d"])
}

fn video_md5s(binary: &Path, path: &Path, nth: usize, pix_fmt: &str, input_args: &[&str]) -> Vec<String> {
    let args = ffmpeg_video_md5_args(path, &format!("0:v:{nth}"), pix_fmt, input_args);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    parse_framemd5(&String::from_utf8(run(binary, &args)).unwrap())
}

/// The arguments (after FFmpeg's global `-v error -nostdin`) of the video
/// oracle behind [`ffmpeg_video_md5s_with`], for the stream `map` (an
/// `-map` specifier such as `0:v:1` or `0:3`).
pub fn ffmpeg_video_md5_args(path: &Path, map: &str, pix_fmt: &str, input_args: &[&str]) -> Vec<String> {
    let mut args = vec!["-apply_cropping".to_string(), "codec".to_string()];
    args.extend(input_args.iter().map(|a| a.to_string()));
    for a in [
        "-i",
        path.to_str().unwrap(),
        "-map",
        map,
        "-fps_mode",
        "passthrough",
        "-pix_fmt",
        pix_fmt,
        "-f",
        "framemd5",
        "-",
    ] {
        args.push(a.to_string());
    }
    args
}

/// The per-frame hashes of FFmpeg's `framemd5` output, in order.
pub fn parse_framemd5(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|l| l.rsplit(',').next().unwrap().trim().to_string())
        .collect()
}

/// Hex MD5 of `bytes`.
pub fn md5_hex(bytes: &[u8]) -> String {
    format!("{:x}", md5::compute(bytes))
}

/// [`pinned_ffmpeg`]'s decode of stream `0:a:nth` as interleaved f32 at the
/// source rate and channel count.
pub fn ffmpeg_audio_f32(path: &Path, nth: usize) -> Vec<f32> {
    let out = run(&pinned_ffmpeg(), &[
        "-i", path.to_str().unwrap(), "-map", &format!("0:a:{nth}"), "-f", "f32le", "-c:a", "pcm_f32le", "-",
    ]);
    out.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect()
}

/// The FFmpeg tree the ports follow, commit 2da55bf, with its `ffmpeg`
/// and `ffprobe` built: `$FFMPEG_SRC`, default ~/projects/ffmpeg-src.
pub fn ffmpeg_src() -> PathBuf {
    std::env::var_os("FFMPEG_SRC")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap()).join("projects/ffmpeg-src"))
}

/// The `ffmpeg` of [`ffmpeg_src`]. Every reference comes from it: the
/// `ffmpeg` on PATH (Homebrew 9.0.2) decodes and demuxes some files
/// differently. With `-cpuflags 0` it runs the C code paths a port
/// reproduces.
pub fn pinned_ffmpeg() -> PathBuf {
    pinned_tool("ffmpeg")
}

/// The `ffprobe` of [`ffmpeg_src`].
pub fn pinned_ffprobe() -> PathBuf {
    pinned_tool("ffprobe")
}

/// `name` in [`ffmpeg_src`], checked once to report commit 2da55bf, so a
/// stale or missing build fails loudly.
fn pinned_tool(name: &str) -> PathBuf {
    static CHECKED: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let path = ffmpeg_src().join(name);
    let mut checked = CHECKED.lock().unwrap_or_else(|e| e.into_inner());
    if !checked.iter().any(|c| c == name) {
        let out = Command::new(&path)
            .arg("-version")
            .output()
            .unwrap_or_else(|e| panic!("{}: {e} (build FFmpeg 2da55bf in FFMPEG_SRC)", path.display()));
        let version = String::from_utf8_lossy(&out.stdout);
        assert!(version.contains("2da55bf"), "{} is not FFmpeg 2da55bf: {}", path.display(), version.lines().next().unwrap_or(""));
        checked.push(name.to_string());
    }
    path
}

/// The `ffmpeg` on PATH (Homebrew 9.0.2), with external libraries the
/// pinned build lacks. It makes the test inputs (encoders such as libx264,
/// libx265, libvpx, libaom, libmp3lame, libopus, libvorbis and libtheora
/// are only here), and it gives the two references FFmpeg's own code
/// cannot: AV1 pictures ([`dav1d_video_md5s`]) and ASS rendering
/// (libass). Every other reference comes from [`pinned_ffmpeg`].
pub fn system_ffmpeg() -> PathBuf {
    PathBuf::from("ffmpeg")
}

/// The layout refcheck reads (and trims) a frame in: the decoder's report,
/// else the container's declared format, else, when the container declares
/// none, what the player infers from the frame (`audio_trim::frame_layout`).
fn read_layout(reported: Option<AudioFormat>, params: &CodecParameters, frame: &AudioFrame) -> AudioFormat {
    match (reported, params.sample_format) {
        (Some(f), _) => f,
        (None, Some(sample_format)) => AudioFormat {
            sample_format,
            sample_rate: params.sample_rate.unwrap_or(48000),
            channels: params.channels.unwrap_or(1),
        },
        (None, None) => audio_trim::frame_layout(None, params, frame),
    }
}

/// Every audio frame converted to interleaved f32 in [-1, 1]. Each frame is
/// read in the layout the decoder reported for it through
/// `Decoder::output_audio_format` (see [`Decoded::frame_formats`]), else in
/// the container's declared one, else in the one the player infers from
/// the frame. Panics when a frame's buffers are shorter than its sample
/// count in that layout.
pub fn interleaved_f32(decoded: &Decoded) -> Vec<f32> {
    let mut out = Vec::new();
    for (index, frame) in decoded.frames.iter().enumerate() {
        let Frame::Audio(a) = frame else { continue };
        let layout = read_layout(decoded.frame_formats.get(index).copied().flatten(), &decoded.params, a);
        let (format, channels) = (layout.sample_format, layout.channels as usize);
        let n = a.samples as usize;
        let w = format.bytes_per_sample();
        let (planes, per_plane) = if format.is_planar() { (channels, n * w) } else { (1, n * channels * w) };
        assert!(
            a.data.len() >= planes && a.data[..planes].iter().all(|p| p.len() >= per_plane),
            "frame {index}: {n} samples x {channels} channels of {format:?} need {planes} plane(s) of {per_plane} bytes, \
             the frame has {:?}",
            a.data.iter().map(Vec::len).collect::<Vec<_>>()
        );
        for i in 0..n {
            for c in 0..channels {
                out.push(sample_f32(format, &a.data, channels, c, i));
            }
        }
    }
    out
}

fn sample_f32(format: SampleFormat, data: &[Vec<u8>], channels: usize, c: usize, i: usize) -> f32 {
    let (plane, index) = if format.is_planar() { (&data[c], i) } else { (&data[0], i * channels + c) };
    let w = format.bytes_per_sample();
    let b = &plane[index * w..index * w + w];
    match format {
        SampleFormat::U8 | SampleFormat::U8P => (b[0] as f32 - 128.0) / 128.0,
        SampleFormat::S16 | SampleFormat::S16P => i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0,
        SampleFormat::S32 | SampleFormat::S32P => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2147483648.0,
        SampleFormat::F32 | SampleFormat::F32P => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        SampleFormat::S24 => (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8388608.0,
        SampleFormat::F64 | SampleFormat::F64P => f64::from_le_bytes(b.try_into().unwrap()) as f32,
        other => panic!("refcheck: add conversion for {other:?}"),
    }
}

/// Signal-to-noise ratio of `test` against `reference`, in dB, over their
/// common length, or why it cannot be scored: an empty reference, an empty
/// comparison interval, a non-finite sample on either side, or lengths
/// that differ by more than `slack` samples. A silent reference scores
/// +infinity against silence and -infinity against anything else, so the
/// only acceptance test is `snr >= floor`.
pub fn try_snr_db(reference: &[f32], test: &[f32], slack: usize) -> Result<f64, String> {
    if reference.is_empty() {
        return Err("the reference is empty".into());
    }
    if test.is_empty() {
        return Err("nothing to compare: the decode is empty".into());
    }
    if reference.len().abs_diff(test.len()) > slack {
        return Err(format!("length {} vs FFmpeg {} (slack {slack})", test.len(), reference.len()));
    }
    if let Some(i) = reference.iter().position(|x| !x.is_finite()) {
        return Err(format!("reference sample {i} is {}", reference[i]));
    }
    if let Some(i) = test.iter().position(|x| !x.is_finite()) {
        return Err(format!("decoded sample {i} is {}", test[i]));
    }
    let n = reference.len().min(test.len());
    let (mut signal, mut noise) = (0f64, 0f64);
    for i in 0..n {
        let r = reference[i] as f64;
        signal += r * r;
        noise += (r - test[i] as f64).powi(2);
    }
    Ok(if noise == 0.0 { f64::INFINITY } else { 10.0 * (signal / noise).log10() })
}

/// [`try_snr_db`], panicking when the inputs cannot be scored.
pub fn snr_db(reference: &[f32], test: &[f32], slack: usize) -> f64 {
    try_snr_db(reference, test, slack).unwrap_or_else(|e| panic!("snr_db: {e}"))
}

/// `binary -v error -nostdin <args>`; its stdout.
fn run(binary: &Path, args: &[&str]) -> Vec<u8> {
    let out = Command::new(binary)
        .args(["-v", "error", "-nostdin"])
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("{}: {e}", binary.display()));
    assert!(out.status.success(), "{} {args:?}: {}", binary.display(), String::from_utf8_lossy(&out.stderr));
    out.stdout
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::{
        AudioFrame, CodecId, CodecInfo, CodecResolver, Decoder, Demuxer, Packet, ReadSeek, StreamInfo, TimeBase,
    };

    #[test]
    fn snr_rejects_an_empty_reference() {
        assert!(try_snr_db(&[], &[], 0).is_err(), "empty/empty");
        assert!(try_snr_db(&[], &[0.25, -0.25], 2).is_err(), "empty reference / nonempty decode");
    }

    #[test]
    fn snr_rejects_an_empty_comparison_interval() {
        assert!(try_snr_db(&[0.25, -0.25], &[], 2).is_err(), "nonempty reference / empty decode");
    }

    #[test]
    fn snr_of_silence_against_noise_is_minus_infinity_and_fails_any_floor() {
        let snr = try_snr_db(&[0.0; 4], &[0.1, -0.1, 0.1, -0.1], 0).unwrap();
        assert_eq!(snr, f64::NEG_INFINITY);
        assert!(snr < 90.0, "-infinity must fail the acceptance test snr >= floor");
        let exact = try_snr_db(&[0.0; 4], &[0.0; 4], 0).unwrap();
        assert_eq!(exact, f64::INFINITY);
        assert!(exact >= 90.0);
    }

    #[test]
    fn snr_bounds_the_length_difference_by_the_slack() {
        let reference = [0.5f32; 10];
        assert!(try_snr_db(&reference, &reference[..7], 2).is_err(), "3 missing samples, slack 2");
        assert_eq!(try_snr_db(&reference, &reference[..7], 3), Ok(f64::INFINITY));
        let mut longer = reference.to_vec();
        longer.extend([0.5; 3]);
        assert!(try_snr_db(&reference, &longer, 2).is_err(), "3 extra samples, slack 2");
    }

    #[test]
    fn snr_rejects_non_finite_samples_on_either_side() {
        assert!(try_snr_db(&[0.5, f32::NAN], &[0.5, 0.5], 0).is_err());
        assert!(try_snr_db(&[0.5, 0.5], &[0.5, f32::INFINITY], 0).is_err());
    }

    #[test]
    #[should_panic(expected = "the reference is empty")]
    fn snr_db_panics_where_try_snr_db_errs() {
        snr_db(&[], &[0.1], 1);
    }

    fn s16_plane(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    fn format(sample_format: SampleFormat, channels: u16) -> AudioFormat {
        AudioFormat { sample_format, sample_rate: 48000, channels }
    }

    fn audio(samples: u32, data: Vec<Vec<u8>>) -> Frame {
        Frame::Audio(AudioFrame { samples, pts: None, data })
    }

    #[test]
    fn interleaved_f32_reads_each_frame_in_its_own_layout() {
        let mut params = CodecParameters::audio(CodecId::new("test"));
        params.sample_format = Some(SampleFormat::S16);
        params.channels = Some(1);
        // Mono S16 until the stream switches to stereo planar float, as
        // LATM stereo-to-5.1 or HE-AAC mono-until-PS streams do.
        let decoded = Decoded {
            params,
            trim_fallbacks: audio_trim::Fallbacks::default(),
            audio_format: Some(format(SampleFormat::F32P, 2)),
            frame_formats: vec![Some(format(SampleFormat::S16, 1)), Some(format(SampleFormat::F32P, 2))],
            frame_video_layouts: vec![(None, None); 2],
            frames: vec![
                audio(2, vec![s16_plane(&[16384, -16384])]),
                audio(
                    2,
                    vec![
                        [0.5f32, 0.25].iter().flat_map(|x| x.to_le_bytes()).collect(),
                        [-0.5f32, -0.25].iter().flat_map(|x| x.to_le_bytes()).collect(),
                    ],
                ),
            ],
        };
        assert_eq!(interleaved_f32(&decoded), vec![0.5, -0.5, 0.5, -0.5, 0.25, -0.25]);
    }

    #[test]
    #[should_panic(expected = "need 2 plane(s)")]
    fn interleaved_f32_refuses_a_frame_shorter_than_its_layout() {
        let mut params = CodecParameters::audio(CodecId::new("test"));
        params.sample_format = Some(SampleFormat::S16P);
        params.channels = Some(2);
        let decoded = Decoded {
            params,
            trim_fallbacks: audio_trim::Fallbacks::default(),
            audio_format: None,
            frame_formats: vec![None],
            frame_video_layouts: vec![(None, None)],
            frames: vec![audio(2, vec![s16_plane(&[1, 2])])],
        };
        interleaved_f32(&decoded);
    }

    // A container and decoder whose output layout changes mid-stream: the
    // first two packets decode to mono S16, the rest to stereo S16.

    const LAYOUT_CODEC: &str = "refcheck_layout_switch";

    struct SwitchingDemuxer {
        streams: Vec<StreamInfo>,
        next: u8,
    }

    impl Demuxer for SwitchingDemuxer {
        fn format_name(&self) -> &str {
            "refcheck_layout"
        }
        fn streams(&self) -> &[StreamInfo] {
            &self.streams
        }
        fn next_packet(&mut self) -> oxideav_core::Result<Packet> {
            if self.next == 4 {
                return Err(Error::Eof);
            }
            let packet = Packet::new(0, TimeBase::new(1, 48000), vec![self.next]);
            self.next += 1;
            Ok(packet)
        }
    }

    fn open_switching(_input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> oxideav_core::Result<Box<dyn Demuxer>> {
        let mut params = CodecParameters::audio(CodecId::new(LAYOUT_CODEC));
        params.sample_format = Some(SampleFormat::S16);
        params.channels = Some(2);
        params.sample_rate = Some(48000);
        let stream = StreamInfo { index: 0, time_base: TimeBase::new(1, 48000), duration: None, start_time: None, params };
        Ok(Box::new(SwitchingDemuxer { streams: vec![stream], next: 0 }))
    }

    struct SwitchingDecoder {
        id: CodecId,
        channels: u16,
        pending: Option<Frame>,
    }

    impl Decoder for SwitchingDecoder {
        fn codec_id(&self) -> &CodecId {
            &self.id
        }
        fn send_packet(&mut self, packet: &Packet) -> oxideav_core::Result<()> {
            let k = packet.data[0] as i16;
            self.channels = if k < 2 { 1 } else { 2 };
            // Two samples per channel, value 1000 * packet + channel.
            let samples: Vec<i16> = (0..2).flat_map(|_| (0..self.channels as i16).map(move |c| 1000 * k + c)).collect();
            self.pending = Some(audio(2, vec![s16_plane(&samples)]));
            Ok(())
        }
        fn receive_frame(&mut self) -> oxideav_core::Result<Frame> {
            self.pending.take().ok_or(Error::NeedMore)
        }
        fn flush(&mut self) -> oxideav_core::Result<()> {
            Ok(())
        }
        fn output_audio_format(&self) -> Option<AudioFormat> {
            Some(format(SampleFormat::S16, self.channels))
        }
    }

    fn make_switching(_params: &CodecParameters) -> oxideav_core::Result<Box<dyn Decoder>> {
        Ok(Box::new(SwitchingDecoder { id: CodecId::new(LAYOUT_CODEC), channels: 0, pending: None }))
    }

    fn register_switching(ctx: &mut RuntimeContext) {
        ctx.containers.register_demuxer("refcheck_layout", open_switching);
        ctx.containers.register_extension("rclayout", "refcheck_layout");
        ctx.codecs.register(CodecInfo::new(CodecId::new(LAYOUT_CODEC)).decoder(make_switching));
    }

    #[test]
    fn decode_snapshots_the_layout_of_every_frame() {
        let path = std::env::temp_dir().join(format!("refcheck-layout-{}.rclayout", std::process::id()));
        std::fs::write(&path, b"layout switch").unwrap();
        let decoded = decode(&path, &[register_switching], MediaType::Audio, 0);
        let _ = std::fs::remove_file(&path);

        let channels: Vec<u16> = decoded.frame_formats.iter().map(|f| f.unwrap().channels).collect();
        assert_eq!(channels, [1, 1, 2, 2], "per-frame layouts");
        assert_eq!(decoded.audio_format.unwrap().channels, 2, "final layout");
        let scale = |v: i16| v as f32 / 32768.0;
        let expected: Vec<f32> = [0, 0, 1000, 1000, 2000, 2001, 2000, 2001, 3000, 3001, 3000, 3001]
            .into_iter()
            .map(scale)
            .collect();
        assert_eq!(interleaved_f32(&decoded), expected);
    }

    // `decode` applies each packet's `audio_trim` once, after decoding, as
    // libavcodec does; the fixture's samples carry their output index.

    use trim_fixture::{Mode, Spec};

    /// The decoder-output samples `decode` keeps, as runs `[start, end)`.
    fn decode_fixture(spec: &Spec) -> Vec<(u64, u64)> {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let name = format!("refcheck-trim-{}-{n}.{}", std::process::id(), trim_fixture::EXTENSION);
        let path = std::env::temp_dir().join(name);
        std::fs::write(&path, spec.to_bytes()).unwrap();
        let decoded = decode(&path, &[trim_fixture::register], MediaType::Audio, 0);
        let _ = std::fs::remove_file(&path);
        trim_fixture::runs(&trim_fixture::indices(&interleaved_f32(&decoded), spec.channels as usize))
    }

    fn range(start: u64, end: u64) -> Vec<(u64, u64)> {
        if start < end { vec![(start, end)] } else { Vec::new() }
    }

    #[test]
    fn decode_skips_priming_across_frames_and_drops_the_end_padding() {
        // MP4 edit-list priming: 2220 samples, more than two 1024-sample
        // frames, stamped before zero; 340 samples of padding at the end.
        let mut spec = Spec::new(1, 48000, 1024, 6);
        spec.start_pts = -2220;
        spec.packets[0].skip = 2220;
        spec.packets[5].discard = 340;
        assert_eq!(decode_fixture(&spec), range(2220, 6 * 1024 - 340));
    }

    #[test]
    fn decode_rescales_trims_to_the_decoders_output_rate() {
        // An SBR-like decoder doubles the declared rate: each 1024-sample
        // packet decodes to 2048 samples, and the trims double with it.
        let mut spec = Spec::new(2, 24000, 1024, 5);
        spec.output_rate = 48000;
        spec.packets[0].skip = 1100;
        spec.packets[4].discard = 100;
        assert_eq!(decode_fixture(&spec), range(2200, 5 * 2048 - 200));
    }

    #[test]
    fn decode_trims_the_tail_a_delayed_decoder_returns_at_flush() {
        let mut spec = Spec::new(1, 48000, 1024, 4);
        spec.mode = Mode::Delayed;
        spec.packets[0].skip = 1500;
        spec.packets[3].discard = 300;
        assert_eq!(decode_fixture(&spec), range(1500, 4 * 1024 - 300));
    }

    #[test]
    fn padding_stays_with_the_packet_a_delayed_decoder_outputs_late() {
        // The decoder returns each packet's samples after the next packet
        // is sent: packet 1's padding is sample 7, whenever it comes out.
        let mut spec = Spec::new(1, 48000, 4, 3);
        spec.mode = Mode::Delayed;
        spec.packets[1].discard = 1;
        assert_eq!(decode_fixture(&spec), vec![(0, 7), (8, 12)]);
    }

    #[test]
    fn stamped_frames_find_their_packet_after_one_that_decoded_to_nothing() {
        // The first packet decodes to nothing; the frames carry their
        // packet's pts, so packet 2's padding stays on packet 2's samples.
        let mut spec = Spec::new(1, 48000, 1024, 4);
        spec.silent_packets = 1;
        spec.stamp = true;
        spec.packets[2].discard = 100;
        assert_eq!(decode_fixture(&spec), vec![(1024, 3 * 1024 - 100), (3 * 1024, 4 * 1024)]);
    }

    #[test]
    fn without_durations_or_pts_output_follows_the_order_packets_were_sent() {
        // As Ogg Opus: no packet durations, unstamped frames.
        let mut spec = Spec::new(1, 48000, 1024, 4);
        spec.silent_packets = 1;
        spec.durations = false;
        spec.packets[2].discard = 100;
        assert_eq!(decode_fixture(&spec), vec![(1024, 3 * 1024 - 100), (3 * 1024, 4 * 1024)]);
    }

    #[test]
    fn decode_drops_padding_that_spans_frames_of_one_packet() {
        // Two 512-sample frames per packet; 700 samples of padding cover
        // the whole second frame and the end of the first.
        let mut spec = Spec::new(1, 48000, 1024, 3);
        spec.mode = Mode::Split;
        spec.packets[2].discard = 700;
        assert_eq!(decode_fixture(&spec), range(0, 3 * 1024 - 700));
    }

    #[test]
    fn a_later_skip_replaces_the_pending_one() {
        // libavcodec's decode.c: a nonzero skip in the side data replaces
        // what is left of the previous one; it does not add to it.
        let mut spec = Spec::new(1, 48000, 1024, 4);
        spec.packets[0].skip = 3000;
        spec.packets[1].skip = 100;
        assert_eq!(decode_fixture(&spec), range(1024 + 100, 4 * 1024));
    }

    #[test]
    fn long_decoder_silence_keeps_output_without_a_trim_panic() {
        let mut spec = Spec::new(1, 48000, 4, 104);
        spec.silent_packets = 100;
        spec.stamp = true;
        spec.packets[102].discard = 1;
        assert_eq!(decode_fixture(&spec), vec![(400, 411), (412, 416)]);
    }

    #[test]
    fn hostile_trims_are_bounded_and_invalid_ones_ignored() {
        // A skip of u32::MAX seconds' worth drops everything, quickly.
        let mut spec = Spec::new(1, 48000, 1024, 3);
        spec.packets[0].skip = u32::MAX;
        spec.packets[0].trim_rate = 1;
        assert_eq!(decode_fixture(&spec), range(0, 0));
        // Padding larger than the packet's output is ignored.
        let mut spec = Spec::new(1, 48000, 1024, 3);
        spec.packets[2].discard = u32::MAX;
        spec.packets[2].trim_rate = 1;
        assert_eq!(decode_fixture(&spec), range(0, 3 * 1024));
        // A trim without a rate means nothing.
        let mut spec = Spec::new(1, 48000, 1024, 3);
        spec.packets[0].skip = 1000;
        spec.packets[2].discard = 1000;
        spec.packets[0].trim_rate = 0;
        spec.packets[2].trim_rate = 0;
        assert_eq!(decode_fixture(&spec), range(0, 3 * 1024));
    }
}
