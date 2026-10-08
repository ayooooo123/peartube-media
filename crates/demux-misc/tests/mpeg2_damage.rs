//! Frame-exact concealment through the registry and TS demuxer. The fixed
//! mutation corpus bounds bytes, geometry, packet count and receive calls.
use oxideav_core::{CodecId, CodecParameters, Decoder, Error, Frame, MediaType, Packet, TimeBase};
use std::{path::{Path, PathBuf}, process::Command};

fn directory(name: &str) -> PathBuf {
    let path = PathBuf::from(std::env::var_os("CARGO_TARGET_DIR").expect("absolute CARGO_TARGET_DIR"))
        .join("evidence").join(name);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn ffmpeg(tool: &Path, args: &[&str], path: &Path) {
    let out = Command::new(tool).args(["-nostdin", "-v", "error", "-y"])
        .args(args).arg(path).output().unwrap();
    assert!(out.status.success(), "{}: {}", path.display(), String::from_utf8_lossy(&out.stderr));
}

fn synthetic(dir: &Path) -> Vec<u8> {
    let path = dir.join("clean.m2v");
    ffmpeg(&refcheck::system_ffmpeg(), &[
        "-f", "lavfi", "-i", "testsrc2=size=96x64:rate=25:duration=0.48",
        "-c:v", "mpeg2video", "-threads", "1", "-g", "6", "-bf", "2", "-q:v", "4", "-f", "mpeg2video",
    ], &path);
    std::fs::read(path).unwrap()
}

fn codes(data: &[u8]) -> Vec<(usize, u8)> {
    data.windows(4).enumerate().filter(|(_, w)| w[..3] == [0, 0, 1])
        .map(|(i, w)| (i, w[3])).collect()
}

fn expected(path: &Path) -> Vec<String> {
    let args = refcheck::ffmpeg_video_md5_args(path, "0:v:0", "yuv420p", &["-idct", "simple"]);
    let out = Command::new(refcheck::pinned_ffmpeg()).args(["-v", "error", "-nostdin"]).args(args).output().unwrap();
    assert!(out.status.success(), "{}: {}", path.display(), String::from_utf8_lossy(&out.stderr));
    refcheck::parse_framemd5(std::str::from_utf8(&out.stdout).unwrap())
}

#[test]
fn damaged_i_p_b_transport_streams_equal_every_ffmpeg_frame() {
    let dir = directory("mpeg2-damage");
    let clean = synthetic(&dir);
    let starts = codes(&clean);
    let mut cases = vec![("clean".to_string(), clean.clone())];
    for (kind, label) in [(1, "i"), (2, "p"), (3, "b")] {
        // Prefer the later I-picture so both spatial and temporal decisions
        // have a prior reference. Also exercise an opening damaged I below.
        let pictures: Vec<_> = starts.iter().filter(|&&(at, code)| code == 0 && (clean[at + 5] >> 3) & 7 == kind)
            .map(|&(at, _)| at).collect();
        let at = if kind == 1 { *pictures.last().unwrap() } else { pictures[0] };
        let end = starts.iter().find(|&&(i, code)| i > at && matches!(code, 0 | 0xb3 | 0xb8 | 0xb7))
            .map_or(clean.len(), |&(i, _)| i);
        let slices: Vec<_> = starts.iter().filter(|&&(i, code)| i > at && i < end && (1..=0xaf).contains(&code))
            .map(|&(i, _)| i).collect();
        assert_eq!(slices.len(), 4, "one slice per macroblock row");
        let mut corrupt = clean.clone();
        corrupt[slices[2] + 4] &= 7; // qscale == 0: rejected slice, next row remains usable.
        cases.push((format!("corrupt-{label}"), corrupt));
        let cut = slices[2] + (slices[3] - slices[2]) / 2;
        cases.push((format!("truncated-{label}"), clean[..cut].to_vec()));
    }
    let first_slice = starts.iter().find(|&&(_, c)| (1..=0xaf).contains(&c)).unwrap().0;
    let mut opening = clean.clone();
    opening[first_slice + 4] &= 7;
    cases.push(("corrupt-opening-i".to_string(), opening));
    let mut failures = Vec::new();
    for (name, bytes) in cases {
        let es = dir.join(format!("{name}.m2v"));
        std::fs::write(&es, bytes).unwrap();
        let ts = dir.join(format!("{name}.ts"));
        ffmpeg(&refcheck::pinned_ffmpeg(), &["-fflags", "+genpts", "-i", es.to_str().unwrap(), "-c:v", "copy", "-f", "mpegts"], &ts);
        let decoded = refcheck::decode(&ts, &[codecs::register_all], MediaType::Video, 0);
        let ours: Vec<_> = decoded.frames.iter().map(|frame| {
            let Frame::Video(video) = frame else { panic!("video") };
            refcheck::md5_hex(&refcheck::pack(video, &[(96, 64), (48, 32), (48, 32)]))
        }).collect();
        let theirs = expected(&ts);
        assert!(!theirs.is_empty(), "{name}: the oracle must decode damaged input");
        if name == "clean" || name.starts_with("corrupt") {
            assert_eq!(theirs.len(), 12, "{name}: all pictures retained");
        }
        let differing: Vec<_> = ours.iter().zip(&theirs).enumerate().filter_map(|(i, (a, b))| (a != b).then_some(i)).collect();
        eprintln!("{name}: frames {}/{}; differing {differing:?}", ours.len(), theirs.len());
        if ours != theirs {
            let raw: Vec<u8> = decoded.frames.iter().flat_map(|frame| {
                let Frame::Video(video) = frame else { unreachable!() };
                refcheck::pack(video, &[(96, 64), (48, 32), (48, 32)])
            }).collect();
            std::fs::write(dir.join(format!("{name}.actual.yuv")), raw).unwrap();
            failures.push(name);
        }
    }
    assert!(failures.is_empty(), "complete-frame oracle mismatches: {failures:?}");
}

fn drain_bounded(decoder: &mut dyn Decoder) -> usize {
    let mut count = 0;
    for _ in 0..32 {
        match decoder.receive_frame() {
            Ok(Frame::Video(_)) => count += 1,
            Ok(_) => panic!("not video"),
            Err(_) => return count,
        }
    }
    panic!("decoder failed to exhaust a <=12-picture input");
}

#[test]
fn two_thousand_fixed_seed_damaged_inputs_terminate_without_panics() {
    let dir = directory("mpeg2-mutations");
    let clean = synthetic(&dir);
    let starts = codes(&clean);
    let pictures: Vec<_> = starts.iter().filter(|&&(_, c)| c == 0).map(|&(i, _)| i).collect();
    let second = pictures[1];
    let slice_data: Vec<_> = starts.windows(2).filter(|w| w[0].0 > second && (1..=0xaf).contains(&w[0].1))
        .map(|w| (w[0].0 + 4, w[1].0)).filter(|&(a,b)| b > a).collect();
    let context = codecs::context();
    let params = CodecParameters::video(CodecId::new("mpeg2video"));
    let mut decoder = context.codecs.first_decoder(&params).unwrap();
    let mut seed = 0x7634_a821u32;
    let mut pictures_returned = 0;
    for iteration in 0..2000 {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        let mut bytes = clean.clone();
        let (from, to) = slice_data[seed as usize % slice_data.len()];
        let position = from + (seed.rotate_left(13) as usize % (to - from));
        match iteration % 4 {
            0 => bytes.truncate(position),
            1 => bytes[position] ^= 1 << ((seed >> 24) & 7),
            2 => bytes[position..to.min(position + 17)].fill(0),
            _ => bytes[position..to.min(position + 17)].fill(0xff),
        }
        for chunk in bytes.chunks(997) {
            if decoder.send_packet(&Packet::new(0, TimeBase::new(1, 25), chunk.to_vec())).is_err() { break; }
            pictures_returned += drain_bounded(decoder.as_mut());
        }
        let _ = decoder.flush();
        pictures_returned += drain_bounded(decoder.as_mut());
        decoder.reset().unwrap();
        assert!(matches!(decoder.receive_frame(), Err(Error::NeedMore)));
    }
    assert!(pictures_returned >= 2000, "mutations must reach picture reconstruction, not just header rejection");
    eprintln!("2000 mutations; seed 0x7634a821; {pictures_returned} pictures returned; reset drained every epoch");
}
