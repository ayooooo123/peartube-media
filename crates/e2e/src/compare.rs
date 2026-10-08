//! Verdicts of one captured stream against FFmpeg's decode of it.

use serde::Serialize;

use crate::oracle::{AudioFrameInfo, Pcm, SrtCue, SubEvent};
use crate::tap::Cue;

/// A stream's verdict. `Decodes` is not a pass: FFmpeg cannot decode the
/// format, so the stream played to the end with output but nothing
/// verified it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Verdict {
    #[serde(rename = "PASS")]
    Pass,
    #[serde(rename = "DECODES")]
    Decodes,
    #[serde(rename = "FAIL")]
    Fail,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Pass => "PASS",
            Verdict::Decodes => "DECODES",
            Verdict::Fail => "FAIL",
        }
    }
}

/// One stream's comparison.
#[derive(Debug)]
pub struct Compare {
    pub verdict: Verdict,
    pub metric: String,
    pub error: Option<String>,
    /// Non-accepting observations, reported beside the verdict.
    pub diagnostics: Vec<String>,
}

impl Compare {
    pub fn pass(metric: impl Into<String>) -> Self {
        Compare { verdict: Verdict::Pass, metric: metric.into(), error: None, diagnostics: Vec::new() }
    }

    pub fn decodes(metric: impl Into<String>) -> Self {
        Compare { verdict: Verdict::Decodes, metric: metric.into(), error: None, diagnostics: Vec::new() }
    }

    pub fn fail(metric: impl Into<String>, error: impl Into<String>) -> Self {
        Compare { verdict: Verdict::Fail, metric: metric.into(), error: Some(error.into()), diagnostics: Vec::new() }
    }
}

// ---------------------------------------------------------------- audio

/// One sample of `pcm` back in the canonical encoding FFmpeg writes for
/// `format`: the inverse of the engine's contracted conversion to f32
/// (`s16 / 32768`, `s32 / 2^31`, `(u8 - 128) / 128`, floats as is). `None`
/// when `x` is not on that format's grid, i.e. no sample of `format`
/// converts to it.
fn canonical_sample(x: f32, format: Pcm, out: &mut Vec<u8>) -> Option<()> {
    let on_grid = |v: f64, lo: f64, hi: f64| (v.fract() == 0.0 && (lo..=hi).contains(&v)).then_some(v);
    match format {
        Pcm::U8 => out.push(on_grid(x as f64 * 128.0 + 128.0, 0.0, 255.0)? as u8),
        Pcm::S16 => out.extend((on_grid(x as f64 * 32768.0, -32768.0, 32767.0)? as i16).to_le_bytes()),
        Pcm::S32 => {
            out.extend((on_grid(x as f64 * 2147483648.0, -2147483648.0, 2147483647.0)? as i32).to_le_bytes())
        }
        Pcm::F32 => out.extend(x.to_le_bytes()),
        Pcm::F64 => out.extend((x as f64).to_le_bytes()),
    }
    Some(())
}

/// Whether every sample of FFmpeg's canonical PCM survives the engine's
/// conversion to f32 unchanged, so that equal f32 output proves equal
/// integer output. 32-bit integers keep 24 significant bits in an f32, and
/// doubles lose their low mantissa bits.
fn representable_in_f32(reference: &[u8], format: Pcm) -> Result<(), String> {
    match format {
        Pcm::S32 => {
            for (i, b) in reference.chunks_exact(4).enumerate() {
                let v = i32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                if (v as f32) as f64 != v as f64 {
                    return Err(format!(
                        "FFmpeg's sample {i} ({v}) has more than 24 significant bits: the player's f32 output \
                         cannot carry it, so its exactness is unverifiable"
                    ));
                }
            }
        }
        Pcm::F64 => {
            for (i, b) in reference.chunks_exact(8).enumerate() {
                let v = f64::from_le_bytes(b.try_into().unwrap());
                if (v as f32) as f64 != v {
                    return Err(format!(
                        "FFmpeg's sample {i} ({v}) is not an f32: the player's f32 output cannot carry it"
                    ));
                }
            }
        }
        Pcm::U8 | Pcm::S16 | Pcm::F32 => {}
    }
    Ok(())
}

/// `audio:md5`: the player's PCM, back in FFmpeg's canonical encoding of the
/// decoder's sample format, must be byte-identical to FFmpeg's, with the same
/// sample count. Returns the metric, or why it differs.
pub fn exact_pcm(ours: &[f32], reference: &[u8], format: Pcm, channels: usize) -> Result<String, String> {
    let w = format.bytes();
    if reference.is_empty() {
        return Err("FFmpeg decoded no samples".into());
    }
    if reference.len() % w != 0 {
        return Err(format!("FFmpeg's PCM is {} bytes, not whole {w}-byte samples", reference.len()));
    }
    representable_in_f32(reference, format)?;
    let mut bytes = Vec::with_capacity(ours.len() * w);
    for (i, &x) in ours.iter().enumerate() {
        if canonical_sample(x, format, &mut bytes).is_none() {
            return Err(format!("sample {i} ({x}) is not a {format:?} value: the decode is not exact"));
        }
    }
    let per_frame = channels.max(1);
    let (got, want) = (ours.len() / per_frame, reference.len() / w / per_frame);
    let (md5_got, md5_want) = (refcheck::md5_hex(&bytes), refcheck::md5_hex(reference));
    if bytes == reference {
        return Ok(format!("samples={got} md5={md5_got}"));
    }
    let mismatch = bytes.chunks(w).zip(reference.chunks(w)).position(|(a, b)| a != b);
    Err(match mismatch {
        Some(i) => format!(
            "md5 {md5_got} vs FFmpeg {md5_want}: first difference at sample {} (channel {}); {got} vs {want} samples",
            i / per_frame,
            i % per_frame
        ),
        None => format!("md5 {md5_got} vs FFmpeg {md5_want}: {got} samples vs FFmpeg {want} (the common part is equal)"),
    })
}

/// How many interleaved samples a lossy decode may run long or short: one
/// frame of FFmpeg's decoder for this stream (its largest), times the
/// channel count.
pub fn lossy_slack(frames: &[AudioFrameInfo], channels: u16) -> Result<usize, String> {
    let largest = frames.iter().map(|f| f.nb_samples).max().ok_or("FFmpeg decoded no frames")?;
    if let Some(f) = frames.iter().find(|f| f.channels != channels) {
        return Err(format!("FFmpeg decodes {} channels, the player {channels}", f.channels));
    }
    Ok(largest as usize * channels as usize)
}

/// `audio:snr:<floor>`: SNR over the common length, lengths within `slack`
/// samples, accepted only when `snr >= floor` (+infinity passes, -infinity
/// and NaN do not).
pub fn snr_pcm(ours: &[f32], reference: &[f32], slack: usize, floor: f64) -> Compare {
    match refcheck::try_snr_db(reference, ours, slack) {
        Ok(snr) if snr >= floor => Compare::pass(format!("snr={snr:.1} dB")),
        Ok(snr) => Compare::fail(format!("snr={snr:.1} dB"), format!("SNR {snr:.1} dB below the {floor} dB floor")),
        Err(e) => Compare::fail("snr=n/a", e),
    }
}

// ---------------------------------------------------------------- subtitles

/// A time as FFmpeg's `srt` muxer writes it: `hh:mm:ss,mmm`, the
/// microseconds rounded to the nearest millisecond (FFmpeg rescales a
/// subtitle's times to the muxer's milliseconds rounding to nearest).
pub fn srt_time(us: i64) -> String {
    srt_ms(us_to_ms(us))
}

fn us_to_ms(us: i64) -> i64 {
    us.max(0).saturating_add(500) / 1000
}

fn srt_ms(ms: i64) -> String {
    let s = ms / 1000;
    format!("{:02}:{:02}:{:02},{:03}", s / 3600, s / 60 % 60, s % 60, ms % 1000)
}

/// A cue's end as FFmpeg's `srt` muxer writes it. A display state up until
/// the next (`end_us == i64::MAX`, as EIA-608 captions are) ends where
/// FFmpeg's does: its `end_display_time` is `UINT32_MAX` milliseconds.
fn srt_end(start_us: i64, end_us: i64) -> String {
    match end_us {
        i64::MAX => srt_ms(us_to_ms(start_us) + i64::from(u32::MAX)),
        end => srt_time(end),
    }
}

/// A display state that puts nothing up: a bitmap state with no visible
/// pixel (DVB's page clears), or a caption state with no text (an emptied
/// caption screen). It takes down what is up.
fn blank_state(cue: &Cue) -> bool {
    match cue {
        Cue::Bitmap { blank, .. } => *blank,
        Cue::Text { state, text, .. } => *state && plain_text(text).is_empty(),
    }
}

/// Every cue the decoder handed the pipeline was shown: the headless
/// capture's non-empty shows must number the decoded cues that are not
/// blank states, which no non-empty show records.
pub fn shown_all(cues: &[Cue], shown: usize) -> Result<(), String> {
    let visible = cues.iter().filter(|c| !blank_state(c)).count();
    if shown != visible {
        return Err(format!("the pipeline showed {shown} of the {visible} decoded non-blank cues"));
    }
    Ok(())
}

/// A SubRip body's text without markup: SubRip tags (`<...>`) and ASS
/// override blocks (`{...}`) removed, each line trimmed, empty lines
/// dropped. Markup conventions differ between renderers (`#0000FF` against
/// FFmpeg's `#0000ff`, ASS styles FFmpeg's `srt` encoder turns into `<font>`
/// tags), the words on screen do not.
pub fn plain_text(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        let close = match c {
            '<' => '>',
            '{' => '}',
            _ => {
                out.push(c);
                continue;
            }
        };
        // Only a closed tag is markup; a lone `<` is text.
        let rest: String = chars.clone().collect();
        match rest.find(close) {
            Some(end) => {
                for _ in 0..rest[..=end].chars().count() {
                    chars.next();
                }
            }
            None => out.push(c),
        }
    }
    out.replace("\r\n", "\n")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// `sub:text`: the decoded cues, in order, against FFmpeg's decode
/// re-encoded as SubRip: same count, same timing to the millisecond, same
/// text ([`plain_text`]). Bodies whose markup differs from FFmpeg's are
/// reported as a non-accepting diagnostic.
pub fn text_cues(cues: &[Cue], shown: usize, reference: &[SrtCue]) -> Result<(String, Vec<String>), String> {
    shown_all(cues, shown)?;
    let mut ours = Vec::with_capacity(cues.len());
    for (i, cue) in cues.iter().enumerate() {
        match cue {
            Cue::Text { start_us, end_us, text, .. } => ours.push(SrtCue {
                timing: format!("{} --> {}", srt_time(*start_us), srt_end(*start_us, *end_us)),
                body: text.replace("\r\n", "\n").trim().to_string(),
            }),
            Cue::Bitmap { .. } => return Err(format!("cue {i} is a bitmap; the policy expects text")),
        }
    }
    for (i, (o, r)) in ours.iter().zip(reference).enumerate() {
        if o.timing != r.timing || plain_text(&o.body) != plain_text(&r.body) {
            return Err(format!("cue {i}: {:?} {:?} vs FFmpeg {:?} {:?}", o.timing, o.body, r.timing, r.body));
        }
    }
    if ours.len() != reference.len() {
        return Err(format!("{} cues vs FFmpeg {} (the common ones match)", ours.len(), reference.len()));
    }
    let restyled: Vec<usize> = (0..ours.len()).filter(|&i| ours[i].body != reference[i].body).collect();
    let diagnostics = restyled
        .first()
        .map(|&i| {
            format!(
                "markup differs from FFmpeg's SubRip rendering in {} of {} cues (non-accepting), first cue {i}: {:?} vs {:?}",
                restyled.len(),
                ours.len(),
                ours[i].body,
                reference[i].body
            )
        })
        .into_iter()
        .collect();
    Ok((format!("cues={} text+timing match", ours.len()), diagnostics))
}

/// The distinct states of a canvas sequence, blank ones dropped:
/// consecutive repeats are one state.
fn states(md5s: impl IntoIterator<Item = String>, blank: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for m in md5s {
        if m != blank && out.last() != Some(&m) {
            out.push(m);
        }
    }
    out
}

/// `sub:bitmap`: the decoded bitmaps against FFmpeg's decode: as many shown
/// subtitles as FFmpeg's decoder emits with bitmaps, starting at the same
/// millisecond (ending too where FFmpeg's has an end), and the same canvas
/// states as FFmpeg composes them (sub2video, RGBA).
pub fn bitmap_cues(
    cues: &[Cue],
    shown: usize,
    events: &[SubEvent],
    (width, height): (usize, usize),
    canvases: &[String],
) -> Result<String, String> {
    shown_all(cues, shown)?;
    let mut ours = Vec::new();
    for (i, cue) in cues.iter().enumerate() {
        match cue {
            Cue::Bitmap { blank: true, .. } => {}
            Cue::Bitmap { start_us, end_us, width: w, height: h, md5, .. } => {
                if (*w, *h) != (width, height) {
                    return Err(format!("cue {i} is {w}x{h}, FFmpeg's canvas {width}x{height}"));
                }
                ours.push((*start_us, *end_us, md5.clone()));
            }
            Cue::Text { .. } => return Err(format!("cue {i} is text; the policy expects bitmaps")),
        }
    }
    let shown_events: Vec<&SubEvent> = events.iter().filter(|e| e.rects > 0).collect();
    if ours.len() != shown_events.len() {
        return Err(format!("{} non-blank bitmaps vs FFmpeg {} subtitles with bitmaps", ours.len(), shown_events.len()));
    }
    for (i, ((start, end, _), e)) in ours.iter().zip(&shown_events).enumerate() {
        if start / 1000 != e.start_us / 1000 {
            return Err(format!("bitmap {i} starts at {} vs FFmpeg {}", srt_time(*start), srt_time(e.start_us)));
        }
        if let (Some(end), Some(want)) = (end, e.end_us) {
            if end / 1000 != want / 1000 {
                return Err(format!("bitmap {i} ends at {} vs FFmpeg {}", srt_time(*end), srt_time(want)));
            }
        }
    }
    let blank = refcheck::md5_hex(&vec![0u8; width * height * 4]);
    let got = states(ours.iter().map(|(_, _, m)| m.clone()), &blank);
    let want = states(canvases.iter().cloned(), &blank);
    if got != want {
        return Err(format!("canvas states {got:?} vs FFmpeg {want:?}"));
    }
    Ok(format!("bitmaps={} timing+raster match", ours.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s16(samples: &[i16]) -> (Vec<f32>, Vec<u8>) {
        let f = samples.iter().map(|&s| s as f32 / 32768.0).collect();
        let b = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        (f, b)
    }

    fn tone(n: usize) -> Vec<i16> {
        (0..n).map(|i| ((i as f64 * 0.05).sin() * 20000.0) as i16).collect()
    }

    #[test]
    fn exact_pcm_accepts_identical_samples() {
        let (ours, reference) = s16(&tone(48000));
        assert!(exact_pcm(&ours, &reference, Pcm::S16, 1).is_ok());
    }

    #[test]
    fn one_lsb_error_passes_the_old_120_db_floor_but_not_md5() {
        let samples = tone(48000);
        let (_, reference) = s16(&samples);
        let mut wrong = samples.clone();
        wrong[1000] += 1;
        let (ours, _) = s16(&wrong);
        let (ref_f32, _) = s16(&samples);
        let snr = refcheck::try_snr_db(&ref_f32, &ours, 0).unwrap();
        assert!(snr >= 120.0, "the old audio:md5 stand-in accepted this: {snr} dB");
        let err = exact_pcm(&ours, &reference, Pcm::S16, 1).unwrap_err();
        assert!(err.contains("first difference at sample 1000"), "{err}");
    }

    #[test]
    fn missing_tail_samples_fail_md5() {
        let samples = tone(48000);
        let (ours, reference) = s16(&samples);
        // The old slack (rate / 10 + 2048 = 6848 samples) scored only the
        // common prefix, so this passed as +infinity.
        let short = &ours[..48000 - 6000];
        let (ref_f32, _) = s16(&samples);
        assert_eq!(refcheck::try_snr_db(&ref_f32, short, 48000 / 10 + 2048), Ok(f64::INFINITY));
        let err = exact_pcm(short, &reference, Pcm::S16, 1).unwrap_err();
        assert!(err.contains("42000 samples vs FFmpeg 48000"), "{err}");
    }

    #[test]
    fn exact_pcm_rejects_output_off_the_integer_grid() {
        let (mut ours, reference) = s16(&tone(100));
        ours[3] += 1e-6;
        let err = exact_pcm(&ours, &reference, Pcm::S16, 1).unwrap_err();
        assert!(err.contains("sample 3"), "{err}");
    }

    #[test]
    fn exact_pcm_refuses_32_bit_samples_an_f32_cannot_carry() {
        // 24-bit content in s32 (TrueHD, FLAC-24) is exact in f32 ...
        let v24: Vec<i32> = vec![0x7fff_ff00, i32::MIN, 0x1234_5600];
        let bytes: Vec<u8> = v24.iter().flat_map(|v| v.to_le_bytes()).collect();
        let ours: Vec<f32> = v24.iter().map(|&v| v as f32 / 2147483648.0).collect();
        assert!(exact_pcm(&ours, &bytes, Pcm::S32, 1).is_ok());
        // ... a 32-bit sample is not: f32 equality would not prove it.
        let v32 = [0x1234_5679i32];
        let bytes: Vec<u8> = v32.iter().flat_map(|v| v.to_le_bytes()).collect();
        let ours: Vec<f32> = v32.iter().map(|&v| v as f32 / 2147483648.0).collect();
        let err = exact_pcm(&ours, &bytes, Pcm::S32, 1).unwrap_err();
        assert!(err.contains("unverifiable"), "{err}");
    }

    #[test]
    fn exact_pcm_needs_a_reference() {
        assert!(exact_pcm(&[0.0], &[], Pcm::S16, 1).is_err());
    }

    #[test]
    fn snr_pcm_accepts_only_snr_at_or_above_the_floor() {
        let silence = [0.0f32; 64];
        let noise: Vec<f32> = (0..64).map(|i| if i % 2 == 0 { 0.01 } else { -0.01 }).collect();
        // A silent reference against noise scores -infinity: the old
        // `snr.is_infinite() || snr >= floor` accepted it.
        assert_eq!(snr_pcm(&noise, &silence, 0, 90.0).verdict, Verdict::Fail);
        assert_eq!(snr_pcm(&silence, &silence, 0, 90.0).verdict, Verdict::Pass);
        assert_eq!(snr_pcm(&[], &silence, 64, 90.0).verdict, Verdict::Fail, "empty decode");
        assert_eq!(snr_pcm(&silence, &[], 64, 90.0).verdict, Verdict::Fail, "empty reference");
    }

    fn text(start_ms: i64, end_ms: i64, body: &str) -> Cue {
        Cue::Text { start_us: start_ms * 1000, end_us: end_ms * 1000, text: body.into(), state: false }
    }

    fn srt(timing: &str, body: &str) -> SrtCue {
        SrtCue { timing: timing.into(), body: body.into() }
    }

    #[test]
    fn text_cues_compare_count_timing_and_body() {
        let reference = [srt("00:00:01,000 --> 00:00:02,500", "Hello"), srt("00:01:00,000 --> 00:01:01,000", "<i>Bye</i>")];
        let ours = [text(1000, 2500, "Hello"), text(60_000, 61_000, "<i>Bye</i>")];
        assert_eq!(text_cues(&ours, 2, &reference).unwrap().1, Vec::<String>::new());
        // Same words, other markup: passes, reported as a diagnostic.
        let restyled = [text(1000, 2500, "Hello"), text(60_000, 61_000, "Bye")];
        let (_, diagnostics) = text_cues(&restyled, 2, &reference).unwrap();
        assert!(diagnostics[0].contains("1 of 2 cues"), "{diagnostics:?}");
        // The old check counted shows against packets: any text passed.
        let garbled = [text(1000, 2500, "Hello"), text(60_000, 61_000, "<i>By</i>")];
        assert!(text_cues(&garbled, 2, &reference).unwrap_err().contains("cue 1"));
        let late = [text(1000, 2500, "Hello"), text(60_040, 61_000, "<i>Bye</i>")];
        assert!(text_cues(&late, 2, &reference).unwrap_err().contains("00:01:00,040"));
        assert!(text_cues(&ours[..1], 1, &reference).unwrap_err().contains("1 cues vs FFmpeg 2"));
        assert!(text_cues(&ours, 1, &reference).unwrap_err().contains("showed 1 of the 2"));
    }

    fn bitmap(start_ms: i64, md5: &str, blank: bool) -> Cue {
        Cue::Bitmap { start_us: start_ms * 1000, end_us: None, width: 2, height: 1, md5: md5.into(), blank }
    }

    #[test]
    fn bitmap_cues_compare_timing_and_raster_states() {
        let blank = refcheck::md5_hex(&[0u8; 8]);
        let events = [
            SubEvent { start_us: 67_467, end_us: None, rects: 2 },
            SubEvent { start_us: 900_000, end_us: None, rects: 0 },
        ];
        let canvases = [blank.clone(), "aa".to_string(), "aa".to_string(), blank.clone()];
        let ours = [bitmap(67, "aa", false), bitmap(900, &blank, true)];
        // The blank state clears the screen: one non-empty show.
        assert!(bitmap_cues(&ours, 1, &events, (2, 1), &canvases).is_ok());
        assert!(bitmap_cues(&ours, 2, &events, (2, 1), &canvases).unwrap_err().contains("showed 2 of the 1"));
        let wrong_pixels = [bitmap(67, "bb", false), bitmap(900, &blank, true)];
        assert!(bitmap_cues(&wrong_pixels, 1, &events, (2, 1), &canvases).unwrap_err().contains("canvas states"));
        let late = [bitmap(167, "aa", false), bitmap(900, &blank, true)];
        assert!(bitmap_cues(&late, 1, &events, (2, 1), &canvases).unwrap_err().contains("starts at"));
    }

    #[test]
    fn plain_text_drops_markup_but_keeps_words() {
        assert_eq!(plain_text("<font color=\"#0000FF\">blue</font>"), plain_text("<font color=\"#0000ff\">blue</font>"));
        assert_eq!(plain_text("{\\an8}<b>top</b>\n second "), "top\nsecond");
        assert_eq!(plain_text("a < b"), "a < b");
        assert_eq!(plain_text("[SIZE]20"), "[SIZE]20");
    }

    #[test]
    fn srt_time_rounds_to_the_millisecond_as_ffmpeg_does() {
        assert_eq!(srt_time(3_723_456_789), "01:02:03,457");
        assert_eq!(srt_time(3_723_456_499), "01:02:03,456");
        assert_eq!(srt_time(-5), "00:00:00,000");
    }

    /// Captions are display states up until the next: FFmpeg's real time
    /// EIA-608 events end `UINT32_MAX` ms after their start. The first
    /// event is FATE Closedcaption_rollup.m2v's (a picture at 0.967633 s),
    /// as `ffmpeg -real_time 1 ... -c:s srt` writes it; the second, an
    /// emptied screen, puts nothing up.
    #[test]
    fn caption_states_compare_with_ffmpegs_real_time_events() {
        let state = |us: i64, body: &str| Cue::Text { start_us: us, end_us: i64::MAX, text: body.into(), state: true };
        let reference = [srt("00:00:00,968 --> 1193:02:48,263", "<font face=\"Monospace\">{\\an7}(<i> inaudibl</i></font>"), srt("00:00:01,168 --> 1193:02:48,463", "")];
        let ours = [state(967_633, "(<i> inaudibl</i>"), state(1_167_833, "")];
        assert!(text_cues(&ours, 1, &reference).is_ok());
        assert!(text_cues(&ours, 2, &reference).unwrap_err().contains("showed 2 of the 1"));
        let late = [state(968_633, "(<i> inaudibl</i>"), state(1_167_833, "")];
        assert!(text_cues(&late, 1, &reference).unwrap_err().contains("cue 0"));
    }

    #[test]
    fn lossy_slack_is_one_decoder_frame_of_every_channel() {
        let frames = vec![AudioFrameInfo { nb_samples: 1024, channels: 1 }; 10];
        // 48 kHz mono AAC: the old allowance was 48000 / 10 + 2048 = 6848.
        assert_eq!(lossy_slack(&frames, 1), Ok(1024));
        let stereo = vec![AudioFrameInfo { nb_samples: 1152, channels: 2 }; 3];
        assert_eq!(lossy_slack(&stereo, 2), Ok(2304));
        assert!(lossy_slack(&stereo, 1).is_err(), "channel count disagrees");
        assert!(lossy_slack(&[], 2).is_err());
    }
}
