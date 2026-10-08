use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;

use oxideav_core::{Demuxer, MediaType, TimeBase};
use refcheck::{decode, fate, pinned_ffmpeg};

fn pinned_ffprobe() -> PathBuf {
    pinned_ffmpeg().parent().unwrap().join("ffprobe")
}

fn run_pinned_ffmpeg(path: &Path) -> Vec<u8> {
    let out = Command::new(pinned_ffmpeg())
        .args([
            "-v",
            "error",
            "-nostdin",
            "-cpuflags",
            "0",
            "-i",
            path.to_str().unwrap(),
            "-map",
            "0:a:0",
            "-f",
            "s16le",
            "-c:a",
            "pcm_s16le",
            "-",
        ])
        .output()
        .expect("pinned ffmpeg runs");
    assert!(out.status.success(), "ffmpeg error: {}", String::from_utf8_lossy(&out.stderr));
    out.stdout
}

struct ProbePacket {
    pts: i64,
    duration: i64,
    size: usize,
}

fn run_pinned_ffprobe_packets(path: &Path) -> Vec<ProbePacket> {
    let out = Command::new(pinned_ffprobe())
        .args([
            "-v",
            "error",
            "-show_packets",
            "-select_streams",
            "a:0",
            path.to_str().unwrap(),
        ])
        .output()
        .expect("pinned ffprobe runs");
    assert!(out.status.success(), "ffprobe error: {}", String::from_utf8_lossy(&out.stderr));

    let text = String::from_utf8(out.stdout).unwrap();
    let mut packets = Vec::new();
    let mut pts = 0i64;
    let mut duration = 0i64;
    let mut size = 0usize;
    let mut in_packet = false;

    for line in text.lines() {
        let line = line.trim();
        if line == "[PACKET]" {
            in_packet = true;
        } else if line == "[/PACKET]" {
            if in_packet {
                packets.push(ProbePacket { pts, duration, size });
            }
            in_packet = false;
        } else if in_packet {
            if let Some((k, v)) = line.split_once('=') {
                match k {
                    "pts" => pts = v.parse().unwrap_or(0),
                    "duration" => duration = v.parse().unwrap_or(0),
                    "size" => size = v.parse().unwrap_or(0),
                    _ => {}
                }
            }
        }
    }

    packets
}

fn run_pinned_ffprobe_seek_pts(path: &Path, interval: &str) -> Vec<i64> {
    let out = Command::new(pinned_ffprobe())
        .args([
            "-v",
            "error",
            "-read_intervals",
            interval,
            "-select_streams",
            "a:0",
            "-show_entries",
            "packet=pts",
            path.to_str().unwrap(),
        ])
        .output()
        .expect("pinned ffprobe runs");
    assert!(out.status.success(), "ffprobe error: {}", String::from_utf8_lossy(&out.stderr));

    let text = String::from_utf8(out.stdout).unwrap();
    let mut pts_list = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("pts=") {
            if let Ok(p) = v.parse::<i64>() {
                pts_list.push(p);
            }
        }
    }
    pts_list
}

fn open_demuxer(path: &Path) -> Box<dyn Demuxer> {
    let file = File::open(path).expect("open file");
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let mut head = [0u8; 1024];
    let mut f = File::open(path).expect("open file");
    use std::io::Read;
    let n = f.read(&mut head).unwrap_or(0);
    let probe = oxideav_core::ProbeData {
        buf: &head[..n],
        ext: Some(ext),
    };

    if codec_musepack::mpc8_probe(&probe) > 0 {
        codec_musepack::open_mpc8(Box::new(file), &oxideav_core::NullCodecResolver).expect("open mpc8")
    } else if codec_musepack::mpc_probe(&probe) > 0 {
        codec_musepack::open_mpc(Box::new(file), &oxideav_core::NullCodecResolver).expect("open mpc")
    } else {
        panic!("Neither demuxer claimed {}", path.display());
    }
}

fn interleaved_s16le(decoded: &refcheck::Decoded) -> Vec<u8> {
    let mut out = Vec::new();
    let channels = decoded.audio_format.map_or(2, |f| f.channels as usize);
    for frame in &decoded.frames {
        let oxideav_core::Frame::Audio(a) = frame else { continue };
        let n = a.samples as usize;
        assert!(a.data.len() >= channels, "planar audio data plane count");
        for i in 0..n {
            for c in 0..channels {
                let plane = &a.data[c];
                out.push(plane[i * 2]);
                out.push(plane[i * 2 + 1]);
            }
        }
    }
    out
}

#[test]
fn test_mpc7_decode_byte_exact() {
    let path = fate("musepack/inside-mp7.mpc");
    let reference_pcm = run_pinned_ffmpeg(&path);
    let decoded = decode(&path, &[codec_musepack::register], MediaType::Audio, 0);
    let ours_pcm = interleaved_s16le(&decoded);
    assert_eq!(ours_pcm.len(), reference_pcm.len(), "PCM byte count must match");
    assert_eq!(ours_pcm, reference_pcm, "PCM bytes must be bit-exact with FFmpeg");
}

#[test]
fn test_mpc8_decode_byte_exact() {
    let path = fate("musepack/inside-mp8.mpc");
    let reference_pcm = run_pinned_ffmpeg(&path);
    let decoded = decode(&path, &[codec_musepack::register], MediaType::Audio, 0);

    let ours_pcm = interleaved_s16le(&decoded);
    assert_eq!(ours_pcm.len(), reference_pcm.len(), "PCM byte count must match");
    for i in 0..ours_pcm.len().min(reference_pcm.len()) / 2 {
        let ours = i16::from_le_bytes([ours_pcm[2 * i], ours_pcm[2 * i + 1]]);
        let theirs = i16::from_le_bytes([reference_pcm[2 * i], reference_pcm[2 * i + 1]]);
        if ours != theirs {
            panic!("First mismatch at sample {i} (frame {}, sub {}): ours {ours} vs ref {theirs}", i / 2 / 1152, (i / 2) % 1152);
        }
    }
    assert_eq!(ours_pcm, reference_pcm, "PCM bytes must be bit-exact with FFmpeg");
}

#[test]
fn test_mpc7_demuxer_packets() {
    let path = fate("musepack/inside-mp7.mpc");
    let expected = run_pinned_ffprobe_packets(&path);
    let mut demuxer = open_demuxer(&path);

    let mut actual = Vec::new();
    while let Ok(pkt) = demuxer.next_packet() {
        actual.push(ProbePacket {
            pts: pkt.pts.unwrap_or(0),
            duration: pkt.duration.unwrap_or(0),
            size: pkt.data.len(),
        });
    }

    assert_eq!(actual.len(), expected.len(), "Packet count mismatch");
    for (i, (act, exp)) in actual.iter().zip(&expected).enumerate() {
        assert_eq!(act.pts, exp.pts, "Packet {i} pts mismatch");
        assert_eq!(act.duration, exp.duration, "Packet {i} duration mismatch");
        assert_eq!(act.size, exp.size, "Packet {i} size mismatch");
    }
}

#[test]
fn test_mpc8_demuxer_packets() {
    let path = fate("musepack/inside-mp8.mpc");
    let expected = run_pinned_ffprobe_packets(&path);
    let mut demuxer = open_demuxer(&path);

    let mut actual = Vec::new();
    while let Ok(pkt) = demuxer.next_packet() {
        actual.push(ProbePacket {
            pts: pkt.pts.unwrap_or(0),
            duration: pkt.duration.unwrap_or(0),
            size: pkt.data.len(),
        });
    }

    assert_eq!(actual.len(), expected.len(), "Packet count mismatch");
    for (i, (act, exp)) in actual.iter().zip(&expected).enumerate() {
        assert_eq!(act.pts, exp.pts, "Packet {i} pts mismatch");
        assert_eq!(act.duration, exp.duration, "Packet {i} duration mismatch");
        assert_eq!(act.size, exp.size, "Packet {i} size mismatch");
    }
}

#[test]
fn test_mpc8_seek() {
    let path = fate("musepack/inside-mp8.mpc");
    let expected_pts = run_pinned_ffprobe_seek_pts(&path, "8.4%+#3");
    assert_eq!(expected_pts.len(), 3, "Expected 3 packets from ffprobe");

    let mut demuxer = open_demuxer(&path);
    let stream = &demuxer.streams()[0];
    let tb = stream.time_base;
    // Rescale 8.4 seconds to stream timebase ticks:
    // (8_400_000 * 1225) / (1_000_000 * 2048) = 5
    let target_pts = TimeBase::new(1, 1_000_000).rescale(8_400_000, tb);
    demuxer.seek_to(0, target_pts).expect("seek_to 8.4s");

    let mut got_pts = Vec::new();
    for _ in 0..3 {
        let pkt = demuxer.next_packet().expect("next packet after seek");
        got_pts.push(pkt.pts.expect("pts present"));
    }

    assert_eq!(got_pts, expected_pts, "Next 3 packets pts must match ffprobe");
}

#[test]
fn test_mpc7_seek() {
    let path = fate("musepack/inside-mp7.mpc");
    let expected_pts = run_pinned_ffprobe_seek_pts(&path, "5.0%+#3");
    assert_eq!(expected_pts.len(), 3, "Expected 3 packets from ffprobe");

    let mut demuxer = open_demuxer(&path);
    let stream = &demuxer.streams()[0];
    let tb = stream.time_base;
    // Rescale 5.0 seconds to stream timebase ticks:
    // (5_000_000 * 1225) / (1_000_000 * 32) = 191
    let target_pts = TimeBase::new(1, 1_000_000).rescale(5_000_000, tb);
    demuxer.seek_to(0, target_pts).expect("seek_to 5.0s");

    let mut got_pts = Vec::new();
    for _ in 0..3 {
        let pkt = demuxer.next_packet().expect("next packet after seek");
        got_pts.push(pkt.pts.expect("pts present"));
    }

    assert_eq!(got_pts, expected_pts, "Next 3 packets pts must match ffprobe");
}

#[test]
fn regression_p1_sh_payload_overflow() {
    // 16-byte file with SH varlen 0 (payload_size < 0)
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"MPCKSH\x00"); // 4-byte magic + 2-byte tag + 1-byte varlen 0
    bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // CRC 4 bytes
    bytes.push(0x08); // ver 8
    bytes.extend_from_slice(&[0x00, 0x00]); // varlen samples 0, varlen silence 0
    bytes.extend_from_slice(&[0x1b, 0x1b]); // extradata 2 bytes
    assert_eq!(bytes.len(), 16);

    let cursor = Box::new(std::io::Cursor::new(bytes));
    let res = codec_musepack::open_mpc8(cursor, &oxideav_core::NullCodecResolver);
    assert!(res.is_err(), "must reject invalid SH payload size without panic");
}

#[test]
fn regression_p1_seek_table_overflow() {
    // Flipping bit 0 of ST payload at offset 243293 drives ppos[0] >= 2^63
    let path = fate("musepack/inside-mp8.mpc");
    let mut bytes = std::fs::read(&path).expect("read file");
    bytes[243293] ^= 0x80; // set high bit in varlen

    let cursor = Box::new(std::io::Cursor::new(bytes));
    // Must not panic on ppos[0] * 2 in debug mode
    let _ = codec_musepack::open_mpc8(cursor, &oxideav_core::NullCodecResolver);
}

#[test]
fn regression_p2_sv8_per_frame_pts() {
    let path = fate("musepack/inside-mp8.mpc");
    let mut demuxer = open_demuxer(&path);
    let params = demuxer.streams()[0].params.clone();
    let mut decoder = codec_musepack::make_mpc8_decoder(&params).expect("make decoder");

    let pkt = demuxer.next_packet().expect("first packet");
    assert_eq!(pkt.pts, Some(0));
    decoder.send_packet(&pkt).expect("send packet");

    let f1 = decoder.receive_frame().expect("first frame");
    let f2 = decoder.receive_frame().expect("second frame");
    assert_eq!(f1.pts(), Some(0), "first frame must have packet pts");
    assert_eq!(f2.pts(), None, "subsequent frames must have pts None");
}
