//! The shape of one pixel, from the video codec's own headers. Video stored
//! anamorphic (a DVD's 720x480 shown at 16:9, a 1440x1080 broadcast shown
//! 1920 wide) only looks right when the picture is stretched by it, and the
//! coded size alone cannot say so: the H.264/HEVC VUI, the MPEG-2 sequence
//! header and the MPEG-4 Part 2 VOL do. They sit in the container's setup
//! bytes or, for streams that carry them inline (MPEG-PS/TS, AVI), in the
//! first packets.

use oxideav_core::CodecParameters;

/// Whether `params`' codec has a header [`pixel_aspect`] reads.
pub(crate) fn readable(params: &CodecParameters) -> bool {
    matches!(params.codec_id.as_str(), "h264" | "h265" | "hevc" | "mpeg2video" | "mpeg2" | "mpeg4video" | "mpeg4" | "mp4v-es")
}

/// The pixel's width over its height as `(h, v)`, from the setup bytes or
/// `data` (a packet): `Some(None)` for square pixels, `None` while no header
/// has been seen.
pub(crate) fn pixel_aspect(params: &CodecParameters, data: &[u8]) -> Option<Option<(u32, u32)>> {
    let found = match params.codec_id.as_str() {
        "h264" | "h265" | "hevc" => crate::native::codec::video_config(params, data).map(|c| c.pixel_aspect),
        "mpeg2video" | "mpeg2" => mpeg2(&params.extradata).or_else(|| mpeg2(data)),
        "mpeg4video" | "mpeg4" | "mp4v-es" => mpeg4(&params.extradata).or_else(|| mpeg4(data)),
        _ => None,
    }?;
    // A malformed header must not squash the picture to a sliver.
    Some(found.map(reduce).filter(|&(h, v)| h != v && h <= v * 4 && v <= h * 4))
}

fn reduce((h, v): (u32, u32)) -> (u32, u32) {
    let (mut a, mut b) = (h, v);
    while b != 0 {
        (a, b) = (b, a % b);
    }
    if a == 0 { (h, v) } else { (h / a, v / a) }
}

/// Offsets just past each `00 00 01 <code>` start code in `data`.
fn start_codes<'a>(data: &'a [u8], code: impl Fn(u8) -> bool + 'a) -> impl Iterator<Item = usize> + 'a {
    (0..data.len().saturating_sub(3))
        .filter(move |&i| data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 && code(data[i + 3]))
        .map(|i| i + 4)
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn read(&mut self, n: usize) -> Option<u32> {
        let mut value = 0;
        for _ in 0..n {
            let byte = *self.data.get(self.pos / 8)?;
            value = (value << 1) | u32::from(byte >> (7 - self.pos % 8) & 1);
            self.pos += 1;
        }
        Some(value)
    }
}

/// ISO/IEC 13818-2 6.3.3: `aspect_ratio_information` is the display's shape
/// (4:3, 16:9, 2.21:1), over the display size when a sequence display
/// extension gives one and the coded size otherwise.
fn mpeg2(data: &[u8]) -> Option<Option<(u32, u32)>> {
    let header = start_codes(data, |c| c == 0xB3).next()?;
    let mut bits = Bits { data, pos: header * 8 };
    let (mut width, mut height) = (bits.read(12)?, bits.read(12)?);
    let display: (u32, u32) = match bits.read(4)? {
        2 => (4, 3),
        3 => (16, 9),
        4 => (221, 100),
        _ => return Some(None),
    };
    // sequence_display_extension (identifier 2): 6.2.2.4.
    if let Some(ext) = start_codes(data, |c| c == 0xB5).find(|&at| data.get(at).is_some_and(|b| b >> 4 == 2)) {
        let mut bits = Bits { data, pos: ext * 8 + 4 };
        let colour = bits.read(3).and_then(|_| bits.read(1));
        if colour == Some(1) {
            bits.read(24);
        }
        if let (Some(w), Some(_), Some(h)) = (bits.read(14), bits.read(1), bits.read(14)) {
            if w > 0 && h > 0 {
                (width, height) = (w, h);
            }
        }
    }
    if width == 0 || height == 0 {
        return None;
    }
    Some(Some((display.0 * height, display.1 * width)))
}

/// ISO/IEC 14496-2 6.2.3 / table 6-12: the VOL's `aspect_ratio_info`.
fn mpeg4(data: &[u8]) -> Option<Option<(u32, u32)>> {
    let vol = start_codes(data, |c| (0x20..=0x2F).contains(&c)).next()?;
    let mut bits = Bits { data, pos: vol * 8 };
    bits.read(9)?; // random_accessible_vol, video_object_type_indication
    if bits.read(1)? == 1 {
        bits.read(7)?; // video_object_layer_verid, video_object_layer_priority
    }
    Some(match bits.read(4)? {
        2 => Some((12, 11)),
        3 => Some((10, 11)),
        4 => Some((16, 11)),
        5 => Some((40, 33)),
        15 => match (bits.read(8)?, bits.read(8)?) {
            (h, v) if h > 0 && v > 0 => Some((h, v)),
            _ => None,
        },
        _ => None,
    })
}
