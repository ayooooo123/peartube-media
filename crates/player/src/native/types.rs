//! What the native route knows about the streams it serves: filled from
//! the codec configuration (`codec.rs`), written into the init segment
//! (`fmp4.rs`) and the playlists (`hls.rs`).

/// The video codecs AVPlayer decodes from fMP4.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VideoCodec {
    /// Sample entry `avc1`, record `avcC`.
    H264,
    /// Sample entry `hvc1` (parameter sets in the record only), record `hvcC`.
    Hevc,
}

/// HLS `VIDEO-RANGE`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum VideoRange {
    #[default]
    Sdr,
    /// transfer_characteristics 16 (SMPTE ST 2084).
    Pq,
    /// transfer_characteristics 18 (ARIB STD-B67).
    Hlg,
}

/// The `colr` box, colour type `nclx` (ISO/IEC 23001-8 code points).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Colour {
    pub primaries: u16,
    pub transfer: u16,
    pub matrix: u16,
    pub full_range: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct VideoConfig {
    pub codec: VideoCodec,
    /// Coded size from the SPS after cropping.
    pub width: u32,
    pub height: u32,
    /// The `avcC` / `hvcC` box payload. hvcC keeps only its VPS, SPS and
    /// PPS arrays (iOS rejects records carrying SEI arrays).
    pub record: Vec<u8>,
    /// Byte width of the NAL length prefixes in the samples (1, 2 or 4),
    /// as the record declares.
    pub nal_length_size: u8,
    /// RFC 6381 `CODECS` entry: `avc1.PPCCLL`, `hvc1.<profile>.<compat>.<tier><level>.<constraints>`.
    pub codec_string: String,
    pub range: VideoRange,
    pub colour: Option<Colour>,
    /// `mdcv` payload (ST 2086 mastering display, 24 bytes).
    pub mastering_display: Option<[u8; 24]>,
    /// `clli` payload (MaxCLL, MaxFALL, 4 bytes).
    pub content_light: Option<[u8; 4]>,
    /// `pasp`: horizontal and vertical spacing, when not square.
    pub pixel_aspect: Option<(u32, u32)>,
    /// Frames per second, from the VUI timing or the container.
    pub frame_rate: Option<f64>,
    /// Pictures a decoder may hold back before output (H.264
    /// max_num_reorder_frames, HEVC sps_max_num_reorder_pics).
    pub reorder_depth: u32,
    /// Field-coded or interlaced: AVPlayer does not deinterlace.
    pub interlaced: bool,
}

/// The audio codecs the native route copies into fMP4 unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AudioCodec {
    /// Sample entry `mp4a`, config box `esds`.
    Aac,
    /// Sample entry `ac-3`, config box `dac3`.
    Ac3,
    /// Sample entry `ec-3`, config box `dec3`.
    Eac3,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AudioConfig {
    pub codec: AudioCodec,
    /// The track's timescale: the AAC core rate from the
    /// AudioSpecificConfig (not the SBR output rate), the AC-3/E-AC-3 rate.
    pub sample_rate: u32,
    pub channels: u16,
    /// The config box payload: `esds` (FullBox: version and flags, then the
    /// ES_Descriptor), `dac3` or `dec3`.
    pub config: Vec<u8>,
    /// RFC 6381 `CODECS` entry: `mp4a.40.2`, `mp4a.40.5`, `ac-3`, `ec-3`.
    pub codec_string: String,
    /// Samples per access unit at `sample_rate` (AAC 1024 or 960, AC-3
    /// 1536); 0 for E-AC-3, whose frames say it (`codec::eac3_frame_samples`).
    pub frame_samples: u32,
    /// The packets start with ADTS headers to strip.
    pub adts: bool,
    /// ISO 639-2/T language for `mdhd`, e.g. `eng`.
    pub language: Option<String>,
}
