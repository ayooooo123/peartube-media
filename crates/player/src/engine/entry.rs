//! Where a seek starts decoding video.
//!
//! A seek lands where the container marks random access: Matroska's
//! keyframe bit and Cues, MP4's sync samples, a transport stream's key
//! packets. For H.264 and HEVC such a mark is not always a picture a decoder
//! can start from. A non-IDR I picture without a recovery point keeps the
//! reference pictures before it, and the pictures after it may predict from
//! them (x264's open-GOP I frames once their recovery-point SEI is gone,
//! encoders that mark every I frame). Decoding from there loses or damages
//! those pictures; FFmpeg's decoder shows nothing until it recovers. FFmpeg's
//! `-ss` gets every picture from the target on because it lands on an
//! earlier point (fftools seeks 3/23 s before the target when a stream has
//! a decoding delay).
//!
//! [`entry`] tells a picture a decoder can start from (IDR, a recovery
//! point, an HEVC IRAP picture) from one that depends on earlier pictures.
//! A seek whose landing is a dependent picture goes back to the random
//! access point before it until it reaches one to start from, at most
//! [`LOOKBACK_SECS`] before the target, and decodes from there; the
//! pictures before the target are dropped as after any seek. Every other
//! codec starts at its keyframes.
//!
//! An H.264 recovery point whose `recovery_frame_cnt` is above zero (a
//! gradual decoding refresh such as x264's `--intra-refresh`, where a
//! column of intra blocks sweeps the picture) is complete only that many
//! frames later: the pictures shown before then are partly built from
//! references the decoder never had. FFmpeg's decoder outputs none of them
//! (h264_slice.c `h264_field_start` and `h264_select_output_frame`).
//! [`Recovery`] finds the picture where it recovers. A seek whose target
//! comes before that goes back to the random-access point before it too;
//! where it cannot go back further, the pictures before the recovery are
//! not shown, as FFmpeg shows none.

use oxideav_core::CodecParameters;

/// How far before its target a seek goes back for a picture to start from:
/// the longest IDR distance of common encodes (x264's default 250 frames at
/// 25 fps). Past it the seek starts at the earliest random-access point it
/// reached.
pub(super) const LOOKBACK_SECS: f64 = 10.0;

/// What decoding from a random-access packet gives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Entry {
    /// Every picture from this one on, in presentation order, decodes as
    /// from the start of the stream.
    Refresh,
    /// An H.264 recovery point with `recovery_frame_cnt` of `frames`, above
    /// zero: the pictures are complete from the one [`Recovery`] finds.
    Recovery { frames: u32 },
    /// Pictures after this one may predict from pictures before it.
    Dependent,
}

/// Whether [`entry`] can tell anything but [`Entry::Refresh`] for the
/// codec of `params`.
pub(super) fn checked(params: &CodecParameters) -> bool {
    matches!(params.codec_id.as_str(), "h264" | "hevc" | "h265")
}

/// What starting to decode at `data`, a random-access packet of a stream of
/// `params`, gives. H.264: an IDR picture, or a recovery-point SEI (FFmpeg's
/// keyframes) with a `recovery_frame_cnt` of zero, starts one; a larger
/// count recovers later. HEVC: an IRAP picture (IDR, CRA, BLA; a CRA's
/// leading pictures precede it on screen) or a recovery-point SEI. A packet
/// with no picture of its own, or one that does not parse, is taken as the
/// container says.
pub(super) fn entry(params: &CodecParameters, data: &[u8]) -> Entry {
    match params.codec_id.as_str() {
        "h264" => {
            let unit = h264_access_unit(params, data);
            match unit.recovery {
                _ if unit.idr => Entry::Refresh,
                Some(0) => Entry::Refresh,
                Some(frames) => Entry::Recovery { frames },
                None if unit.picture => Entry::Dependent,
                None => Entry::Refresh,
            }
        }
        "hevc" | "h265" => hevc(&nals(data, hevc_framing(&params.extradata))),
        _ => Entry::Refresh,
    }
}

/// Whether no picture predicts from the picture `data` holds: an H.264
/// access unit whose slices all have `nal_ref_idc` 0. Such a picture before
/// a seek's target is not decoded at all (FFmpeg's `skip_frame=nonref`,
/// mpv's `hr-seek-framedrop`). False for anything else.
pub(super) fn non_reference(params: &CodecParameters, data: &[u8]) -> bool {
    if params.codec_id.as_str() != "h264" {
        return false;
    }
    let unit = h264_access_unit(params, data);
    unit.picture && !unit.reference
}

/// Follows an H.264 stream in decoding order from a recovery point
/// ([`Entry::Recovery`]) to the picture FFmpeg's decoder marks recovered
/// (h264_slice.c `h264_field_start`): the reference picture whose
/// `frame_num` is `recovery_frame_cnt` past the recovery point's, an IDR
/// picture, or the sooner one a later recovery point names. FFmpeg outputs
/// no picture shown before it. `frame_num` grows by one after each
/// reference picture, which this counts (frame pictures without
/// `frame_num` gaps).
pub(super) struct Recovery {
    /// The next picture's `frame_num`, counted from the recovery point's.
    frame_num: u32,
    /// The recovered picture's `frame_num`, counted the same way.
    recovers: u32,
}

impl Recovery {
    pub(super) fn new() -> Recovery {
        Recovery { frame_num: 0, recovers: u32::MAX }
    }

    /// The next access unit in decoding order, the recovery point's own
    /// first: whether it holds the recovered picture.
    pub(super) fn recovered(&mut self, params: &CodecParameters, data: &[u8]) -> bool {
        let unit = h264_access_unit(params, data);
        if !unit.picture {
            return false;
        }
        if let Some(frames) = unit.recovery {
            self.recovers = self.recovers.min(self.frame_num.saturating_add(frames));
        }
        let recovered = unit.idr || (unit.reference && self.frame_num == self.recovers);
        if unit.reference {
            self.frame_num = self.frame_num.saturating_add(1);
        }
        recovered
    }
}

/// What one H.264 access unit holds, as far as starting to decode goes.
#[derive(Default)]
struct AccessUnit {
    /// It has a slice.
    picture: bool,
    /// A slice has `nal_ref_idc` above zero.
    reference: bool,
    /// It has an IDR slice.
    idr: bool,
    /// Its recovery-point SEI's `recovery_frame_cnt`; FFmpeg ignores one of
    /// 2^16 or more (h264_sei.c).
    recovery: Option<u32>,
}

fn h264_access_unit(params: &CodecParameters, data: &[u8]) -> AccessUnit {
    let mut unit = AccessUnit::default();
    for nal in nals(data, h264_framing(&params.extradata)) {
        let Some(&header) = nal.first() else { continue };
        match header & 0x1F {
            // non-IDR slice, slice data partition A to C, IDR slice
            kind @ 1..=5 => {
                unit.picture = true;
                unit.reference |= header & 0x60 != 0;
                unit.idr |= kind == 5;
            }
            // SEI
            6 if unit.recovery.is_none() => {
                unit.recovery = sei_payload(&rbsp(&nal[1..]), 6).and_then(ue).filter(|&count| count < 1 << 16);
            }
            _ => {}
        }
    }
    unit
}

fn hevc(nals: &[&[u8]]) -> Entry {
    let mut picture = false;
    for nal in nals {
        if nal.len() < 2 {
            continue;
        }
        match (nal[0] >> 1) & 0x3F {
            // IRAP: BLA, IDR, CRA and the reserved IRAP types
            16..=23 => return Entry::Refresh,
            // prefix SEI
            39 if sei_payload(&rbsp(&nal[2..]), 6).is_some() => return Entry::Refresh,
            0..=31 => picture = true,
            _ => {}
        }
    }
    if picture { Entry::Dependent } else { Entry::Refresh }
}

/// How a stream's packets frame their NAL units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Framing {
    /// Start codes (MPEG-TS, raw streams).
    AnnexB,
    /// Big-endian lengths of this many bytes (MP4, Matroska).
    Length(usize),
}

/// AVCDecoderConfigurationRecord extradata (configurationVersion 1, as
/// FFmpeg's h264 decoder tells it) frames NAL units by length; anything
/// else is Annex B. The packets are never sniffed: a 4-byte length of 256
/// to 511 starts with `00 00 01`.
fn h264_framing(extradata: &[u8]) -> Framing {
    match extradata {
        [1, _, _, _, size, ..] => Framing::Length(usize::from(size & 3) + 1),
        _ => Framing::AnnexB,
    }
}

/// HEVCDecoderConfigurationRecord extradata (FFmpeg's hevc decoder takes
/// extradata not starting with a start code as one) frames NAL units by
/// length.
fn hevc_framing(extradata: &[u8]) -> Framing {
    if extradata.len() < 23 || extradata.starts_with(&[0, 0, 1]) || extradata.starts_with(&[0, 0, 0, 1]) {
        Framing::AnnexB
    } else {
        Framing::Length(usize::from(extradata[21] & 3) + 1)
    }
}

/// The NAL units of one packet.
fn nals(data: &[u8], framing: Framing) -> Vec<&[u8]> {
    let mut out = Vec::new();
    match framing {
        Framing::Length(size) => {
            let mut at = 0;
            while at + size <= data.len() {
                let len = data[at..at + size].iter().fold(0usize, |n, &b| (n << 8) | usize::from(b));
                at += size;
                let end = at.saturating_add(len).min(data.len());
                out.push(&data[at..end]);
                at = end;
            }
        }
        Framing::AnnexB => {
            let starts: Vec<usize> = (0..data.len().saturating_sub(2))
                .filter(|&i| data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1)
                .map(|i| i + 3)
                .collect();
            for (k, &start) in starts.iter().enumerate() {
                let end = starts.get(k + 1).map_or(data.len(), |&next| next - 3);
                out.push(&data[start..end.max(start)]);
            }
        }
    }
    out
}

/// A NAL unit's payload without its emulation-prevention bytes.
fn rbsp(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len());
    let mut zeros = 0;
    for &b in payload {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// The payload of an SEI RBSP's first message of `payload_type` (6, the
/// recovery point, in both H.264 and HEVC).
fn sei_payload(rbsp: &[u8], payload_type: usize) -> Option<&[u8]> {
    let mut at = 0;
    // more_rbsp_data: messages until the stop bit.
    while at < rbsp.len() && rbsp[at..] != [0x80] {
        let (kind, next) = sei_number(rbsp, at)?;
        let (size, next) = sei_number(rbsp, next)?;
        let end = next.saturating_add(size);
        if kind == payload_type {
            return rbsp.get(next..end.min(rbsp.len()));
        }
        at = end;
    }
    None
}

/// The Exp-Golomb `ue(v)` at the start of `bits`.
fn ue(bits: &[u8]) -> Option<u32> {
    let bit = |i: usize| bits.get(i / 8).map(|byte| (byte >> (7 - i % 8)) & 1);
    let mut zeros = 0;
    while bit(zeros)? == 0 {
        zeros += 1;
        if zeros > 31 {
            return None;
        }
    }
    let mut value = 1u64;
    for i in 0..zeros {
        value = (value << 1) | u64::from(bit(zeros + 1 + i)?);
    }
    u32::try_from(value - 1).ok()
}

/// An SEI payload type or size: 0xFF bytes adding 255 each, then the last
/// byte. The value and where the next field starts.
fn sei_number(rbsp: &[u8], mut at: usize) -> Option<(usize, usize)> {
    let mut value = 0usize;
    loop {
        let byte = *rbsp.get(at)?;
        at += 1;
        value = value.saturating_add(usize::from(byte));
        if byte != 0xFF {
            return Some((value, at));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::CodecId;

    fn params(codec: &str, extradata: &[u8]) -> CodecParameters {
        let mut p = CodecParameters::video(CodecId::new(codec));
        p.extradata = extradata.to_vec();
        p
    }

    fn length_prefixed(nals: &[&[u8]]) -> Vec<u8> {
        nals.iter().flat_map(|n| (n.len() as u32).to_be_bytes().into_iter().chain(n.iter().copied())).collect()
    }

    fn annex_b(nals: &[&[u8]]) -> Vec<u8> {
        nals.iter().flat_map(|n| [0, 0, 0, 1].into_iter().chain(n.iter().copied())).collect()
    }

    const AVCC: &[u8] = &[1, 0x64, 0, 0x1F, 0xFF, 0xE1];
    const IDR: &[u8] = &[0x65, 0x88, 0x84];
    const NON_IDR_I: &[u8] = &[0x41, 0x9A, 0x00];
    const AUD: &[u8] = &[0x09, 0x10];
    /// x264's version string SEI (user data unregistered), then a recovery
    /// point: the second message is the one that counts.
    const SEI_USER_THEN_RECOVERY: &[u8] = &[0x06, 0x05, 0x02, 0xAB, 0xCD, 0x06, 0x01, 0xC4, 0x80];
    const SEI_BUFFERING_PERIOD: &[u8] = &[0x06, 0x00, 0x01, 0x80, 0x80];

    #[test]
    fn h264_idr_and_recovery_points_start_decoding_other_i_frames_depend() {
        let p = params("h264", AVCC);
        assert_eq!(entry(&p, &length_prefixed(&[AUD, IDR])), Entry::Refresh);
        assert_eq!(entry(&p, &length_prefixed(&[SEI_USER_THEN_RECOVERY, NON_IDR_I])), Entry::Refresh);
        assert_eq!(entry(&p, &length_prefixed(&[SEI_BUFFERING_PERIOD, NON_IDR_I])), Entry::Dependent);
        assert_eq!(entry(&p, &length_prefixed(&[AUD, NON_IDR_I])), Entry::Dependent);
        // No picture: the container's mark stands.
        assert_eq!(entry(&p, &length_prefixed(&[AUD])), Entry::Refresh);
    }

    #[test]
    fn h264_without_avcc_extradata_is_annex_b() {
        let p = params("h264", &[]);
        assert_eq!(entry(&p, &annex_b(&[AUD, NON_IDR_I])), Entry::Dependent);
        assert_eq!(entry(&p, &annex_b(&[AUD, IDR])), Entry::Refresh);
        // A 3-byte start code too.
        let mut short = vec![0, 0, 1];
        short.extend_from_slice(IDR);
        assert_eq!(entry(&p, &short), Entry::Refresh);
    }

    /// An emulation-prevention byte inside an SEI payload ahead of the
    /// recovery point must not shift the message boundaries.
    #[test]
    fn h264_sei_parsing_drops_emulation_prevention() {
        let p = params("h264", AVCC);
        // payload type 5, size 3: 00 00 03 01 escapes the payload 00 00 01.
        let sei: &[u8] = &[0x06, 0x05, 0x03, 0x00, 0x00, 0x03, 0x01, 0x06, 0x01, 0xC4, 0x80];
        assert_eq!(entry(&p, &length_prefixed(&[sei, NON_IDR_I])), Entry::Refresh);
    }

    #[test]
    fn hevc_irap_pictures_start_decoding_trailing_pictures_depend() {
        let hvcc = {
            let mut e = vec![1u8; 23];
            e[21] = 0x03;
            e
        };
        let p = params("hevc", &hvcc);
        let cra: &[u8] = &[21 << 1, 1, 0xAF];
        let idr: &[u8] = &[19 << 1, 1, 0xAF];
        let trail_r: &[u8] = &[1 << 1, 1, 0xD0];
        let recovery: &[u8] = &[39 << 1, 1, 0x06, 0x01, 0x80, 0x80];
        assert_eq!(entry(&p, &length_prefixed(&[cra])), Entry::Refresh);
        assert_eq!(entry(&p, &length_prefixed(&[idr])), Entry::Refresh);
        assert_eq!(entry(&p, &length_prefixed(&[trail_r])), Entry::Dependent);
        assert_eq!(entry(&p, &length_prefixed(&[recovery, trail_r])), Entry::Refresh);
        assert_eq!(entry(&params("hevc", &[]), &annex_b(&[trail_r])), Entry::Dependent);
    }

    /// Only an access unit whose every slice has nal_ref_idc 0 is one no
    /// other picture predicts from.
    #[test]
    fn h264_non_reference_pictures() {
        let p = params("h264", AVCC);
        let b_nonref: &[u8] = &[0x01, 0x9E, 0x00];
        let b_ref: &[u8] = &[0x21, 0x9E, 0x00];
        assert!(non_reference(&p, &length_prefixed(&[AUD, b_nonref, b_nonref])));
        assert!(!non_reference(&p, &length_prefixed(&[AUD, b_nonref, b_ref])));
        assert!(!non_reference(&p, &length_prefixed(&[NON_IDR_I])));
        assert!(!non_reference(&p, &length_prefixed(&[AUD])));
        assert!(!non_reference(&params("hevc", &[]), &annex_b(&[&[1 << 1, 1, 0xD0]])));
    }

    /// A recovery point with recovery_frame_cnt 2 recovers at the reference
    /// picture two frame_nums on; non-reference pictures do not advance
    /// frame_num.
    #[test]
    fn h264_recovery_point_with_a_count_recovers_that_many_reference_frames_later() {
        let p = params("h264", AVCC);
        // recovery_frame_cnt ue(v) 2 = 011, exact_match 1, broken_link 0,
        // changing_slice_group_idc 00, payload alignment 1.
        let recovery_2: &[u8] = &[0x06, 0x06, 0x01, 0x71, 0x80];
        let p_ref: &[u8] = &[0x41, 0x9A, 0x00];
        let b_nonref: &[u8] = &[0x01, 0x9E, 0x00];
        let entry_unit = length_prefixed(&[AUD, recovery_2, p_ref]);
        assert_eq!(entry(&p, &entry_unit), Entry::Recovery { frames: 2 });
        let mut recovery = Recovery::new();
        let order = [&entry_unit, &length_prefixed(&[b_nonref]), &length_prefixed(&[p_ref]),
            &length_prefixed(&[b_nonref]), &length_prefixed(&[p_ref])];
        let recovered: Vec<bool> = order.iter().map(|unit| recovery.recovered(&p, unit)).collect();
        assert_eq!(recovered, [false, false, false, false, true]);
    }

    /// A later recovery point that names a sooner picture, or an IDR
    /// picture, ends the recovery there, as in FFmpeg.
    #[test]
    fn h264_recovery_ends_early_at_a_sooner_recovery_point_or_an_idr() {
        let p = params("h264", AVCC);
        let recovery_9: &[u8] = &[0x06, 0x06, 0x01, 0x15, 0x80];
        let p_ref: &[u8] = &[0x41, 0x9A, 0x00];
        let recovery_0: &[u8] = &[0x06, 0x06, 0x01, 0xC4, 0x80];
        let mut recovery = Recovery::new();
        assert_eq!(entry(&p, &length_prefixed(&[recovery_9, p_ref])), Entry::Recovery { frames: 9 });
        assert!(!recovery.recovered(&p, &length_prefixed(&[recovery_9, p_ref])));
        assert!(recovery.recovered(&p, &length_prefixed(&[recovery_0, p_ref])));
        let mut recovery = Recovery::new();
        assert!(!recovery.recovered(&p, &length_prefixed(&[recovery_9, p_ref])));
        assert!(recovery.recovered(&p, &length_prefixed(&[AUD, IDR])));
    }
}
