use codec_tracker::{READ_FRAMES, Song};
use oxideav_core::{CodecRegistry, ContainerRegistry, Error, Frame};
use std::io::Cursor;

fn pcm_hash(pcm: &[f32]) -> String {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    for sample in pcm { hash.update(sample.to_le_bytes()); }
    format!("{:x}", hash.finalize())
}

#[test]
fn adlib_pitch_volume_retrigger_note_cut_and_seek_match_openmpt() {
    let song = Song::load(include_bytes!("fixtures/fm-events.s3m")).unwrap();
    let output = render(&song, 137);
    // Stock openmpt123 0.8.9: melodic two-operator FM, with volume zero,
    // portamento, retrigger/volume slide, hard pan and delayed note cut.
    assert_eq!(output.len() / 2, 73_920);
    assert!(output[..11_520].iter().step_by(2).any(|v| v.abs() > 0.01));
    assert!(output[1..11_520].iter().step_by(2).all(|&v| v == 0.0));
    assert_eq!(pcm_hash(&output), include_str!("fixtures/fm-events.sha256").trim());
    assert_eq!(render(&song, READ_FRAMES), output);
    for frame in [5760, 40320, 54721] {
        let mut sought = song.renderer_at(frame);
        let mut pcm = [0.0; READ_FRAMES * 2];
        assert_eq!(sought.read(&mut pcm), READ_FRAMES);
        assert_eq!(pcm.as_slice(), &output[frame as usize * 2..frame as usize * 2 + pcm.len()]);
    }
}

#[test]
fn random_effect_seeds_and_seek_match_native_event_mixing() {
    let bytes = include_bytes!("fixtures/random-effects.it");
    // Golden hashes use the unchanged libopenmpt 0.8.9 mixer with its native
    // PRNG seeded through scripts/tracker-seeded-oracle.cpp. No random effect
    // controls are removed. Zero and u32::MAX exercise wrapping seed setup.
    for golden in include_str!("fixtures/random-effects.sha256").lines() {
        let (seed, expected) = golden.split_once(' ').unwrap();
        let song = Song::load(bytes).unwrap().with_seed(seed.parse().unwrap());
        let output = render(&song, 137);
        assert_eq!(output.len() / 2, 50_880);
        assert_eq!(pcm_hash(&output), expected, "seed={seed}");
        assert_eq!(render(&song, READ_FRAMES), output);
        let mut sought = song.renderer_at(12345);
        let mut pcm = [0.0; READ_FRAMES * 2];
        assert_eq!(sought.read(&mut pcm), READ_FRAMES);
        assert_eq!(pcm.as_slice(), &output[24690..24690 + pcm.len()]);
    }
}

#[test]
fn s3m_tracker_version_and_it_unfiltered_samples_match_openmpt() {
    use sha2::{Digest, Sha256};
    for (bytes, expected) in [
        (include_bytes!("fixtures/tone.s3m").as_slice(), "b089395648efde67133b886996ce5d636cd1ab7f94e6fce53064b2d1ae6b68b0"),
        (include_bytes!("fixtures/tone.it").as_slice(), "3b1662e8c330f5fd55fd567c4d727fd87f1bb438a745e9fe251a37e5ef07e74c"),
    ] {
        let pcm = render(&Song::load(bytes).unwrap(), READ_FRAMES);
        assert_eq!(pcm.len() / 2, 16_320);
        let mut hash = Sha256::new();
        for sample in pcm { hash.update(sample.to_le_bytes()); }
        assert_eq!(format!("{:x}", hash.finalize()), expected);
    }
}


#[test]
fn legacy_trackers_render_short_looped_samples() {
    for (format, bytes) in [
        ("mtm", include_bytes!("fixtures/tone.mtm").as_slice()),
        ("669", include_bytes!("fixtures/tone.669").as_slice()),
        ("ult", include_bytes!("fixtures/tone.ult").as_slice()),
        ("stm", include_bytes!("fixtures/tone.stm").as_slice()),
    ] {
        let song = Song::load(bytes).unwrap();
        assert_eq!(song.format().name(), format);
        let output = render(&song, READ_FRAMES);
        assert_eq!(output.len() as u64 / 2, song.frames());
        assert!(output.iter().all(|x| x.is_finite()));
        assert!(output.iter().any(|x| x.abs() > 0.01), "{format}: silent sample");
    }
}
fn module() -> Vec<u8> {
    // Two orders, two rows each, a looping square wave and a volume change.
    let mut d = vec![0u8; 1084 + 2 * 1024 + 32];
    d[..8].copy_from_slice(b"row seek");
    d[42..44].copy_from_slice(&16u16.to_be_bytes());
    d[45] = 64;
    d[48..50].copy_from_slice(&16u16.to_be_bytes());
    d[950] = 2;
    d[952] = 0;
    d[953] = 1;
    d[1080..1084].copy_from_slice(b"M.K.");
    d[1084..1088].copy_from_slice(&[0x01, 0xac, 0x10, 0]);
    d[1084 + 16..1084 + 20].copy_from_slice(&[0, 0, 0x0d, 0]);
    d[2108..2112].copy_from_slice(&[0x01, 0xac, 0x1c, 32]);
    d[2108 + 16..2108 + 20].copy_from_slice(&[0, 0, 0x0d, 0]);
    d[3132..3148].fill(64);
    d[3148..3164].fill(192);
    d
}

fn render(song: &Song, chunk: usize) -> Vec<f32> {
    let mut r = song.renderer_at(0);
    let mut out = Vec::new();
    let mut buf = vec![0.0; chunk * 2];
    loop {
        let n = r.read(&mut buf);
        if n == 0 {
            break;
        }
        assert!(n <= chunk);
        out.extend_from_slice(&buf[..n * 2]);
        assert!(out.len() < 48_000 * 10 * 2, "four-row song must end");
    }
    out
}

#[test]
fn seek_preserves_audio_and_caller_chunk_sizes_do_not_change_ramps() {
    let song = Song::load(&module()).unwrap();
    let expected = render(&song, READ_FRAMES);
    assert_eq!(expected.len() / 2, 27_840, "four 120 ms rows plus 100 ms fade");
    assert_eq!(song.frames(), 27_840);
    assert!(expected.iter().any(|v| v.abs() > 0.01), "sample must be audible");
    assert_eq!(render(&song, 137), expected);
    let row = song.row_start(1, 0).unwrap();
    assert_eq!(row.frame, 11_520);
    assert_eq!(song.row_at(row.frame + 1).unwrap(), row);
    let mut sought = song.seek_order_row(1, 0).unwrap();
    let mut output = vec![0.0; 2048];
    assert_eq!(sought.read(&mut output), 1024);
    assert_eq!(output, expected[row.frame as usize * 2..row.frame as usize * 2 + 2048]);
    assert!(song.seek_order_row(0, 63).is_none());
    let mut end = song.renderer_at(song.frames());
    assert_eq!(end.read(&mut output), 0);
}

#[test]
fn registered_demuxer_seek_resends_song_at_row_boundary() {
    let mut containers = ContainerRegistry::new();
    let mut codecs = CodecRegistry::new();
    codec_tracker::register_containers(&mut containers);
    codec_tracker::register_codecs(&mut codecs);
    let mut demux = containers.open_demuxer("mod", Box::new(Cursor::new(module())), &codecs).unwrap();
    let params = demux.streams()[0].params.clone();
    let mut decoder = codecs.first_decoder(&params).unwrap();
    assert_eq!(demux.seek_to(0, 12_000).unwrap(), 11_520);
    let packet = demux.next_packet().unwrap();
    decoder.send_packet(&packet).unwrap();
    let Frame::Audio(frame) = decoder.receive_frame().unwrap() else { panic!("audio expected") };
    assert_eq!(frame.pts, Some(11_520));
    let expected = render(&Song::load(&module()).unwrap(), READ_FRAMES);
    let bytes: Vec<u8> = expected[23_040..23_040 + 2048].iter().flat_map(|v| v.to_le_bytes()).collect();
    assert_eq!(frame.data[0], bytes);
    assert!(matches!(demux.next_packet(), Err(Error::Eof)));
    decoder.reset().unwrap();
    assert!(matches!(decoder.receive_frame(), Err(Error::NeedMore)));
}

#[test]
fn malformed_sizes_and_pattern_bytes_are_bounded() {
    let original = module();
    let mut state = 0x10_08_2026u32;
    for iteration in 0..384 {
        let mut damaged = original.clone();
        for _ in 0..(iteration % 8 + 1) {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let at = state as usize % damaged.len();
            damaged[at] ^= (state >> 24) as u8;
        }
        if iteration % 8 == 0 {
            damaged.truncate(state as usize % damaged.len());
        }
        if let Some(song) = Song::load(&damaged) {
            assert!(song.frames() <= 48_000 * 7200 + 4800 + 120_000);
            let mut renderer = song.renderer_at(0);
            let mut pcm = [0.0; READ_FRAMES * 2];
            let n = renderer.read(&mut pcm);
            assert!(pcm[..n * 2].iter().all(|v| v.is_finite()));
        }
    }
}

/// External corpus is hash-pinned by scripts/check-tracker.py before use.
#[test]
#[ignore = "requires TRACKER_CORPUS; run after the hash-pinned oracle comparison"]
fn corpus_mutations() {
    let root = std::path::PathBuf::from(std::env::var_os("TRACKER_CORPUS").expect("TRACKER_CORPUS"));
    let pins = include_str!("../../../corpus/tracker.tsv");
    let mut accepted = [0usize; 4];
    let mut attempted = 0;
    let mut state = 0x08_10_2026u32;
    for row in pins.lines().filter(|l| !l.starts_with('#') && !l.is_empty()) {
        let relative = row.split('\t').nth(1).unwrap();
        let format = match relative.rsplit('.').next().unwrap() {
            "mod" => 0, "s3m" => 1, "xm" => 2, "it" => 3, _ => continue,
        };
        let path = if let Some(local) = relative.strip_prefix("repo:") {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").join(local)
        } else { root.join(relative) };
        let bytes = std::fs::read(path).unwrap();
        for iteration in 0..16 {
            let mut damaged = bytes.clone();
            for _ in 0..(iteration % 8 + 1) {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                let at = state as usize % damaged.len();
                damaged[at] ^= (state >> 24) as u8;
            }
            if iteration % 8 == 0 {
                damaged.truncate(state as usize % damaged.len());
            }
            attempted += 1;
            eprintln!("mutation {relative} {iteration}");
            if let Some(song) = Song::load(&damaged) {
                accepted[format] += 1;
                assert!(song.frames() <= 48_000 * 7200 + 4800 + 120_000);
                let mut renderer = song.renderer_at(0);
                let mut pcm = [0.0; READ_FRAMES * 2];
                for _ in 0..8 {
                    let n = renderer.read(&mut pcm);
                    assert!(pcm[..n * 2].iter().all(|v| v.is_finite()));
                }
            }
        }
    }
    assert!(accepted.iter().all(|&n| n >= 16), "mutations must reach every format: {accepted:?}");
    eprintln!("attempted={attempted} loaded={accepted:?}");
}
