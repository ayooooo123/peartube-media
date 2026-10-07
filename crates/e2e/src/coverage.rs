//! Which yardstick rows an entry covers. A row is credited by what actually
//! ran - the codec the player decoded, the demuxer the player's probe rule
//! picked - never by the manifest's label, and only when the reference
//! comparison passed. Codec rows accept a fixed set of ids: the row's codec
//! under each name OxideAV registers it with, plus the genuine aliases
//! spelled out below. Container rows name the demuxers that implement them.

use std::path::Path;

use crate::manifest::Kind;

/// One judged stream of an entry.
#[derive(Clone, Debug)]
pub struct StreamFacts {
    pub kind: Kind,
    /// The codec id the player decoded the stream with.
    pub codec: String,
    /// The container tag FFmpeg reports for the stream, e.g. `XVID`.
    pub tag: Option<String>,
    /// `PASS`, `DECODES` or `FAIL`.
    pub verdict: String,
    /// The HTTP pass agreed (or did not run).
    pub http_ok: bool,
}

/// What one entry's run established.
#[derive(Clone, Debug, Default)]
pub struct EntryFacts {
    /// The demuxer the player's probe rule picked.
    pub demuxer: Option<String>,
    /// The file is an OGM (Ogg with DirectShow-style stream headers).
    pub ogm: bool,
    /// Why the entry as a whole failed (open, engine, tracks, selection,
    /// HTTP), if it did: no container row then.
    pub entry_failure: Option<String>,
    /// A failure that also discredits every stream's result: the player's
    /// tracks or selection differed from what the comparison mapped, or the
    /// HTTP pass failed outright (bytes, open, end of playback).
    pub stream_failure: Option<String>,
    pub streams: Vec<StreamFacts>,
}

/// How an entry stands on one row it claims.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Claim {
    /// FFmpeg verified it.
    Verified,
    /// It played with output, but FFmpeg cannot decode the format, so
    /// nothing verified it.
    Unverified,
    Failed(String),
}

/// A codec the row accepts: one of `ids`, optionally only under one of
/// `tags` (FFmpeg's container tag) or inside an OGM file.
struct Codec {
    ids: &'static [&'static str],
    tags: &'static [&'static str],
    ogm: bool,
}

const fn ids(ids: &'static [&'static str]) -> Codec {
    Codec { ids, tags: &[], ogm: false }
}

const fn tagged(ids: &'static [&'static str], tags: &'static [&'static str]) -> Codec {
    Codec { ids, tags, ogm: false }
}

enum Rule {
    Codec(Kind, Vec<Codec>),
    /// Linear PCM: every `pcm_*` id but the companded G.711 ones, and
    /// OxideAV's raw signed-linear `slin*`.
    Lpcm,
    /// The ADPCM family: every `adpcm_*` id.
    Adpcm,
    Container { demuxers: &'static [&'static str], ogm: bool },
}

const MPEG4: &[&str] = &["mpeg4video", "mpeg4"];

fn rule(row: &str) -> Option<Rule> {
    use Kind::{Audio, Subtitle, Video};
    let codec = |kind, list: &'static [&'static str]| Some(Rule::Codec(kind, vec![ids(list)]));
    let container = |demuxers| Some(Rule::Container { demuxers, ogm: false });
    match row {
        "video:mpeg1" => codec(Video, &["mpeg1video"]),
        "video:mpeg2" => codec(Video, &["mpeg2video"]),
        // DivX ;-) 3 is MS-MPEG-4 v3 (DIV3/MP43, OxideAV also registers
        // div3); DivX 4-6 are MPEG-4 Part 2 under DivX tags.
        "video:divx" => {
            Some(Rule::Codec(Video, vec![ids(&["msmpeg4v3", "div3"]), tagged(MPEG4, &["DIVX", "divx", "DX50", "dx50"])]))
        }
        "video:mpeg4" => codec(Video, MPEG4),
        // Xvid and 3ivx are MPEG-4 Part 2 encoders: only their tags tell.
        "video:xvid" => Some(Rule::Codec(Video, vec![tagged(MPEG4, &["XVID", "xvid", "XVIX"])])),
        "video:3ivx" => Some(Rule::Codec(Video, vec![tagged(MPEG4, &["3IV1", "3IV2", "3IVX", "3ivx", "3iv2"])])),
        "video:h261" => codec(Video, &["h261"]),
        // H.263+ is H.263 version 2.
        "video:h263" => codec(Video, &["h263", "h263p"]),
        "video:h263i" => codec(Video, &["h263i"]),
        "video:h264" => codec(Video, &["h264"]),
        "video:cinepak" => codec(Video, &["cinepak"]),
        "video:theora" => codec(Video, &["theora"]),
        // VC-2 is the SMPTE-standardised Dirac profile.
        "video:dirac" => codec(Video, &["dirac", "vc2"]),
        "video:mjpeg" => codec(Video, &["mjpeg"]),
        "video:wmv1" => codec(Video, &["wmv1"]),
        "video:wmv2" => codec(Video, &["wmv2"]),
        "video:wmv3" => codec(Video, &["wmv3"]),
        "video:vc1" => codec(Video, &["vc1"]),
        "video:svq1" => codec(Video, &["svq1"]),
        "video:svq3" => codec(Video, &["svq3"]),
        "video:dv" => codec(Video, &["dvvideo"]),
        "video:vp3" => codec(Video, &["vp3"]),
        "video:vp5" => codec(Video, &["vp5"]),
        // VP6F (Flash) and VP6A (alpha) are VP6.
        "video:vp6" => codec(Video, &["vp6", "vp6f", "vp6a"]),
        "video:iv32" => codec(Video, &["indeo3"]),
        "video:rv10" => codec(Video, &["rv10"]),
        "video:rv20" => codec(Video, &["rv20"]),
        "video:rv30" => codec(Video, &["rv30"]),
        "video:rv40" => codec(Video, &["rv40"]),
        // OxideAV registers HEVC as both h265 and hevc.
        "video:hevc" => codec(Video, &["hevc", "h265"]),
        "video:vp8" => codec(Video, &["vp8"]),
        "video:vp9" => codec(Video, &["vp9"]),
        "video:av1" => codec(Video, &["av1"]),

        "audio:mp1" => codec(Audio, &["mp1"]),
        "audio:mp2" => codec(Audio, &["mp2"]),
        "audio:mp3" => codec(Audio, &["mp3"]),
        "audio:aac" => codec(Audio, &["aac", "aac_latm"]),
        "audio:vorbis" => codec(Audio, &["vorbis"]),
        "audio:ac3" => codec(Audio, &["ac3"]),
        "audio:eac3" => codec(Audio, &["eac3"]),
        "audio:mlp" => codec(Audio, &["mlp"]),
        "audio:truehd" => codec(Audio, &["truehd"]),
        "audio:dts" => codec(Audio, &["dts", "dca"]),
        // FFmpeg's wmav1/wmav2 are OxideAV's wma1/wma2.
        "audio:wma1" => codec(Audio, &["wma1", "wmav1"]),
        "audio:wma2" => codec(Audio, &["wma2", "wmav2"]),
        "audio:wmapro" => codec(Audio, &["wmapro"]),
        "audio:wmalossless" => codec(Audio, &["wmalossless"]),
        "audio:wmavoice" => codec(Audio, &["wmavoice"]),
        "audio:flac" => codec(Audio, &["flac"]),
        "audio:alac" => codec(Audio, &["alac"]),
        "audio:speex" => codec(Audio, &["speex"]),
        // SV7 and SV8 are Musepack.
        "audio:musepack" => codec(Audio, &["musepack", "musepack7", "musepack8"]),
        "audio:atrac3" => codec(Audio, &["atrac3"]),
        "audio:atrac3p" => codec(Audio, &["atrac3p", "atrac3plus"]),
        "audio:wavpack" => codec(Audio, &["wavpack"]),
        // OxideAV's planar-output MOD decoder is the MOD decoder.
        "audio:mod" => codec(Audio, &["mod", "mod_planar"]),
        "audio:tta" => codec(Audio, &["tta"]),
        "audio:ape" => codec(Audio, &["ape"]),
        "audio:cook" => codec(Audio, &["cook"]),
        "audio:ra144" => codec(Audio, &["ra_144"]),
        "audio:ra288" => codec(Audio, &["ra_288"]),
        "audio:sipr" => codec(Audio, &["sipr"]),
        "audio:ralf" => codec(Audio, &["ralf"]),
        // G.711 A-law and mu-law under OxideAV's three names each.
        "audio:alaw" => codec(Audio, &["alaw", "pcm_alaw", "g711a"]),
        "audio:ulaw" => codec(Audio, &["ulaw", "pcm_mulaw", "g711u"]),
        "audio:amrnb" => codec(Audio, &["amr_nb", "amrnb"]),
        "audio:amrwb" => codec(Audio, &["amr_wb", "amrwb"]),
        "audio:midi" => codec(Audio, &["midi"]),
        "audio:lpcm" => Some(Rule::Lpcm),
        "audio:adpcm" => Some(Rule::Adpcm),
        "audio:qcelp" => codec(Audio, &["qcelp"]),
        "audio:dvaudio" => codec(Audio, &["dvaudio"]),
        "audio:qdm2" => codec(Audio, &["qdm2"]),
        "audio:qdmc" => codec(Audio, &["qdmc"]),
        "audio:mace3" => codec(Audio, &["mace3"]),
        "audio:mace6" => codec(Audio, &["mace6"]),
        "audio:opus" => codec(Audio, &["opus"]),

        "sub:dvd" => codec(Subtitle, &["vobsub", "dvd_subtitle"]),
        "sub:subrip" => codec(Subtitle, &["subrip", "srt"]),
        "sub:microdvd" => codec(Subtitle, &["microdvd"]),
        // SubViewer 1 and 2 are SubViewer.
        "sub:subviewer" => codec(Subtitle, &["subviewer", "subviewer1", "subviewer2"]),
        "sub:ass" => codec(Subtitle, &["ass", "ssa"]),
        "sub:sami" => codec(Subtitle, &["sami"]),
        "sub:vplayer" => codec(Subtitle, &["vplayer"]),
        "sub:mpl2" => codec(Subtitle, &["mpl2"]),
        "sub:eia608" => codec(Subtitle, &["eia_608", "eia608"]),
        "sub:cea708" => codec(Subtitle, &["cea_708", "cea708"]),
        "sub:usf" => codec(Subtitle, &["usf"]),
        "sub:svcd" => codec(Subtitle, &["svcd", "ogt"]),
        "sub:dvb" => codec(Subtitle, &["dvbsub", "dvb_subtitle"]),
        // OGM subtitles: text streams of an OGM file.
        "sub:ogm" => Some(Rule::Codec(Subtitle, vec![Codec { ids: &["text", "subrip"], tags: &[], ogm: true }])),
        "sub:cmml" => codec(Subtitle, &["cmml"]),
        "sub:kate" => codec(Subtitle, &["kate"]),
        "sub:webvtt" => codec(Subtitle, &["webvtt"]),
        "sub:pgs" => codec(Subtitle, &["pgs", "hdmv_pgs_subtitle"]),
        "sub:tx3g" => codec(Subtitle, &["mov_text"]),

        // MPEG elementary streams: MPEG-1/2 video ES and MPEG audio ES (an
        // MP3 file is one).
        "container:mpeg_es" => container(&["mpegvideo", "mp3"]),
        "container:mpeg_ps" => container(&["mpeg"]),
        "container:mpeg_ts" => container(&["mpegts"]),
        "container:pva" => container(&["pva"]),
        "container:mp3" => container(&["mp3"]),
        "container:avi" => container(&["avi"]),
        "container:asf" => container(&["asf"]),
        "container:mp4" => container(&["mp4"]),
        "container:mov" => container(&["mov"]),
        "container:ogg" => container(&["ogg"]),
        "container:ogm" => Some(Rule::Container { demuxers: &["ogg"], ogm: true }),
        // WebM is a Matroska profile; OxideAV registers it under both names.
        "container:mkv" => container(&["matroska", "webm"]),
        "container:rm" => container(&["rm"]),
        "container:wav" => container(&["wav"]),
        "container:raw_dts" => container(&["dts", "dtshd"]),
        "container:raw_aac" => container(&["adts"]),
        "container:raw_ac3" => container(&["ac3", "eac3"]),
        "container:raw_flac" => container(&["flac"]),
        "container:raw_dv" => container(&["dv", "rawdv"]),
        "container:flv" => container(&["flv"]),
        "container:mxf" => container(&["mxf"]),
        "container:nut" => container(&["nut"]),
        "container:midi" => container(&["smf"]),
        "container:voc" => container(&["voc"]),
        "container:aiff" => container(&["aiff"]),
        "container:caf" => container(&["caf"]),
        _ => None,
    }
}

fn accepts(rule: &Rule, s: &StreamFacts, ogm: bool) -> bool {
    match rule {
        Rule::Codec(kind, codecs) => {
            s.kind == *kind
                && codecs.iter().any(|c| {
                    c.ids.contains(&s.codec.as_str())
                        && (c.tags.is_empty() || s.tag.as_deref().is_some_and(|t| c.tags.contains(&t)))
                        && (!c.ogm || ogm)
                })
        }
        Rule::Lpcm => {
            s.kind == Kind::Audio
                && ((s.codec.starts_with("pcm_") && !matches!(s.codec.as_str(), "pcm_alaw" | "pcm_mulaw"))
                    || s.codec.starts_with("slin"))
        }
        Rule::Adpcm => s.kind == Kind::Audio && s.codec.starts_with("adpcm_"),
        Rule::Container { .. } => false,
    }
}

fn standing(s: &StreamFacts) -> Claim {
    match (s.verdict.as_str(), s.http_ok) {
        (_, false) => Claim::Failed(format!("{} stream ({}) differs over HTTP", s.kind.name(), s.codec)),
        ("PASS", true) => Claim::Verified,
        ("DECODES", true) => Claim::Unverified,
        _ => Claim::Failed(format!("{} stream ({}) failed its comparison", s.kind.name(), s.codec)),
    }
}

/// How `facts` stand on `row`.
pub fn claim(row: &str, facts: &EntryFacts) -> Claim {
    let Some(rule) = rule(row) else {
        return Claim::Failed(format!("no attribution rule for {row}"));
    };
    if let Rule::Container { demuxers, ogm } = rule {
        let Some(demuxer) = facts.demuxer.as_deref() else {
            return Claim::Failed("no demuxer opened the file".into());
        };
        if !demuxers.contains(&demuxer) || (ogm && !facts.ogm) {
            let what = if ogm { format!("OGM via {demuxers:?}") } else { format!("{demuxers:?}") };
            return Claim::Failed(format!("the player demuxed it with {demuxer}, {row} needs {what}"));
        }
        if let Some(why) = &facts.entry_failure {
            return Claim::Failed(why.clone());
        }
        if facts.streams.is_empty() {
            return Claim::Failed("no stream was played and compared".into());
        }
        // Every stream the container delivered must have checked out.
        let mut worst = Claim::Verified;
        for s in &facts.streams {
            match standing(s) {
                Claim::Failed(why) => return Claim::Failed(why),
                Claim::Unverified => worst = Claim::Unverified,
                Claim::Verified => {}
            }
        }
        return worst;
    }
    let matching: Vec<&StreamFacts> = facts.streams.iter().filter(|s| accepts(&rule, s, facts.ogm)).collect();
    if matching.is_empty() {
        let played: Vec<String> = facts.streams.iter().map(|s| format!("{} {}", s.kind.name(), s.codec)).collect();
        return Claim::Failed(format!("no stream decoded as {row} (played: {})", played.join(", ")));
    }
    if let Some(why) = &facts.stream_failure {
        return Claim::Failed(why.clone());
    }
    // The best matching stream decides.
    let standings: Vec<Claim> = matching.iter().map(|s| standing(s)).collect();
    if standings.contains(&Claim::Verified) {
        Claim::Verified
    } else if standings.contains(&Claim::Unverified) {
        Claim::Unverified
    } else {
        standings.into_iter().next().unwrap_or(Claim::Failed(String::new()))
    }
}

/// Whether `path` is an OGM file: an Ogg stream whose first page carries a
/// DirectShow OGM stream header (`\x01video`, `\x01audio` or `\x01text`).
pub fn is_ogm(path: &Path) -> bool {
    let Ok(data) = std::fs::read(path) else { return false };
    let data = &data[..data.len().min(64 * 1024)];
    let mut at = 0;
    while let Some(pos) = data[at..].windows(4).position(|w| w == b"OggS") {
        let page = at + pos;
        at = page + 4;
        let Some(&flags) = data.get(page + 5) else { break };
        let Some(&segments) = data.get(page + 26) else { break };
        let body = page + 27 + segments as usize;
        if flags & 0x02 != 0 {
            let packet = data.get(body..).unwrap_or(&[]);
            if packet.first() == Some(&0x01) && [&b"video"[..], b"audio", b"text"].iter().any(|t| packet[1..].starts_with(t)) {
                return true;
            }
        }
    }
    false
}

/// Every yardstick row has an attribution rule.
pub fn missing_rules(rows: &[String]) -> Vec<String> {
    rows.iter().filter(|r| rule(r).is_none()).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(kind: Kind, codec: &str, verdict: &str) -> StreamFacts {
        StreamFacts { kind, codec: codec.into(), tag: None, verdict: verdict.into(), http_ok: true }
    }

    #[test]
    fn rows_follow_the_decoded_codec_not_the_label() {
        // wmv8_x8intra.wmv: ffprobe says wmav2 + wmv2. Its manifest record
        // claimed wma1/wmv1 too, and the old attribution credited every
        // claimed video row from any passing video stream.
        let facts = EntryFacts {
            demuxer: Some("asf".into()),
            streams: vec![stream(Kind::Audio, "wma2", "FAIL"), stream(Kind::Video, "wmv2", "PASS")],
            ..Default::default()
        };
        assert_eq!(claim("video:wmv2", &facts), Claim::Verified);
        assert!(matches!(claim("video:wmv1", &facts), Claim::Failed(why) if why.contains("no stream decoded as video:wmv1")));
        assert!(matches!(claim("audio:wma1", &facts), Claim::Failed(_)));
        assert!(matches!(claim("audio:wma2", &facts), Claim::Failed(why) if why.contains("failed its comparison")));
        // A failed comparison withdraws the container too.
        assert!(matches!(claim("container:asf", &facts), Claim::Failed(_)));
    }

    #[test]
    fn container_rows_follow_the_demuxer_that_ran() {
        // truehd_5.1.raw claimed container:mpeg_ps, but it is a raw TrueHD
        // stream: the old attribution credited mpeg_ps when no open/engine
        // error was reported.
        let facts = EntryFacts {
            demuxer: Some("truehd".into()),
            streams: vec![stream(Kind::Audio, "truehd", "PASS")],
            ..Default::default()
        };
        assert!(matches!(claim("container:mpeg_ps", &facts), Claim::Failed(why) if why.contains("demuxed it with truehd")));
        assert_eq!(claim("audio:truehd", &facts), Claim::Verified);
        let mkv = EntryFacts { demuxer: Some("webm".into()), ..facts };
        assert_eq!(claim("container:mkv", &mkv), Claim::Verified, "WebM is a Matroska profile");
    }

    #[test]
    fn entry_failures_withdraw_containers_and_mapping_failures_everything() {
        let flac = EntryFacts {
            demuxer: Some("flac".into()),
            streams: vec![stream(Kind::Audio, "flac", "PASS")],
            ..Default::default()
        };
        assert_eq!(claim("container:raw_flac", &flac), Claim::Verified);
        // Another stream had no decoder: this one still passed.
        let engine = EntryFacts { entry_failure: Some("engine: no video decoder".into()), ..flac.clone() };
        assert_eq!(claim("audio:flac", &engine), Claim::Verified);
        assert!(matches!(claim("container:raw_flac", &engine), Claim::Failed(_)));
        // The HTTP pass failed outright: nothing it should confirm is.
        let why = Some("http: GET served other bytes".to_string());
        let http = EntryFacts { entry_failure: why.clone(), stream_failure: why, ..flac };
        assert!(matches!(claim("audio:flac", &http), Claim::Failed(w) if w.contains("other bytes")));
        assert!(matches!(claim("container:raw_flac", &http), Claim::Failed(_)));
    }

    #[test]
    fn alias_rows_need_their_tag() {
        let mut xvid = stream(Kind::Video, "mpeg4video", "PASS");
        let facts = |s: &StreamFacts| EntryFacts { demuxer: Some("avi".into()), streams: vec![s.clone()], ..Default::default() };
        assert!(matches!(claim("video:xvid", &facts(&xvid)), Claim::Failed(_)), "untagged MPEG-4");
        xvid.tag = Some("XVID".into());
        assert_eq!(claim("video:xvid", &facts(&xvid)), Claim::Verified);
        assert_eq!(claim("video:mpeg4", &facts(&xvid)), Claim::Verified);
        assert!(matches!(claim("video:3ivx", &facts(&xvid)), Claim::Failed(_)));
        assert_eq!(claim("video:divx", &facts(&stream(Kind::Video, "msmpeg4v3", "PASS"))), Claim::Verified);
    }

    #[test]
    fn decodes_only_and_http_failures_are_not_verified() {
        let midi = EntryFacts { demuxer: Some("smf".into()), streams: vec![stream(Kind::Audio, "midi", "DECODES")], ..Default::default() };
        assert_eq!(claim("audio:midi", &midi), Claim::Unverified);
        assert_eq!(claim("container:midi", &midi), Claim::Unverified);
        let mut http = stream(Kind::Audio, "flac", "PASS");
        http.http_ok = false;
        let facts = EntryFacts { demuxer: Some("flac".into()), streams: vec![http], ..Default::default() };
        assert!(matches!(claim("audio:flac", &facts), Claim::Failed(why) if why.contains("HTTP")));
    }

    #[test]
    fn lpcm_and_adpcm_are_families_and_g711_is_not_lpcm() {
        let facts = |codec: &str| EntryFacts { demuxer: Some("wav".into()), streams: vec![stream(Kind::Audio, codec, "PASS")], ..Default::default() };
        assert_eq!(claim("audio:lpcm", &facts("pcm_s16le")), Claim::Verified);
        assert!(matches!(claim("audio:lpcm", &facts("pcm_alaw")), Claim::Failed(_)));
        assert_eq!(claim("audio:adpcm", &facts("adpcm_ms")), Claim::Verified);
    }

    #[test]
    fn ogm_files_are_recognised_by_their_stream_headers() {
        assert!(is_ogm(&refcheck::fate("ogg-ogm/bots01.ogm")));
        assert!(!is_ogm(&refcheck::fate("ogg-vorbis/tos.ogg")));
    }

    #[test]
    fn every_yardstick_row_has_a_rule() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus");
        let yardstick: toml::Value = toml::from_str(&std::fs::read_to_string(dir.join("yardstick.toml")).unwrap()).unwrap();
        let rows: Vec<String> = yardstick["rows"].as_array().unwrap().iter().map(|r| r.as_str().unwrap().to_string()).collect();
        assert_eq!(missing_rules(&rows), Vec::<String>::new());
    }
}
