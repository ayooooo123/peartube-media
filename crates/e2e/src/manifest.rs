//! corpus/manifest.toml: the samples, the yardstick rows each claims, the
//! streams to play, and how each kind of stream is judged.
//!
//! Policies are per kind (`video:md5`, `audio:snr:90`, `audio:decodes`, …).
//! Records that name the same sample and stream selection merge into one
//! entry; two different policies for one kind are a manifest error, so a
//! decode-only token can never displace an oracle comparison.

use std::collections::BTreeMap;

use serde::Serialize;

/// The contract's accuracy floor for float decoders. Integer and lossless
/// decoders are held to `audio:md5`.
pub const CONTRACT_SNR_FLOOR_DB: f64 = 90.0;

/// A stream kind the player selects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Video,
    Audio,
    Subtitle,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Video => "video",
            Kind::Audio => "audio",
            Kind::Subtitle => "subtitle",
        }
    }

    /// FFmpeg's `codec_type` for this kind.
    pub fn ffmpeg_type(self) -> &'static str {
        self.name()
    }

    /// The prefix of this kind's policy tokens and yardstick rows.
    fn prefix(self) -> &'static str {
        match self {
            Kind::Video => "video",
            Kind::Audio => "audio",
            Kind::Subtitle => "sub",
        }
    }
}

/// How the streams of one kind are judged.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Policy {
    /// `video:md5`: every frame's MD5 equals FFmpeg's, same frame count.
    VideoMd5,
    /// `audio:md5`: PCM byte-identical to FFmpeg's in the decoder's sample
    /// format, same sample count (integer and lossless decoders).
    AudioMd5,
    /// `audio:snr:<dB>`: SNR at or above the floor (never below the
    /// contract's 90 dB), lengths within one decoder frame (float decoders).
    AudioSnr(f64),
    /// `sub:count`: as many cues shown as FFmpeg reads packets.
    SubCount,
    /// `<kind>:decodes`: FFmpeg cannot decode the format, so playing to the
    /// end with output is all that can be checked; never a verified pass.
    Decodes(Kind),
}

impl Policy {
    pub fn kind(self) -> Kind {
        match self {
            Policy::VideoMd5 => Kind::Video,
            Policy::AudioMd5 | Policy::AudioSnr(_) => Kind::Audio,
            Policy::SubCount => Kind::Subtitle,
            Policy::Decodes(kind) => kind,
        }
    }

    pub fn token(self) -> String {
        match self {
            Policy::VideoMd5 => "video:md5".into(),
            Policy::AudioMd5 => "audio:md5".into(),
            Policy::AudioSnr(db) => format!("audio:snr:{db}"),
            Policy::SubCount => "sub:count".into(),
            Policy::Decodes(kind) => format!("{}:decodes", kind.prefix()),
        }
    }
}

/// One `compare` token.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Token {
    Policy(Policy),
    /// `diag:audio:snr:<dB>`: SNR measured against a floor, reported, never
    /// accepting.
    Diagnostic(f64),
}

fn parse_db(s: &str, token: &str) -> Result<f64, String> {
    match s.parse::<f64>() {
        Ok(db) if db.is_finite() => Ok(db),
        _ => Err(format!("`{token}`: `{s}` is not a dB value")),
    }
}

fn parse_token(token: &str) -> Result<Token, String> {
    Ok(Token::Policy(match token {
        "video:md5" => Policy::VideoMd5,
        "audio:md5" => Policy::AudioMd5,
        "sub:count" => Policy::SubCount,
        "video:decodes" => Policy::Decodes(Kind::Video),
        "audio:decodes" => Policy::Decodes(Kind::Audio),
        "sub:decodes" => Policy::Decodes(Kind::Subtitle),
        _ => {
            if let Some(db) = token.strip_prefix("diag:audio:snr:") {
                return Ok(Token::Diagnostic(parse_db(db, token)?));
            }
            if let Some(db) = token.strip_prefix("audio:snr:") {
                let db = parse_db(db, token)?;
                if db < CONTRACT_SNR_FLOOR_DB {
                    return Err(format!(
                        "`{token}` is below the contract's {CONTRACT_SNR_FLOOR_DB} dB floor; keep it as a \
                         non-accepting `diag:audio:snr:{db}` beside an accepting policy"
                    ));
                }
                Policy::AudioSnr(db)
            } else {
                return Err(format!(
                    "unknown compare token `{token}` (policies are per kind: video:md5, video:decodes, audio:md5, \
                     audio:snr:<dB>, audio:decodes, sub:count, sub:decodes; diagnostics: diag:audio:snr:<dB>)"
                ));
            }
        }
    }))
}

/// Which stream of each kind to play; `None` is the player's default (the
/// first video and audio track; the runner picks the first subtitle track,
/// since the player shows none by default).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct Selection {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<u32>,
}

/// One sample with one stream selection, its claimed rows and its policies.
#[derive(Clone, Debug)]
pub struct Entry {
    pub path: String,
    pub rows: Vec<String>,
    pub policies: BTreeMap<Kind, Policy>,
    /// `diag:audio:snr:<dB>` floors.
    pub diagnostics: Vec<f64>,
    pub selection: Selection,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEntry {
    path: String,
    rows: Vec<String>,
    compare: Vec<String>,
    #[serde(default)]
    streams: BTreeMap<String, u32>,
}

#[derive(Debug, serde::Deserialize)]
struct RawManifest {
    #[serde(default)]
    entry: Vec<RawEntry>,
}

/// Parses the manifest against the yardstick's rows. Every problem is
/// reported, not just the first.
pub fn parse(manifest: &str, yardstick: &[String]) -> Result<Vec<Entry>, Vec<String>> {
    let raw: RawManifest = toml::from_str(manifest).map_err(|e| vec![format!("manifest.toml: {e}")])?;
    let mut errors = Vec::new();
    let mut merged: BTreeMap<(String, Selection), Entry> = BTreeMap::new();
    for raw in raw.entry {
        let mut selection = Selection::default();
        for (key, &index) in &raw.streams {
            match key.as_str() {
                "video" => selection.video = Some(index),
                "audio" => selection.audio = Some(index),
                "subtitle" => selection.subtitle = Some(index),
                other => errors.push(format!("{}: unknown stream kind `{other}` in streams", raw.path)),
            }
        }
        for row in &raw.rows {
            if !yardstick.contains(row) {
                errors.push(format!("{}: row `{row}` is not in yardstick.toml", raw.path));
            }
        }
        if raw.compare.is_empty() {
            errors.push(format!("{}: no compare policy", raw.path));
        }
        let entry = merged.entry((raw.path.clone(), selection)).or_insert_with(|| Entry {
            path: raw.path.clone(),
            rows: Vec::new(),
            policies: BTreeMap::new(),
            diagnostics: Vec::new(),
            selection,
        });
        for row in raw.rows {
            if !entry.rows.contains(&row) {
                entry.rows.push(row);
            }
        }
        for token in &raw.compare {
            match parse_token(token) {
                Ok(Token::Policy(policy)) => match entry.policies.get(&policy.kind()) {
                    Some(&existing) if existing != policy => errors.push(format!(
                        "{}: contradictory {} policies `{}` and `{}`",
                        raw.path,
                        policy.kind().name(),
                        existing.token(),
                        policy.token()
                    )),
                    _ => {
                        entry.policies.insert(policy.kind(), policy);
                    }
                },
                Ok(Token::Diagnostic(db)) => {
                    if !entry.diagnostics.contains(&db) {
                        entry.diagnostics.push(db);
                    }
                }
                Err(e) => errors.push(format!("{}: {e}", raw.path)),
            }
        }
    }
    if errors.is_empty() { Ok(merged.into_values().collect()) } else { Err(errors) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yardstick() -> Vec<String> {
        ["video:dv", "audio:dvaudio", "container:raw_dv", "audio:qdm2", "audio:flac", "container:mkv"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn records_of_one_sample_merge_per_kind() {
        // The manifest's dvcprohd_720p50.mov records: before, their compare
        // lists were concatenated and the bare `decodes` of the audio record
        // turned the video's md5 check into a frames-captured check.
        let m = r#"
            [[entry]]
            path = "fate:dv/dvcprohd_720p50.mov"
            rows = ["video:dv"]
            compare = ["video:md5"]
            [[entry]]
            path = "fate:dv/dvcprohd_720p50.mov"
            rows = ["audio:dvaudio"]
            compare = ["audio:decodes"]
            [[entry]]
            path = "fate:dv/dvcprohd_720p50.mov"
            rows = ["container:raw_dv"]
            compare = ["video:md5"]
        "#;
        let entries = parse(m, &yardstick()).unwrap();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.rows, ["video:dv", "audio:dvaudio", "container:raw_dv"]);
        assert_eq!(e.policies.get(&Kind::Video), Some(&Policy::VideoMd5));
        assert_eq!(e.policies.get(&Kind::Audio), Some(&Policy::Decodes(Kind::Audio)));
    }

    #[test]
    fn a_bare_decodes_token_is_rejected() {
        let m = r#"
            [[entry]]
            path = "fate:dv/dvcprohd_720p50.mov"
            rows = ["audio:dvaudio"]
            compare = ["decodes"]
        "#;
        let errors = parse(m, &yardstick()).unwrap_err();
        assert!(errors[0].contains("unknown compare token `decodes`"), "{errors:?}");
    }

    #[test]
    fn contradictory_policies_for_one_kind_are_rejected() {
        let m = r#"
            [[entry]]
            path = "fate:dv/dvcprohd_720p50.mov"
            rows = ["video:dv"]
            compare = ["video:md5"]
            [[entry]]
            path = "fate:dv/dvcprohd_720p50.mov"
            rows = ["container:raw_dv"]
            compare = ["video:decodes"]
        "#;
        let errors = parse(m, &yardstick()).unwrap_err();
        assert!(errors[0].contains("contradictory video policies"), "{errors:?}");
    }

    #[test]
    fn floors_below_the_contract_are_diagnostics_only() {
        let below = r#"
            [[entry]]
            path = "fate:qt-surge-suite/surge-2-16-B-QDM2.mov"
            rows = ["audio:qdm2"]
            compare = ["audio:snr:60"]
        "#;
        let errors = parse(below, &yardstick()).unwrap_err();
        assert!(errors[0].contains("below the contract's 90 dB floor"), "{errors:?}");
        let diag = r#"
            [[entry]]
            path = "fate:qt-surge-suite/surge-2-16-B-QDM2.mov"
            rows = ["audio:qdm2"]
            compare = ["audio:snr:90", "diag:audio:snr:60"]
        "#;
        let e = &parse(diag, &yardstick()).unwrap()[0];
        assert_eq!(e.policies.get(&Kind::Audio), Some(&Policy::AudioSnr(90.0)));
        assert_eq!(e.diagnostics, [60.0]);
    }

    #[test]
    fn different_selections_of_one_sample_stay_separate() {
        let m = r#"
            [[entry]]
            path = "fate:mkv/flac_channel_layouts.mka"
            rows = ["audio:flac"]
            compare = ["audio:md5"]
            [[entry]]
            path = "fate:mkv/flac_channel_layouts.mka"
            rows = ["audio:flac", "container:mkv"]
            compare = ["audio:md5"]
            streams = { audio = 1 }
        "#;
        let entries = parse(m, &yardstick()).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].selection, Selection::default());
        assert_eq!(entries[1].selection.audio, Some(1));
    }

    #[test]
    fn unknown_rows_and_stream_kinds_are_rejected() {
        let m = r#"
            [[entry]]
            path = "gen:x.mkv"
            rows = ["audio:nope"]
            compare = ["audio:md5"]
            streams = { data = 1 }
        "#;
        let errors = parse(m, &yardstick()).unwrap_err();
        assert_eq!(errors.len(), 2, "{errors:?}");
    }

    #[test]
    fn the_corpus_manifest_parses() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus");
        let yardstick: toml::Value =
            toml::from_str(&std::fs::read_to_string(dir.join("yardstick.toml")).unwrap()).unwrap();
        let rows: Vec<String> =
            yardstick["rows"].as_array().unwrap().iter().map(|r| r.as_str().unwrap().to_string()).collect();
        let manifest = std::fs::read_to_string(dir.join("manifest.toml")).unwrap();
        if let Err(errors) = parse(&manifest, &rows) {
            panic!("corpus/manifest.toml:\n{}", errors.join("\n"));
        }
    }
}
