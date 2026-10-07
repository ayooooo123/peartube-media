//! Helpers for the decoder regression tests in `tests/`.
//!
//! A decoder is judged on exactly the input FFmpeg's decoder sees: the
//! packets FFmpeg's own demuxer and parser emit for one stream (payloads,
//! timestamps and key flags) go straight into the OxideAV decoder,
//! whatever the container forks do.

#![forbid(unsafe_code)]

use oxideav_core::{CodecParameters, Decoder, Error, Packet, RuntimeContext, TimeBase};
use refcheck::{Decoded, Registrar};
use std::path::Path;
use std::process::Command;

/// Every packet FFmpeg reads for stream `spec` (`"v:0"`, `"a:1"`) of
/// `path`, in demux order, after the bitstream filters in `bsf` (e.g.
/// `h264_mp4toannexb` for length-prefixed H.264): the bytes
/// `ffmpeg -c copy -f data` writes, cut, timed and flagged as
/// `-f framecrc` lists the same packets. Panics when FFmpeg fails or the
/// two outputs disagree.
pub fn ffmpeg_packets(path: &Path, spec: &str, bsf: Option<&str>) -> Vec<Packet> {
    let p = path.to_str().expect("UTF-8 path");
    let map = format!("0:{spec}");
    // `-copyinkf`: a stream copy otherwise drops the packets before the
    // first keyframe, which FFmpeg's decoder does receive.
    let mut args = vec!["-v", "error", "-nostdin", "-i", p, "-map", &map, "-c", "copy", "-copyinkf"];
    if let Some(bsf) = bsf {
        args.extend(["-bsf", bsf]);
    }
    let table = String::from_utf8(tool("ffmpeg", &[&args[..], &["-f", "framecrc", "-"]].concat()))
        .expect("framecrc output is UTF-8");
    let data = tool("ffmpeg", &[&args[..], &["-f", "data", "-"]].concat());

    let mut time_base = None;
    let mut packets = Vec::new();
    let mut offset = 0usize;
    for line in table.lines() {
        if let Some(tb) = line.strip_prefix("#tb 0: ") {
            let (num, den) = tb.trim().split_once('/').expect("#tb is num/den");
            time_base = Some(TimeBase::new(num.parse().expect("numerator"), den.parse().expect("denominator")));
            continue;
        }
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        // stream, dts, pts, duration, size, crc[, F=0x<flags>][, side data]
        let fields: Vec<&str> = line.split(',').map(str::trim).collect();
        assert!(fields.len() >= 6, "framecrc line: {line}");
        let number = |i: usize| fields[i].parse::<i64>().ok();
        let size: usize = fields[4].parse().expect("packet size");
        let flags = fields[6..]
            .iter()
            .find_map(|f| f.strip_prefix("F=0x"))
            .map_or(1, |hex| u32::from_str_radix(hex, 16).expect("packet flags"));
        let end = offset + size;
        assert!(end <= data.len(), "{p} {spec}: framecrc lists more bytes than the data muxer wrote");
        let tb = time_base.expect("framecrc prints #tb before the packets");
        let mut packet = Packet::new(0, tb, data[offset..end].to_vec()).with_keyframe(flags & 1 != 0);
        packet.dts = number(1);
        packet.pts = number(2);
        packet.duration = number(3);
        packets.push(packet);
        offset = end;
    }
    assert_eq!(offset, data.len(), "{p} {spec}: framecrc's packet sizes do not add up to the copied bytes");
    packets
}

/// Sends every packet to the first decoder `registrars` install for
/// `params`, then flushes, and returns all output with every error the
/// decoder reported (packet index, message; a flush error carries index
/// `packets.len()`). Decoding goes on after a refused packet, as the
/// player's does.
pub fn decode_packets(
    registrars: &[Registrar],
    params: &CodecParameters,
    packets: &[Packet],
) -> (Decoded, Vec<(usize, String)>) {
    let mut ctx = RuntimeContext::new();
    for register in registrars {
        register(&mut ctx);
    }
    let mut decoder = ctx
        .codecs
        .first_decoder(params)
        .unwrap_or_else(|e| panic!("no decoder for {:?}: {e}", params.codec_id));
    let mut out = Decoded {
        params: params.clone(),
        audio_format: None,
        frame_formats: Vec::new(),
        frames: Vec::new(),
        trim_fallbacks: Default::default(),
    };
    let mut errors = Vec::new();
    let drain = |decoder: &mut Box<dyn Decoder>, out: &mut Decoded, errors: &mut Vec<(usize, String)>, index: usize| loop {
        match decoder.receive_frame() {
            Ok(frame) => {
                out.frames.push(frame);
                out.frame_formats.push(decoder.output_audio_format());
            }
            Err(Error::NeedMore) | Err(Error::Eof) => break,
            Err(e) => {
                errors.push((index, format!("receive_frame: {e}")));
                break;
            }
        }
    };
    for (index, packet) in packets.iter().enumerate() {
        match decoder.send_packet(packet) {
            Ok(()) => drain(&mut decoder, &mut out, &mut errors, index),
            Err(e) => errors.push((index, format!("send_packet: {e}"))),
        }
    }
    if let Err(e) = decoder.flush() {
        errors.push((packets.len(), format!("flush: {e}")));
    }
    drain(&mut decoder, &mut out, &mut errors, packets.len());
    out.audio_format = decoder.output_audio_format();
    (out, errors)
}

/// Runs an FFmpeg tool (`ffmpeg`, `ffprobe`) and returns its stdout;
/// panics with its stderr when it fails.
pub fn tool(program: &str, args: &[&str]) -> Vec<u8> {
    let out = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("{program} must be on PATH: {e}"));
    assert!(out.status.success(), "{program} {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    out.stdout
}
