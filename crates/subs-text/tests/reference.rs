mod common;

use std::path::Path;

use common::{ffmpeg_cues, format_srt_time, visible_text};
use oxideav_core::{Frame, MediaType, Packet, TimeBase};
use refcheck::fate;

fn ffmpeg_srt_cues(path: &Path) -> Vec<(String, String)> {
    ffmpeg_cues(path, &[], "srt")
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
fn test_sami_reference() {
    let fate_samples = [
        "sub/SAMI_capability_tester.smi",
        "sub/SAMI_multilang_tweak_tester.smi",
    ];

    for sample_rel in fate_samples {
        let sample = fate(sample_rel);
        let ffmpeg_cues = ffmpeg_srt_cues(&sample);

        let decoded = refcheck::decode(
            &sample,
            &[subs_text::register],
            MediaType::Subtitle,
            0,
        );

        assert_eq!(
            decoded.frames.len(),
            ffmpeg_cues.len(),
            "[{}] cue count mismatch: decoded {} vs ffmpeg {}",
            sample_rel,
            decoded.frames.len(),
            ffmpeg_cues.len()
        );

        for (i, frame) in decoded.frames.iter().enumerate() {
            if let Frame::Subtitle(cue) = frame {
                let timing = format!(
                    "{} --> {}",
                    format_srt_time(cue.start_us),
                    format_srt_time(cue.end_us)
                );
                assert_eq!(
                    timing, ffmpeg_cues[i].0,
                    "[{}] cue {} timing mismatch: {} vs {}",
                    sample_rel,
                    i + 1,
                    timing,
                    ffmpeg_cues[i].0
                );

                let body = subs_text::sami::render_srt_body(&cue.segments).trim().to_string();
                assert_eq!(
                    body, ffmpeg_cues[i].1,
                    "[{}] cue {} body mismatch:\n  Act: {:?}\n  Exp: {:?}",
                    sample_rel,
                    i + 1,
                    body,
                    ffmpeg_cues[i].1
                );
            } else {
                panic!("expected Subtitle frame, got {:?}", frame);
            }
        }
    }
}

fn assert_standalone_cues(sample: &str, options: &[&str], encoder: &str) {
    let sample = fate(sample);
    let reference = ffmpeg_cues(&sample, options, encoder);
    assert!(!reference.is_empty(), "FFmpeg must produce reference cues");
    let decoded = refcheck::decode(&sample, &[codecs::register_all], MediaType::Subtitle, 0);
    let actual: Vec<_> = decoded.frames.iter().map(|frame| {
        let Frame::Subtitle(cue) = frame else { panic!("expected subtitle frame") };
        let body = if encoder == "text" {
            let mut text = String::new();
            visible_text(&cue.segments, &mut text);
            text
        } else {
            oxideav_subtitle::srt::render_segments(&cue.segments)
        };
        (
            format!("{} --> {}", format_srt_time(cue.start_us), format_srt_time(cue.end_us)),
            body.trim().to_string(),
        )
    }).collect();
    assert_eq!(actual.len(), reference.len(), "cue count for {}", sample.display());
    for (index, (actual, reference)) in actual.iter().zip(&reference).enumerate() {
        assert_eq!(actual, reference, "cue {index} text and timing for {}", sample.display());
    }
}

#[test]
fn test_subviewer1_reference() {
    assert_standalone_cues(
        "sub/SubViewer1_capability_tester.sub",
        &["-sub_charenc", "windows-1250"],
        "srt",
    );
}

#[test]
fn test_vplayer_reference() {
    assert_standalone_cues("sub/VPlayer_capability_tester.txt", &[], "srt");
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
        ("sami", b"<SAMI><BODY><SYNC Start=100><P Class=ENUSCC ID=Source>Speaker<P Class=ENUSCC>Hello <B>World</B></BODY></SAMI>"),
        ("subviewer1", b"[DELAY]\n4\n[00:03:41]\nFirst line|second line\n"),
        ("vplayer", b"0:00:01.50:Hello|world\n"),
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
                    "sami" => {
                        let mut dec = subs_text::sami::make_decoder(&params).unwrap();
                        let _ = dec.send_packet(&packet);
                        let _ = dec.receive_frame();
                    }
                    "subviewer1" => {
                        let mut dec = subs_text::subviewer1::make_decoder(&params).unwrap();
                        let _ = dec.send_packet(&packet);
                        let _ = dec.receive_frame();
                    }
                    "vplayer" => {
                        let mut dec = subs_text::vplayer::make_decoder(&params).unwrap();
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

// SAMI, SubViewer1 and VPlayer have complete reference cases above.
// Compare visible text, not incidental SRT tag serialization (for example
// uppercase versus lowercase color hex). This does not assert style parity.
macro_rules! standalone_reference {
    ($name:ident, $sample:literal) => {
        #[test]
        fn $name() {
            assert_standalone_cues($sample, &[], "text");
        }
    };
}

standalone_reference!(standalone_subrip, "sub/SubRip_capability_tester.srt");
standalone_reference!(standalone_badsyntax, "sub/badsyntax.srt");
standalone_reference!(standalone_empty_events, "sub/empty-events-2167.srt");
standalone_reference!(standalone_madness, "sub/madness.srt");
standalone_reference!(standalone_rrn, "sub/ticket5032-rrn.srt");
standalone_reference!(standalone_microdvd, "sub/MicroDVD_capability_tester.sub");
standalone_reference!(standalone_microdvd_srt, "sub/MicroDVD_capability_tester.srt");
standalone_reference!(standalone_subviewer, "sub/SubViewer_capability_tester.sub");
standalone_reference!(standalone_mpl2, "sub/MPL2_capability_tester.txt");
standalone_reference!(standalone_webvtt, "sub/WebVTT_capability_tester.vtt");
standalone_reference!(standalone_webvtt_extended, "sub/WebVTT_extended_tester.vtt");
standalone_reference!(standalone_ass, "sub/1ededcbd7b.ass");
standalone_reference!(standalone_ssa, "sub/a9-misc.ssa");
