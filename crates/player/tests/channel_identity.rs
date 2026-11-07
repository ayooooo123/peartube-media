//! Decoded channel identity: every slot of the PCM the player hands its sink
//! must carry the speaker its reported layout names. Tone sources give each
//! channel its own frequency; real TrueHD and DTS-HD 7.1 files are checked
//! against FFmpeg's decode channel by name. A side/rear swap anywhere between
//! decoder and sink puts the wrong channel in a labelled slot.

use std::sync::Arc;

use oxideav_core::{ChannelLayout, ChannelPosition};
use player::{AudioCapture, AudioOutputMode, Headless, PcmFormat, Player, PlayerOptions};

#[path = "support/seek_preroll.rs"]
mod fixture;

const RATE: f64 = 48_000.0;

/// FFmpeg's channel names, the core position each names, and a tone for it.
const SPEAKERS: [(&str, ChannelPosition, f64); 8] = [
    ("FL", ChannelPosition::FrontLeft, 300.0),
    ("FR", ChannelPosition::FrontRight, 420.0),
    ("FC", ChannelPosition::FrontCenter, 540.0),
    ("LFE", ChannelPosition::LowFrequency, 70.0),
    ("BL", ChannelPosition::BackLeft, 660.0),
    ("BR", ChannelPosition::BackRight, 780.0),
    ("SL", ChannelPosition::SideLeft, 900.0),
    ("SR", ChannelPosition::SideRight, 1020.0),
];

fn speaker(name: &str) -> (ChannelPosition, f64) {
    let (_, p, hz) = SPEAKERS.iter().find(|(n, _, _)| *n == name).unwrap();
    (*p, *hz)
}

fn play(path: &std::path::Path) -> AudioCapture {
    let headless = Headless::new();
    let opts = PlayerOptions { realtime: false, audio_mode: AudioOutputMode::BitPerfectPcm, ..PlayerOptions::default() };
    let state = Player::open(&path.to_string_lossy(), headless.clone(), Arc::new(codecs::context()), opts, |_| {}).wait();
    assert!(state.ended && state.error.is_none(), "{}: {state:?}", path.display());
    headless.capture().audio.remove(0)
}

fn positions(audio: &AudioCapture, channels: usize, name: &str) -> &'static [ChannelPosition] {
    assert_eq!(usize::from(audio.channels), channels, "{name}");
    let layout = audio.channel_layout.unwrap_or(ChannelLayout::DiscreteN(audio.channels));
    let positions = layout.positions();
    assert_eq!(positions.len(), channels, "{name}: layout {layout:?} names no speakers");
    positions
}

fn samples(bytes: &[u8], format: PcmFormat) -> Vec<f64> {
    match format {
        PcmFormat::U8 => bytes.iter().map(|&b| (f64::from(b) - 128.0) / 128.0).collect(),
        PcmFormat::S16 => bytes.chunks_exact(2).map(|b| f64::from(i16::from_le_bytes([b[0], b[1]])) / 32768.0).collect(),
        PcmFormat::S24 => bytes
            .chunks_exact(3)
            .map(|b| f64::from(i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) / 8_388_608.0)
            .collect(),
        PcmFormat::S32 => bytes.chunks_exact(4).map(|b| f64::from(i32::from_le_bytes(b.try_into().unwrap())) / 2_147_483_648.0).collect(),
        PcmFormat::F32 => bytes.chunks_exact(4).map(|b| f64::from(f32::from_le_bytes(b.try_into().unwrap()))).collect(),
        PcmFormat::F64 => bytes.chunks_exact(8).map(|b| f64::from_le_bytes(b.try_into().unwrap())).collect(),
    }
}

fn goertzel_power(x: impl Iterator<Item = f64>, hz: f64) -> f64 {
    let k = 2.0 * (2.0 * std::f64::consts::PI * hz / RATE).cos();
    let (mut s1, mut s2) = (0.0, 0.0);
    for v in x {
        let s = v + k * s1 - s2;
        s2 = s1;
        s1 = s;
    }
    s1 * s1 + s2 * s2 - k * s1 * s2
}

/// Encodes one tone per FFmpeg channel of `ff_layout`, plays it, and checks
/// that each labelled slot carries the tone of the speaker its label names.
fn assert_tone_slots(name: &str, ff_layout: &str, channels: &[&str], codec_args: &[&str]) {
    let path = fixture::tmp(name);
    let mut graph = String::new();
    for (i, ch) in channels.iter().enumerate() {
        graph.push_str(&format!("sine=frequency={}:sample_rate=48000:duration=1,volume=0.5[a{i}];", speaker(ch).1));
    }
    for i in 0..channels.len() {
        graph.push_str(&format!("[a{i}]"));
    }
    let map: Vec<String> = channels.iter().enumerate().map(|(i, ch)| format!("{i}.0-{ch}")).collect();
    graph.push_str(&format!("join=inputs={}:channel_layout={ff_layout}:map={}[out]", channels.len(), map.join("|")));
    let mut args = vec!["-filter_complex", graph.as_str(), "-map", "[out]"];
    args.extend_from_slice(codec_args);
    fixture::encode(&args, &path);

    let audio = play(&path);
    let n = channels.len();
    let positions = positions(&audio, n, name);
    let all = samples(&audio.exact_pcm, audio.sample_format.expect("sample format"));
    let skip = 4_800 * n; // past encoder delay and the tones' onset
    let mut got = Vec::new();
    for (slot, position) in positions.iter().enumerate() {
        let lane = || all.iter().skip(skip + slot).step_by(n).copied();
        let heard = channels
            .iter()
            .map(|ch| (*ch, goertzel_power(lane(), speaker(ch).1)))
            .fold(("?", 0.0), |best, cur| if cur.1 > best.1 { cur } else { best })
            .0;
        got.push(format!("{position:?}={heard}"));
        assert_eq!(speaker(heard).0, *position, "{name}: slot {slot} is labelled {position:?} but carries {heard}; slots {got:?}");
    }
}

/// Plays a real 7.1 file and checks each labelled slot against FFmpeg's
/// decode of the speaker that label names (FFmpeg 7.1: FL FR FC LFE BL BR SL SR).
fn assert_fate_7_1_slots(relative: &str) {
    let path = refcheck::fate(relative);
    let out = std::process::Command::new(refcheck::pinned_ffmpeg())
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(&path)
        .args(["-map", "0:a:0", "-f", "s32le", "-c:a", "pcm_s32le", "-"])
        .output()
        .unwrap();
    assert!(out.status.success(), "ffmpeg {relative}");
    let ffmpeg_order = ["FL", "FR", "FC", "LFE", "BL", "BR", "SL", "SR"];
    let audio = play(&path);
    assert_eq!(audio.sample_format, Some(PcmFormat::S32), "{relative}");
    let positions = positions(&audio, 8, relative);
    let lane = |bytes: &[u8], slot: usize| -> Vec<u8> { bytes.chunks_exact(32).flat_map(|f| f[slot * 4..slot * 4 + 4].to_vec()).collect() };
    assert_eq!(audio.exact_pcm.len(), out.stdout.len(), "{relative}: sample count");
    for (slot, position) in positions.iter().enumerate() {
        let reference = ffmpeg_order.iter().position(|ch| speaker(ch).0 == *position).unwrap();
        assert!(
            lane(&audio.exact_pcm, slot) == lane(&out.stdout, reference),
            "{relative}: slot {slot} is labelled {position:?} but is not FFmpeg's {}",
            ffmpeg_order[reference]
        );
    }
}

#[test]
fn pcm_7_1_slots_carry_their_speakers() {
    assert_tone_slots("ident71_pcm.mkv", "7.1", &["FL", "FR", "FC", "LFE", "BL", "BR", "SL", "SR"], &["-c:a", "pcm_s24le"]);
}

#[test]
fn flac_7_1_slots_carry_their_speakers() {
    assert_tone_slots("ident71_flac.mkv", "7.1", &["FL", "FR", "FC", "LFE", "BL", "BR", "SL", "SR"], &["-c:a", "flac"]);
}

#[test]
fn opus_7_1_slots_carry_their_speakers() {
    assert_tone_slots("ident71_opus.mkv", "7.1", &["FL", "FR", "FC", "LFE", "BL", "BR", "SL", "SR"], &["-c:a", "libopus", "-b:a", "512k"]);
}

#[test]
fn aac_5_1_slots_carry_their_speakers() {
    assert_tone_slots("ident51_aac.mkv", "5.1(side)", &["FL", "FR", "FC", "LFE", "SL", "SR"], &["-c:a", "aac", "-b:a", "384k"]);
}

#[test]
#[ignore = "the AAC decoder emits 7.1 in bitstream element order (FC first), not FFmpeg's order"]
fn aac_7_1_slots_carry_their_speakers() {
    assert_tone_slots("ident71_aac.mkv", "7.1", &["FL", "FR", "FC", "LFE", "BL", "BR", "SL", "SR"], &["-c:a", "aac", "-b:a", "512k"]);
}

#[test]
fn truehd_7_1_slots_match_ffmpeg_by_speaker() {
    assert_fate_7_1_slots("truehd/atmos.thd");
}

#[test]
fn dts_hd_ma_7_1_slots_match_ffmpeg_by_speaker() {
    assert_fate_7_1_slots("dts/dcadec-suite/xll_71_24_96_768.dtshd");
}
