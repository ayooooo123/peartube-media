use std::path::Path;
use std::process::Command;

use oxideav_core::{Frame, MediaType, Packet, TimeBase};
use refcheck::fate;

fn format_srt_time(us: i64) -> String {
    let ms = (us / 1000).max(0);
    let s = ms / 1000;
    let m = s / 60;
    let h = m / 60;
    format!(
        "{:02}:{:02}:{:02},{:03}",
        h,
        m % 60,
        s % 60,
        ms % 1000
    )
}

fn ffmpeg_srt_cues(path: &Path) -> Vec<(String, String)> {
    let output = Command::new("ffmpeg")
        .args([
            "-i",
            path.to_str().unwrap(),
            "-map",
            "0:s:0",
            "-c:s",
            "srt",
            "-f",
            "srt",
            "-",
        ])
        .output()
        .expect("run ffmpeg");
    assert!(output.status.success(), "ffmpeg failed: {:?}", output);
    let text = String::from_utf8_lossy(&output.stdout);

    let mut cues = Vec::new();
    let blocks: Vec<&str> = text.split("\n\n").collect();
    for block in blocks {
        let lines: Vec<&str> = block.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        if lines.len() >= 2 && lines[1].contains("-->") {
            let timing = lines[1].to_string();
            let body = lines[2..].join("\n");
            cues.push((timing, body));
        }
    }
    cues
}

#[test]
fn test_mov_text_reference() {
    let sample = fate("sub/MovText_capability_tester.mp4");
    let decoded = refcheck::decode(
        &sample,
        &[subs_text::register, oxideav_mp4::register],
        MediaType::Subtitle,
        0,
    );

    assert_eq!(decoded.frames.len(), 3, "expected 3 decoded cues");

    let ffmpeg_cues = ffmpeg_srt_cues(&sample);
    assert_eq!(ffmpeg_cues.len(), 3, "expected 3 ffmpeg cues");

    for (i, frame) in decoded.frames.iter().enumerate() {
        if let Frame::Subtitle(cue) = frame {
            let timing = format!(
                "{} --> {}",
                format_srt_time(cue.start_us),
                format_srt_time(cue.end_us)
            );
            assert_eq!(
                timing, ffmpeg_cues[i].0,
                "cue {} timing mismatch: {} vs {}",
                i + 1,
                timing,
                ffmpeg_cues[i].0
            );

            let body = oxideav_subtitle::srt::render_segments(&cue.segments);
            assert_eq!(
                body, ffmpeg_cues[i].1,
                "cue {} body mismatch: {:?} vs {:?}",
                i + 1,
                body,
                ffmpeg_cues[i].1
            );
        } else {
            panic!("expected Subtitle frame, got {:?}", frame);
        }
    }
}

#[test]
fn test_kate_reference() {
    let sample = fate("ogg-kate/kate-subtitles.ogg");
    let decoded = refcheck::decode(
        &sample,
        &[subs_text::register, oxideav_ogg::register],
        MediaType::Unknown,
        0,
    );

    assert_eq!(decoded.frames.len(), 2, "expected 2 decoded Kate cues");

    if let Frame::Subtitle(cue) = &decoded.frames[0] {
        assert_eq!(cue.start_us, 500_000, "cue 1 start mismatch");
        assert_eq!(cue.end_us, 2_000_000, "cue 1 end mismatch");
        let text = oxideav_subtitle::srt::render_segments(&cue.segments);
        assert_eq!(text, "Hello from Kate\nfirst line");
    } else {
        panic!("expected Subtitle frame");
    }

    if let Frame::Subtitle(cue) = &decoded.frames[1] {
        assert_eq!(cue.start_us, 2_500_000, "cue 2 start mismatch");
        assert_eq!(cue.end_us, 5_000_000, "cue 2 end mismatch");
        let text = oxideav_subtitle::srt::render_segments(&cue.segments);
        assert_eq!(text, "Second event");
    } else {
        panic!("expected Subtitle frame");
    }
}

#[test]
fn test_usf_decode_sample() {
    let usf_xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE USFSubtitles PUBLIC "-//USF//DTD Subtitles 1.0/EN" "http://ultravcs.sourceforge.net/usf/usf.dtd" []>
<USFSubtitles version="1.0">
  <metadata><title>corpus</title><date>2026</date></metadata>
  <styles><style name="Default"><fontstyle face="Arial" size="20"/></style></styles>
  <subtitles>
    <subtitle start="0.500" stop="2.000"><text>Hello from the USF corpus.</text></subtitle>
    <subtitle start="2.500" stop="4.000"><text>USF cue two.</text></subtitle>
    <subtitle start="4.500" stop="5.500"><text>USF cue three.</text></subtitle>
  </subtitles>
</USFSubtitles>"#;

    let params = oxideav_core::CodecParameters::subtitle(oxideav_core::CodecId::new("usf"));

    let mut decoder = subs_text::usf::make_decoder(&params).expect("make usf decoder");
    let packet = Packet::new(0, TimeBase::new(1, 1000), usf_xml.to_vec());
    decoder.send_packet(&packet).expect("send packet");

    let mut cues = Vec::new();
    while let Ok(frame) = decoder.receive_frame() {
        if let Frame::Subtitle(cue) = frame {
            cues.push(cue);
        }
    }

    assert_eq!(cues.len(), 3, "expected 3 USF cues");
    assert_eq!(cues[0].start_us, 500_000);
    assert_eq!(cues[0].end_us, 2_000_000);
    assert_eq!(
        oxideav_subtitle::srt::render_segments(&cues[0].segments),
        "<font face=\"Arial\" size=\"20\">Hello from the USF corpus.</font>"
    );

    assert_eq!(cues[1].start_us, 2_500_000);
    assert_eq!(cues[1].end_us, 4_000_000);
    assert_eq!(
        oxideav_subtitle::srt::render_segments(&cues[1].segments),
        "<font face=\"Arial\" size=\"20\">USF cue two.</font>"
    );

    assert_eq!(cues[2].start_us, 4_500_000);
    assert_eq!(cues[2].end_us, 5_500_000);
    assert_eq!(
        oxideav_subtitle::srt::render_segments(&cues[2].segments),
        "<font face=\"Arial\" size=\"20\">USF cue three.</font>"
    );
}

#[test]
fn test_cmml_decode_sample() {
    let cmml_data = br#"<cmml>
<clip id="intro" start="0.000" end="2.500">
 <title>Introduction</title>
 <desc>This is the introduction clip.</desc>
</clip>
<clip id="middle" start="npt:3.5" end="npt:5.0">
 <title>Middle section</title>
 <desc>Continuing with the video.</desc>
</clip>
<clip id="closing" start="00:00:06.000" end="00:00:08.500">
 <desc>Closing remarks.</desc>
</clip>
</cmml>"#;

    let params = oxideav_core::CodecParameters::subtitle(oxideav_core::CodecId::new("cmml"));

    let mut decoder = subs_text::cmml::make_decoder(&params).expect("make cmml decoder");
    let packet = Packet::new(0, TimeBase::new(1, 1000), cmml_data.to_vec());
    decoder.send_packet(&packet).expect("send packet");

    let mut cues = Vec::new();
    while let Ok(frame) = decoder.receive_frame() {
        if let Frame::Subtitle(cue) = frame {
            cues.push(cue);
        }
    }

    assert_eq!(cues.len(), 3, "expected 3 CMML cues");
    assert_eq!(cues[0].start_us, 0);
    assert_eq!(cues[0].end_us, 2_500_000);
    assert_eq!(
        oxideav_subtitle::srt::render_segments(&cues[0].segments),
        "Introduction\nThis is the introduction clip."
    );

    assert_eq!(cues[1].start_us, 3_500_000);
    assert_eq!(cues[1].end_us, 5_000_000);
    assert_eq!(
        oxideav_subtitle::srt::render_segments(&cues[1].segments),
        "Middle section\nContinuing with the video."
    );

    assert_eq!(cues[2].start_us, 6_000_000);
    assert_eq!(cues[2].end_us, 8_500_000);
    assert_eq!(
        oxideav_subtitle::srt::render_segments(&cues[2].segments),
        "Closing remarks."
    );
}

#[test]
fn test_untrusted_input_robustness() {
    struct Rng(u64);
    impl Rng {
        fn next_u32(&mut self) -> u32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 as u32
        }
        fn next_range(&mut self, max: usize) -> usize {
            if max == 0 {
                0
            } else {
                (self.next_u32() as usize) % max
            }
        }
    }

    let mut rng = Rng(0xDEAD_BEEF_CAFE_BABE);

    let test_seeds: &[(&str, &[u8])] = &[
        ("mov_text", b"\x00\x09Hello CSS\x00\x00\x00\x14styl\x00\x01\x00\x00\x00\x05\x00\x01\x01\x12\xff\x00\x00\xff"),
        ("usf", b"<USFSubtitles><subtitles><subtitle start=\"1.0\" stop=\"2.0\"><text><b>Test</b></text></subtitle></subtitles></USFSubtitles>"),
        ("cmml", b"<cmml><clip start=\"1.0\" end=\"2.0\"><title>T</title><desc>D</desc></clip></cmml>"),
        ("kate", b"\x00\x00\x01\x00\x00\x00\x00\x00\x00\x00\x02\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x04\x00\x00\x00Kate\x00"),
    ];

    for &(codec, seed_data) in test_seeds {
        for _ in 0..2500 {
            let mut mutated = seed_data.to_vec();
            let mode = rng.next_range(4);
            match mode {
                0 => {
                    // Truncation
                    let len = rng.next_range(mutated.len() + 1);
                    mutated.truncate(len);
                }
                1 => {
                    // Bit flip
                    if !mutated.is_empty() {
                        let idx = rng.next_range(mutated.len());
                        let bit = rng.next_range(8);
                        mutated[idx] ^= 1 << bit;
                    }
                }
                2 => {
                    // Random byte replacement
                    if !mutated.is_empty() {
                        let idx = rng.next_range(mutated.len());
                        mutated[idx] = rng.next_u32() as u8;
                    }
                }
                _ => {
                    // Truncate + append random bytes
                    let len = rng.next_range(mutated.len() + 1);
                    mutated.truncate(len);
                    let add = rng.next_range(16);
                    for _ in 0..add {
                        mutated.push(rng.next_u32() as u8);
                    }
                }
            }

            let params = oxideav_core::CodecParameters::subtitle(oxideav_core::CodecId::new(codec));
            let packet = Packet::new(0, TimeBase::new(1, 1000), mutated);

            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                match codec {
                    "mov_text" => {
                        let mut dec = subs_text::mov_text::make_decoder(&params).unwrap();
                        let _ = dec.send_packet(&packet);
                        let _ = dec.receive_frame();
                    }
                    "usf" => {
                        let mut dec = subs_text::usf::make_decoder(&params).unwrap();
                        let _ = dec.send_packet(&packet);
                        let _ = dec.receive_frame();
                    }
                    "cmml" => {
                        let mut dec = subs_text::cmml::make_decoder(&params).unwrap();
                        let _ = dec.send_packet(&packet);
                        let _ = dec.receive_frame();
                    }
                    "kate" => {
                        let mut dec = subs_text::kate::make_decoder(&params).unwrap();
                        let _ = dec.send_packet(&packet);
                        let _ = dec.receive_frame();
                    }
                    _ => unreachable!(),
                }
            }));
            assert!(res.is_ok(), "fuzzing {} panicked!", codec);
        }
    }
}

#[test]
fn test_verify_oxideav_standalone_subtitles() {
    let fate_samples = [
        ("SubRip", "sub/SubRip_capability_tester.srt"),
        ("SubRip", "sub/badsyntax.srt"),
        ("SubRip", "sub/empty-events-2167.srt"),
        ("SubRip", "sub/madness.srt"),
        ("SubRip", "sub/ticket5032-rrn.srt"),
        ("MicroDVD", "sub/MicroDVD_capability_tester.sub"),
        ("MicroDVD", "sub/MicroDVD_capability_tester.srt"),
        ("SubViewer", "sub/SubViewer_capability_tester.sub"),
        ("SubViewer", "sub/SubViewer1_capability_tester.sub"),
        ("SAMI", "sub/SAMI_capability_tester.smi"),
        ("SAMI", "sub/SAMI_multilang_tweak_tester.smi"),
        ("VPlayer", "sub/VPlayer_capability_tester.txt"),
        ("MPL2", "sub/MPL2_capability_tester.txt"),
        ("WebVTT", "sub/WebVTT_capability_tester.vtt"),
        ("WebVTT", "sub/WebVTT_extended_tester.vtt"),
        ("SSA/ASS", "sub/1ededcbd7b.ass"),
        ("SSA/ASS", "sub/a9-misc.ssa"),
    ];

    println!("\n=== OxideAV Standalone Subtitle Verification ===");
    for (format, sample_rel) in fate_samples {
        let sample_path = fate(sample_rel);
        let registrars: &[refcheck::Registrar] = &[
            oxideav_subtitle::register,
            oxideav_ass::register,
        ];

        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            refcheck::decode(&sample_path, registrars, MediaType::Subtitle, 0)
        }));

        match res {
            Ok(decoded) => {
                println!(
                    "PASS: [{}] {} => {} cues decoded (params: {:?})",
                    format,
                    sample_rel,
                    decoded.frames.len(),
                    decoded.params.codec_id
                );
            }
            Err(e) => {
                let msg = if let Some(s) = e.downcast_ref::<&str>() {
                    s.to_string()
                } else if let Some(s) = e.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "panic".to_string()
                };
                println!("FAIL: [{}] {} => {}", format, sample_rel, msg);
            }
        }
    }
    println!("================================================\n");
}
