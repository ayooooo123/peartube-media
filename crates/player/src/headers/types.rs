//! Parsed codec headers and stream metadata for direct playback.

/// Supported video codecs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VideoCodec {
    /// Sample entry `avc1`, record `avcC`.
    H264,
    /// Sample entry `hvc1` (parameter sets in the record only), record `hvcC`.
    Hevc,
    /// Sample entry `av01`, record `av1C`.
    Av1,
}

/// Video transfer-function range.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum VideoRange {
    #[default]
    Sdr,
    /// transfer_characteristics 16 (SMPTE ST 2084).
    Pq,
    /// transfer_characteristics 18 (ARIB STD-B67).
    Hlg,
}

/// Colour characteristics using ISO/IEC 23001-8 code points.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Colour {
    pub primaries: u16,
    pub transfer: u16,
    pub matrix: u16,
    pub full_range: bool,
}

/// Dolby Vision configuration (`dvcC` for profile <= 7, `dvvC` for profile > 7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DoviConfig {
    pub profile: u8,
    pub level: u8,
    pub compat_id: u8,
    /// 24-byte DOVIDecoderConfigurationRecord payload.
    pub record: [u8; 24],
    /// Sample entry FourCC is `dvh1` (Profile 5) instead of `hvc1`.
    pub use_dvh1: bool,
    /// The source is Profile 7 and would require RPU conversion for Profile 8.1.
    pub convert_p7_to_81: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct VideoConfig {
    pub codec: VideoCodec,
    /// Coded size from the SPS after cropping.
    pub width: u32,
    pub height: u32,
    /// The `avcC` / `hvcC` / `av1C` box payload. hvcC keeps only its VPS, SPS
    /// and PPS arrays (iOS rejects records carrying SEI arrays).
    pub record: Vec<u8>,
    /// Byte width of the NAL length prefixes in the samples (1, 2 or 4),
    /// as the record declares.
    pub nal_length_size: u8,
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
    pub dovi: Option<DoviConfig>,
}

/// Supported compressed-audio codec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AudioCodec {
    /// Sample entry `ac-3`, config box `dac3`.
    Ac3,
    /// Sample entry `ec-3`, config box `dec3`.
    Eac3,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AudioConfig {
    pub codec: AudioCodec,
    /// Audio sample rate in Hz.
    pub sample_rate: u32,
    pub channels: u16,
    /// Decoded bits per sample.
    pub bits: u16,
    /// E-AC-3 dec3 configuration payload.
    pub config: Vec<u8>,
    /// E-AC-3 Dolby Atmos (JOC) object count / complexity index.
    pub atmos_joc: Option<u8>,
}
