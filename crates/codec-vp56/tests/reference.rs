//! vp5, vp6, vp6f and vp6a against FFmpeg 2da55bf: every frame's MD5 equals
//! FFmpeg's, frame for frame, on the samples tests/fate/vpx.mak decodes
//! (VP5 and interlaced VP6 in AVI, VP6A in MOV, VP6F in FLV, VP6 in EA
//! files, read here by a test-only EA reader), plus VP6A in FLV, whose
//! adjustment byte crops 304x192 to 300x180. The decoder reports FFmpeg's
//! pixel format and size.

use std::collections::VecDeque;
use std::io::Read;
use std::path::Path;
use std::process::Command;

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, Error, Frame, MediaType, Packet, PixelFormat, ProbeData,
    ProbeScore, ReadSeek, Result, RuntimeContext, StreamInfo, TimeBase,
};
use refcheck::{fate, Registrar};

fn avi(ctx: &mut RuntimeContext) {
    oxideav_avi::__oxideav_entry(ctx);
}

fn mov(ctx: &mut RuntimeContext) {
    oxideav_mov::registry::register(ctx);
}

fn flv(ctx: &mut RuntimeContext) {
    oxideav_flv::register(ctx);
}

/// A test-only reader of EA's VP6 files (libavformat/electronicarts.c:
/// an `MVhd` chunk, then `MV0K` key frames and `MV0F` frames; chunk sizes
/// little-endian and including the 8-byte chunk header).
struct EaVp6 {
    streams: Vec<StreamInfo>,
    packets: VecDeque<Packet>,
}

impl Demuxer for EaVp6 {
    fn format_name(&self) -> &str {
        "ea_vp6_test"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        self.packets.pop_front().ok_or(Error::Eof)
    }
}

fn open_ea(mut input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    let mut data = Vec::new();
    input.read_to_end(&mut data)?;
    let tb = TimeBase::new(1, 1000);
    let mut packets = VecDeque::new();
    let mut at = 0;
    while at + 8 <= data.len() {
        let size = u32::from_le_bytes(data[at + 4..at + 8].try_into().unwrap()) as usize;
        if size < 8 || at + size > data.len() {
            break;
        }
        let tag = &data[at..at + 4];
        if tag == b"MV0K" || tag == b"MV0F" {
            let mut p = Packet::new(0, tb, data[at + 8..at + size].to_vec());
            p.pts = Some(packets.len() as i64);
            p.flags.keyframe = tag == b"MV0K";
            packets.push_back(p);
        }
        at += size;
    }
    let stream = StreamInfo { index: 0, time_base: tb, duration: None, start_time: Some(0), params: CodecParameters::video(CodecId::new("vp6")) };
    Ok(Box::new(EaVp6 { streams: vec![stream], packets }))
}

fn probe_ea(p: &ProbeData) -> ProbeScore {
    if p.buf.starts_with(b"MVhd") { 100 } else { 0 }
}

fn ea(ctx: &mut RuntimeContext) {
    let reg: &mut ContainerRegistry = &mut ctx.containers;
    reg.register_demuxer("ea_vp6_test", open_ea);
    reg.register_probe("ea_vp6_test", probe_ea);
}

/// FFmpeg's pixel format and size for stream `0:v:0`.
fn ffprobe_video(path: &Path) -> (String, usize, usize) {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=width,height,pix_fmt", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .expect("ffprobe on PATH");
    let text = String::from_utf8_lossy(&out.stdout);
    let f: Vec<&str> = text.lines().next().expect("a video stream").split(',').collect();
    (f[2].to_string(), f[0].parse().unwrap(), f[1].parse().unwrap())
}

fn plane_dims(pix_fmt: &str, w: usize, h: usize) -> Vec<(usize, usize)> {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    match pix_fmt {
        "yuv420p" => vec![(w, h), (cw, ch), (cw, ch)],
        "yuva420p" => vec![(w, h), (cw, ch), (cw, ch), (w, h)],
        other => panic!("not a VP5/VP6 pixel format: {other}"),
    }
}

/// The decoder's reports after its first frame.
fn decoder_reports(path: &Path, registrars: &[Registrar]) -> (Option<PixelFormat>, Option<(u32, u32)>) {
    let mut ctx = RuntimeContext::new();
    for r in registrars {
        r(&mut ctx);
    }
    let format = refcheck::probe_container(&ctx, path).unwrap();
    let mut d = ctx.containers.open_demuxer(&format, Box::new(std::fs::File::open(path).unwrap()), &ctx.codecs).unwrap();
    let stream = d.streams().iter().find(|s| s.params.media_type == MediaType::Video).unwrap().clone();
    let mut decoder = ctx.codecs.first_decoder(&stream.params).unwrap();
    loop {
        let p = d.next_packet().unwrap();
        if p.stream_index == stream.index {
            decoder.send_packet(&p).unwrap();
            if decoder.receive_frame().is_ok() {
                return (decoder.output_pixel_format(), decoder.output_video_dimensions());
            }
        }
    }
}

/// Every frame equals FFmpeg's; `input_args` go before FFmpeg's `-i` (a
/// duration limit compares that many frames).
fn check(path: &Path, registrars: &[Registrar], input_args: &[&str]) {
    let name = path.display().to_string();
    let (pix_fmt, w, h) = ffprobe_video(path);
    let (format, dims) = decoder_reports(path, registrars);
    assert_eq!(format.map(refcheck::ffmpeg_pix_fmt), Some(pix_fmt.as_str()), "{name}: the decoder's pixel format");
    assert_eq!(dims, Some((w as u32, h as u32)), "{name}: the decoder's size");
    let decoded = refcheck::decode(path, registrars, MediaType::Video, 0);
    let dims = plane_dims(&pix_fmt, w, h);
    let got: Vec<String> = decoded
        .frames
        .iter()
        .map(|f| {
            let Frame::Video(vf) = f else { panic!("{name}: not a video frame") };
            refcheck::md5_hex(&refcheck::pack(vf, &dims))
        })
        .collect();
    let want = refcheck::ffmpeg_video_md5s_with(path, 0, &pix_fmt, input_args);
    assert!(!want.is_empty(), "{name}: FFmpeg's frames");
    let got = if input_args.is_empty() { &got[..] } else { &got[..want.len().min(got.len())] };
    let first = got.iter().zip(&want).position(|(g, w)| g != w);
    assert!(first.is_none(), "{name}: frame {first:?} of {} differs from FFmpeg's", got.len());
    assert_eq!(got.len(), want.len(), "{name}: frames");
}

/// fate-vp5: the file is cut inside its last frame; FFmpeg's AVI demuxer
/// returns the frame's present bytes and decodes all 247 frames.
#[test]
fn vp5_in_avi() {
    check(&fate("vp5/potter512-400-partial.avi"), &[codec_vp56::register, avi], &[]);
}

/// fate-vp60-interlace1.
#[test]
fn vp6_interlaced_32x32_in_avi() {
    check(&fate("vp6/interlaced32x32.avi"), &[codec_vp56::register, avi], &[]);
}

/// fate-vp60-interlace2.
#[test]
fn vp6_interlaced_32x64_in_avi() {
    check(&fate("vp6/interlaced32x64.avi"), &[codec_vp56::register, avi], &[]);
}

/// fate-vp60.
#[test]
fn vp60_in_ea() {
    check(&fate("ea-vp6/g36.vp6"), &[codec_vp56::register, ea], &[]);
}

/// fate-vp61 (its first 4 seconds).
#[test]
fn vp61_in_ea() {
    check(&fate("ea-vp6/MovieSkirmishGondor.vp6"), &[codec_vp56::register, ea], &["-t", "4"]);
}

/// fate-vp6a: the alpha plane too.
#[test]
fn vp6a_in_mov() {
    check(&fate("flash-vp6/300x180-Scr-f8-056alpha.mov"), &[codec_vp56::register, mov], &[]);
}

/// fate-vp6f.
#[test]
fn vp6f_in_flv() {
    check(&fate("flash-vp6/clip1024.flv"), &[codec_vp56::register, flv], &[]);
}

/// VP6A in FLV: each packet's adjustment byte (FFmpeg's FLV demuxer moves
/// it to the extradata) crops the coded 304x192 to 300x180.
#[test]
fn vp6a_in_flv_cropped_by_its_adjustment_byte() {
    check(&fate("flash-vp6/300x180-Scr-f8-056alpha.flv"), &[codec_vp56::register, flv], &[]);
}
