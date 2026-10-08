// ATSC A/53 closed-caption extraction from H.264, HEVC and MPEG-1/2 video.
//
// Ported from FFmpeg (commit 2da55bf), all LGPL-2.1-or-later:
// - libavcodec/atsc_a53.c: ff_parse_a53_cc
// - libavcodec/itut35.c: ff_itut_t35_parse_buffer (the ATSC "GA94" path)
// - libavcodec/h264_sei.c, hevc/sei.c, h2645_sei.c: the SEI message walks
//   that reach user_data_registered_itu_t_t35
// - libavcodec/h2645_parse.c, h2645_parse.h: NAL splitting (Annex B and
//   length-prefixed), emulation-prevention removal and the RBSP bit length
// - libavcodec/mpeg12dec.c: mpeg_decode_a53_cc and mpeg_decode_user_data
//   (A/53 Part 4, SCTE-20, DVD and Dish Network user data), and the parts
//   of decode_chunks, mpeg_field_start and mpegvideo_dec.c's
//   ff_mpv_frame_start that decide which picture the data lands on.

//! ATSC A/53 caption data (`cc_data` triplets) carried in video.
//!
//! FFmpeg exports a picture's caption data as `AV_FRAME_DATA_A53_CC` side
//! data: the triplets of every caption message read since the previous
//! picture took its data. [`CcExtractor`] reproduces that per access unit:
//! [`CcExtractor::extract`] returns the triplets FFmpeg attaches to the
//! picture(s) the access unit starts, in decode order.
//!
//! - H.264 and HEVC: `user_data_registered_itu_t_t35` SEI messages with
//!   country code 0xB5, provider 0x0031 and identifier `GA94`, in prefix SEI
//!   NAL units. An access unit's data goes to its own picture.
//! - MPEG-1/2: picture user data in A/53 Part 4 (`GA94`), SCTE-20, DVD
//!   (`CC\x01\xf8`) or Dish Network form; the first form seen is the only
//!   one read after it, as in FFmpeg. Data goes to the next picture that
//!   starts decoding: the second field of a field pair passes its data to
//!   the next frame, and B- or P-pictures FFmpeg skips at the start (an
//!   open GOP's leading B-pictures) pass theirs on as well.

/// The video codecs that carry A/53 captions, by FFmpeg codec id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptionCarrier {
    /// `h264`
    H264,
    /// `hevc` (OxideAV's Matroska demuxer says `h265`)
    Hevc,
    /// `mpeg1video` or `mpeg2video`
    Mpeg12,
}

impl CaptionCarrier {
    /// The carrier for a codec id, if it can carry A/53 captions.
    pub fn from_codec_id(codec: &str) -> Option<Self> {
        match codec {
            "h264" => Some(Self::H264),
            "hevc" | "h265" => Some(Self::Hevc),
            "mpeg1video" | "mpeg2video" => Some(Self::Mpeg12),
            _ => None,
        }
    }
}

/// mpeg12dec.c A53_MAX_CC_COUNT: the most triplets one picture's MPEG-2
/// user data may collect. The H.264/HEVC buffer has no such cap in FFmpeg
/// (only INT_MAX); untrusted input gets the same cap there.
const MAX_PENDING_BYTES: usize = 3 * 2000;

/// Extracts the A/53 caption triplets of one access unit of `codec` (an
/// FFmpeg codec id: `h264`, `hevc`, `mpeg1video`, `mpeg2video`), on its
/// own, without the state of the stream around it. H.264/HEVC input may
/// be Annex B (start codes) or 4-byte length-prefixed (AVCC/HVCC): NAL
/// units behind 4-byte lengths that fill the access unit exactly are read
/// as length-prefixed, anything else as Annex B. Use a [`CcExtractor`] for
/// a stream: it carries what FFmpeg carries between access units, and
/// reads the NAL length size from the extradata.
pub fn extract_a53(codec: &str, access_unit: &[u8]) -> Vec<[u8; 3]> {
    let Some(mut extractor) = CcExtractor::new(codec, &[]) else {
        return Vec::new();
    };
    if extractor.carrier != CaptionCarrier::Mpeg12 && filled_by_lengths(access_unit, 4) {
        extractor.nal_length_size = 4;
        extractor.length_prefixed = true;
    }
    let mut triplets = extractor.extract(access_unit);
    triplets.extend(extractor.finish());
    triplets
}

/// `buf` is NAL units behind `size`-byte big-endian lengths, back to back,
/// and nothing else.
fn filled_by_lengths(buf: &[u8], size: usize) -> bool {
    let mut pos = 0usize;
    while pos < buf.len() {
        let Some(prefix) = buf.get(pos..pos + size) else { return false };
        let len = prefix.iter().fold(0usize, |n, &b| (n << 8) | usize::from(b));
        pos += size;
        if len == 0 || len > buf.len() - pos {
            return false;
        }
        pos += len;
    }
    !buf.is_empty()
}

/// Per-stream A/53 caption extraction (see the module docs).
#[derive(Clone, Debug)]
pub struct CcExtractor {
    carrier: CaptionCarrier,
    /// H.264/HEVC NAL length size from avcC/hvcC extradata; 0: none.
    nal_length_size: usize,
    /// The NAL units are length-prefixed (h264dec `is_avc`, hevcdec
    /// `is_nalff`), not Annex B.
    length_prefixed: bool,
    /// Caption bytes read but not yet attached to a picture
    /// (`a53_buf_ref` / `itut_t35.a53_cc`).
    pending: Vec<u8>,
    mpeg: Mpeg12State,
}

/// mpeg12dec.c `enum Mpeg2ClosedCaptionsFormat`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum CcFormat {
    #[default]
    Auto,
    A53Part4,
    Scte20,
    Dvd,
    Dish,
}

const PICT_FRAME: u8 = 3;
const PICT_TYPE_I: u8 = 1;
const PICT_TYPE_P: u8 = 2;
const PICT_TYPE_B: u8 = 3;

/// The MPEG-1/2 decoder state that decides where caption data lands.
#[derive(Clone, Debug)]
struct Mpeg12State {
    cc_format: CcFormat,
    /// A sequence header with a picture size was read: FFmpeg can decode
    /// pictures (`s2->width > 0`, `context_initialized`).
    initialized: bool,
    /// `s->sync`
    sync: bool,
    closed_gop: bool,
    pict_type: u8,
    picture_structure: u8,
    top_field_first: bool,
    progressive_sequence: bool,
    progressive_frame: bool,
    first_field: bool,
    first_slice: bool,
    /// `s->last_pic.ptr` / `s->next_pic.ptr` are set.
    has_last: bool,
    has_next: bool,
}

impl Default for Mpeg12State {
    fn default() -> Self {
        Self {
            cc_format: CcFormat::Auto,
            initialized: false,
            sync: false,
            closed_gop: false,
            pict_type: 0,
            picture_structure: PICT_FRAME,
            top_field_first: false,
            progressive_sequence: false,
            progressive_frame: false,
            first_field: false,
            first_slice: false,
            has_last: false,
            has_next: false,
        }
    }
}

impl CcExtractor {
    /// An extractor for a stream of `codec` (an FFmpeg codec id), or `None`
    /// when that codec carries no A/53 captions. `extradata` is the
    /// stream's: avcC/hvcC set the length size of length-prefixed NAL units;
    /// anything else means Annex B.
    pub fn new(codec: &str, extradata: &[u8]) -> Option<Self> {
        let carrier = CaptionCarrier::from_codec_id(codec)?;
        let nal_length_size = match carrier {
            // h264_ps.c ff_h264_decode_extradata: avcC starts with version 1.
            CaptionCarrier::H264 if extradata.len() >= 7 && extradata[0] == 1 => {
                usize::from(extradata[4] & 3) + 1
            }
            // hevcdec.c hevc_decode_extradata: hvcC unless the extradata
            // starts like an Annex B start code.
            CaptionCarrier::Hevc
                if extradata.len() > 21
                    && (extradata[0] != 0 || extradata[1] != 0 || extradata[2] > 1) =>
            {
                usize::from(extradata[21] & 3) + 1
            }
            _ => 0,
        };
        Some(Self {
            carrier,
            nal_length_size,
            length_prefixed: nal_length_size > 0,
            pending: Vec::new(),
            mpeg: Mpeg12State::default(),
        })
    }

    /// The codec this extractor reads.
    pub fn carrier(&self) -> CaptionCarrier {
        self.carrier
    }

    /// The triplets FFmpeg attaches to the picture(s) `access_unit` starts.
    pub fn extract(&mut self, access_unit: &[u8]) -> Vec<[u8; 3]> {
        let mut out = Vec::new();
        match self.carrier {
            CaptionCarrier::H264 | CaptionCarrier::Hevc => {
                self.extract_h2645(access_unit);
                // The access unit's picture takes everything its SEI held.
                take_pending(&mut self.pending, &mut out);
            }
            CaptionCarrier::Mpeg12 => self.extract_mpeg12(access_unit, &mut out),
        }
        out
    }

    /// Data read but never attached to a picture, at the end of the stream.
    pub fn finish(&mut self) -> Vec<[u8; 3]> {
        let mut out = Vec::new();
        take_pending(&mut self.pending, &mut out);
        out
    }

    /// Forgets the stream state, as a decoder flush on a seek does. The
    /// MPEG-2 caption form, like FFmpeg's option, stays chosen.
    pub fn reset(&mut self) {
        self.pending.clear();
        let cc_format = self.mpeg.cc_format;
        self.mpeg = Mpeg12State { cc_format, ..Mpeg12State::default() };
    }

    // ---- H.264 / HEVC --------------------------------------------------

    fn extract_h2645(&mut self, buf: &[u8]) {
        let hevc = self.carrier == CaptionCarrier::Hevc;
        // h264dec.c decode_nal_units: with 4-byte lengths, a packet that
        // starts with 00 00 00 01 and cannot be read as lengths is Annex B,
        // one whose first length fits is length-prefixed, and any other
        // keeps the last packet's choice. HEVC keeps its extradata's.
        if !hevc && self.nal_length_size == 4 {
            let size = buf.len() as u64;
            let rb32 = |at: usize| buf.get(at..at + 4).map_or(0, |b| u64::from(u32::from_be_bytes([b[0], b[1], b[2], b[3]])));
            if buf.len() > 8 && rb32(0) == 1 && rb32(5) > size {
                self.length_prefixed = false;
            } else if buf.len() > 3 && rb32(0) > 1 && rb32(0) <= size {
                self.length_prefixed = true;
            }
        }
        let nal_length_size = if self.length_prefixed { self.nal_length_size } else { 0 };
        // Only SEI NAL units are read: the others are scanned for their
        // extent, not copied. Escapes never touch the header bytes, so the
        // type can be read before the RBSP is.
        let sei = |nal: &[u8]| nal.first().is_some_and(|&h| if hevc { (h >> 1) & 0x3f == 39 } else { h & 0x1f == 6 });
        let Some(nals) = split_nals(buf, nal_length_size, sei) else {
            // h2645_parse.c: an invalid NAL length fails the whole packet.
            return;
        };
        let header_len = if hevc { 2 } else { 1 };
        for (rbsp, skip_trailing_zeros) in nals {
            let Some(size) = sei_byte_len(&rbsp, header_len, skip_trailing_zeros) else { continue };
            // Forbidden zero bit (h264/hevc_parse_nal_header).
            if rbsp[0] & 0x80 != 0 {
                continue;
            }
            if hevc {
                let nal_type = (rbsp[0] >> 1) & 0x3f;
                let layer_id = ((rbsp[0] & 1) << 5) | (rbsp[1] >> 3);
                let temporal_id_plus1 = rbsp[1] & 7;
                if temporal_id_plus1 == 0 || layer_id == 63 {
                    continue;
                }
                // Only prefix SEI reaches user_data_registered_itu_t_t35;
                // suffix SEI handles the picture hash alone.
                if nal_type == 39 {
                    self.hevc_sei(&rbsp[header_len..header_len + size]);
                }
            } else if rbsp[0] & 0x1f == 6 {
                self.h264_sei(&rbsp[header_len..header_len + size]);
            }
        }
    }

    /// h264_sei.c ff_h264_sei_decode: messages while more than two bytes
    /// remain and the next two are not both zero.
    fn h264_sei(&mut self, sei: &[u8]) {
        let mut pos = 0;
        while sei.len() - pos > 2 && (sei[pos] != 0 || sei[pos + 1] != 0) {
            let mut payload_type = 0usize;
            loop {
                let Some(&byte) = sei.get(pos) else { return };
                payload_type += usize::from(byte);
                pos += 1;
                if byte != 255 {
                    break;
                }
            }
            let mut size = 0usize;
            loop {
                let Some(&byte) = sei.get(pos) else { return };
                size += usize::from(byte);
                pos += 1;
                if byte != 255 {
                    break;
                }
            }
            if size > sei.len() - pos {
                return;
            }
            let payload = &sei[pos..pos + size];
            if payload_type == 4 && registered_user_data(payload, &mut self.pending).is_err() {
                return;
            }
            pos += size;
        }
    }

    /// hevc/sei.c ff_hevc_decode_nal_sei / decode_nal_sei_message.
    fn hevc_sei(&mut self, sei: &[u8]) {
        let mut pos = 0;
        loop {
            let mut payload_type = 0usize;
            let mut byte = 0xffu8;
            while byte == 0xff {
                if sei.len() - pos < 2 || payload_type > i32::MAX as usize - 255 {
                    return;
                }
                byte = sei[pos];
                pos += 1;
                payload_type += usize::from(byte);
            }
            let mut size = 0usize;
            byte = 0xff;
            while byte == 0xff {
                if sei.len() - pos < 1 + size {
                    return;
                }
                byte = sei[pos];
                pos += 1;
                size += usize::from(byte);
            }
            if sei.len() - pos < size {
                return;
            }
            let payload = &sei[pos..pos + size];
            pos += size;
            if payload_type == 4 && registered_user_data(payload, &mut self.pending).is_err() {
                return;
            }
            if pos >= sei.len() {
                return;
            }
        }
    }

    // ---- MPEG-1/2 ------------------------------------------------------

    /// mpeg12dec.c decode_chunks, reduced to what moves caption data.
    fn extract_mpeg12(&mut self, buf: &[u8], out: &mut Vec<[u8; 3]>) {
        const PICTURE: u32 = 0x100;
        const SLICE_MIN: u32 = 0x101;
        const SLICE_MAX: u32 = 0x1af;
        const USER: u32 = 0x1b2;
        const SEQ: u32 = 0x1b3;
        const EXT: u32 = 0x1b5;
        const GOP: u32 = 0x1b8;

        let mut last_code = 0u32;
        let mut picture_start_code_seen = false;
        let mut pos = 0usize;
        while let Some((code, data_start)) = find_start_code(buf, pos) {
            pos = data_start;
            // The data after a start code runs to the end of the packet
            // (input_size = buf_end - buf_ptr).
            let data = &buf[data_start..];
            let m = &mut self.mpeg;
            match code {
                SEQ => {
                    if last_code == 0 {
                        // mpeg1_decode_sequence: 12-bit width, 12-bit height.
                        if data.len() >= 3 {
                            let width = (u32::from(data[0]) << 4) | u32::from(data[1] >> 4);
                            let height = (u32::from(data[1] & 0x0f) << 8) | u32::from(data[2]);
                            if width > 0 && height > 0 {
                                m.initialized = true;
                            }
                        }
                        m.sync = true;
                    }
                }
                GOP => {
                    if last_code == 0 {
                        m.first_field = false;
                        // 25-bit time code, then closed_gop.
                        if data.len() >= 4 {
                            m.closed_gop = data[3] & 0x40 != 0;
                        }
                        m.sync = true;
                    }
                }
                PICTURE => {
                    if picture_start_code_seen && m.picture_structure == PICT_FRAME {
                        // An extra picture after a frame picture is ignored.
                        continue;
                    }
                    picture_start_code_seen = true;
                    if !m.initialized {
                        // "Invalid frame dimensions": the rest of the packet
                        // is not decoded.
                        return;
                    }
                    if last_code == 0 || last_code == SLICE_MIN {
                        // mpeg1_decode_picture: 10-bit temporal reference,
                        // 3-bit picture coding type.
                        m.pict_type = match data.get(1) {
                            Some(&b) => match (b >> 3) & 7 {
                                t @ 1..=3 => t,
                                _ => 0,
                            },
                            None => 0,
                        };
                        m.first_slice = true;
                        last_code = PICTURE;
                    }
                }
                EXT => match data.first().map(|b| b >> 4) {
                    Some(0x1) if last_code == 0 => {
                        // Sequence extension: profile/level (8 bits), then
                        // progressive_sequence.
                        if let Some(&b) = data.get(1) {
                            m.progressive_sequence = b & 0x08 != 0;
                        }
                    }
                    Some(0x8) if last_code == PICTURE => {
                        // Picture coding extension: four 4-bit f_codes, 2-bit
                        // intra_dc_precision, then picture_structure,
                        // top_field_first, ..., progressive_frame.
                        if data.len() >= 5 {
                            let bits = u64::from_be_bytes([0, 0, 0, data[0], data[1], data[2], data[3], data[4]]);
                            // bits 39..36: extension id; f_codes 35..20.
                            if m.pict_type == 0 {
                                let f = |shift: u32| ((bits >> shift) & 0xf) as u8;
                                let (f00, f01, f10, f11) = (f(32), f(28), f(24), f(20));
                                let fix = |v: u8| v + u8::from(v == 0);
                                let (f00, f01, f10, f11) = (fix(f00), fix(f01), fix(f10), fix(f11));
                                if m.initialized {
                                    m.pict_type = if f10 == 15 && f11 == 15 {
                                        if f00 == 15 && f01 == 15 { PICT_TYPE_I } else { PICT_TYPE_P }
                                    } else {
                                        PICT_TYPE_B
                                    };
                                }
                            }
                            m.picture_structure = ((bits >> 16) & 3) as u8;
                            m.top_field_first = (bits >> 15) & 1 != 0;
                            m.progressive_frame = (bits >> 7) & 1 != 0;
                        }
                    }
                    _ => {}
                },
                USER => self.mpeg_user_data(data),
                SLICE_MIN..=SLICE_MAX => {
                    if last_code == PICTURE {
                        if m.progressive_sequence && !m.progressive_frame {
                            m.progressive_frame = true;
                        }
                        if m.picture_structure == 0
                            || (m.progressive_frame && m.picture_structure != PICT_FRAME)
                        {
                            m.picture_structure = PICT_FRAME;
                        }
                        if m.picture_structure == PICT_FRAME {
                            m.first_field = false;
                        } else {
                            m.first_field = !m.first_field;
                        }
                    }
                    if last_code != 0 {
                        last_code = SLICE_MIN;
                        if data.len() < 2 {
                            // "slice too small": the packet stops here.
                            return;
                        }
                        if !m.has_last && m.pict_type == PICT_TYPE_B && !m.closed_gop {
                            // Leading B-pictures of an open GOP are skipped.
                            continue;
                        }
                        if m.pict_type == PICT_TYPE_I {
                            m.sync = true;
                        }
                        if !m.has_next && m.pict_type == PICT_TYPE_P && !m.sync {
                            continue;
                        }
                        if !m.initialized || m.pict_type == 0 {
                            continue;
                        }
                        if m.first_slice {
                            m.first_slice = false;
                            self.field_start(out);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// mpeg_field_start + ff_mpv_frame_start: a frame picture or a first
    /// field starts a frame, which takes the caption data read so far.
    fn field_start(&mut self, out: &mut Vec<[u8; 3]>) {
        let m = &mut self.mpeg;
        if !(m.first_field || m.picture_structure == PICT_FRAME) {
            return;
        }
        if m.pict_type != PICT_TYPE_B {
            m.has_last = m.has_next;
            m.has_next = true;
        }
        // ff_mpv_alloc_dummy_frames
        if !m.has_last && m.pict_type != PICT_TYPE_I {
            m.has_last = true;
        }
        if !m.has_next && m.pict_type == PICT_TYPE_B {
            m.has_next = true;
        }
        take_pending(&mut self.pending, out);
    }

    /// mpeg12dec.c mpeg_decode_user_data and mpeg_decode_a53_cc. `p` runs
    /// from after the user data start code to the end of the packet.
    fn mpeg_user_data(&mut self, p: &[u8]) {
        if p.len() >= 5 && &p[..4] == b"DTG1" {
            return; // active format description
        }
        if p.len() >= 6 && &p[..4] == b"JP3D" && p[4] == 0x03 {
            return; // stereo 3D format
        }
        let format = self.mpeg.cc_format;
        let pending = &mut self.pending;
        if (format == CcFormat::Auto || format == CcFormat::A53Part4)
            && p.len() >= 6
            && &p[..4] == b"GA94"
            && p[4] == 3
            && p[5] & 0x40 != 0
        {
            let cc_count = usize::from(p[5] & 0x1f);
            if cc_count > 0 && p.len() >= 7 + cc_count * 3 {
                if pending.len() + cc_count * 3 > MAX_PENDING_BYTES {
                    return;
                }
                pending.extend_from_slice(&p[7..7 + cc_count * 3]);
                self.set_cc_format(CcFormat::A53Part4);
            }
        } else if (format == CcFormat::Auto || format == CcFormat::Scte20)
            && p.len() >= 2
            && p[0] == 0x03
            && p[1] & 0x7f == 0x01
        {
            let mut gb = BitReader::new(&p[2..]);
            let cc_count = gb.read(5) as usize;
            if cc_count > 0 {
                if pending.len() + cc_count * 3 > MAX_PENDING_BYTES {
                    return;
                }
                let start = pending.len();
                pending.resize(start + cc_count * 3, 0);
                let top_field_first = self.mpeg.top_field_first;
                for i in 0..cc_count {
                    if gb.left() < 26 {
                        break;
                    }
                    gb.skip(2); // priority
                    let mut field = gb.read(2) as u8;
                    gb.skip(5); // line_offset
                    let cc1 = gb.read(8) as u8;
                    let cc2 = gb.read(8) as u8;
                    gb.skip(1); // marker
                    let cap = &mut pending[start + i * 3..start + i * 3 + 3];
                    if field == 0 {
                        // Forbidden: an all-zero triplet.
                        cap.copy_from_slice(&[0, 0, 0]);
                    } else {
                        field = u8::from(field == 2);
                        if !top_field_first {
                            field ^= 1;
                        }
                        cap.copy_from_slice(&[0x04 | field, cc1.reverse_bits(), cc2.reverse_bits()]);
                    }
                }
                self.set_cc_format(CcFormat::Scte20);
            }
        } else if (format == CcFormat::Auto || format == CcFormat::Dvd)
            && p.len() >= 11
            && p[0] == b'C'
            && p[1] == b'C'
            && p[2] == 0x01
            && p[3] == 0xf8
        {
            // The caption count in the data is often wrong: count blocks.
            let mut cc_count = 0usize;
            let mut i = 5;
            while i + 6 <= p.len() && p[i] & 0xfe == 0xfe {
                cc_count += 1;
                i += 6;
            }
            if cc_count > 0 {
                if pending.len() + cc_count * 6 > MAX_PENDING_BYTES {
                    return;
                }
                let field1 = p[4] & 0x80 != 0;
                for block in p[5..5 + cc_count * 6].chunks_exact(6) {
                    pending.extend_from_slice(&[
                        if block[0] == 0xff && field1 { 0xfc } else { 0xfd },
                        block[1],
                        block[2],
                        if block[3] == 0xff && !field1 { 0xfc } else { 0xfd },
                        block[4],
                        block[5],
                    ]);
                }
                self.set_cc_format(CcFormat::Dvd);
            }
        } else if (format == CcFormat::Auto || format == CcFormat::Dish)
            && p.len() >= 12
            && p[0] == 0x05
            && p[1] == 0x02
        {
            const HEADER: u8 = 0xf8 | 0x04; // valid, line 21 field 1
            let mut cc_type = p[7];
            let mut q = &p[8..];
            if cc_type == 0x05 && q.len() >= 7 {
                cc_type = q[6];
                q = &q[7..];
            }
            let mut cc_data = [0u8; 4];
            let mut cc_count = 0usize;
            if cc_type == 0x02 && q.len() >= 4 {
                // A two-byte caption, repeated when the next type is 0x04
                // and the character repeatable (below 32 without parity).
                cc_count = 1;
                cc_data[0] = q[1];
                cc_data[1] = q[2];
                if q[3] == 0x04 && cc_data[0] & 0x7f < 32 {
                    cc_count = 2;
                    cc_data[2] = cc_data[0];
                    cc_data[3] = cc_data[1];
                }
            } else if cc_type == 0x04 && q.len() >= 5 {
                cc_count = 2;
                cc_data.copy_from_slice(&q[1..5]);
            }
            if cc_count > 0 {
                if pending.len() + cc_count * 3 > MAX_PENDING_BYTES {
                    return;
                }
                pending.extend_from_slice(&[HEADER, cc_data[0], cc_data[1]]);
                if cc_count == 2 {
                    pending.extend_from_slice(&[HEADER, cc_data[2], cc_data[3]]);
                }
                self.set_cc_format(CcFormat::Dish);
            }
        }
    }

    fn set_cc_format(&mut self, format: CcFormat) {
        if self.mpeg.cc_format == CcFormat::Auto {
            self.mpeg.cc_format = format;
        }
    }
}

/// Moves whole triplets of `pending` to `out`.
fn take_pending(pending: &mut Vec<u8>, out: &mut Vec<[u8; 3]>) {
    out.extend(pending.chunks_exact(3).map(|t| [t[0], t[1], t[2]]));
    pending.clear();
}

/// h2645_sei.c decode_registered_user_data → itut35.c
/// ff_itut_t35_parse_buffer → atsc_a53.c ff_parse_a53_cc. `Err` stops the
/// rest of the SEI NAL unit, as FFmpeg's error return does.
fn registered_user_data(payload: &[u8], pending: &mut Vec<u8>) -> Result<(), ()> {
    let mut pos = 0usize;
    let left = |pos: usize| payload.len() - pos;
    let Some(&country_code) = payload.first() else { return Err(()) };
    pos += 1;
    if country_code == 0xff {
        if left(pos) < 1 {
            return Err(());
        }
        pos += 1; // country code extension byte
    }
    let be16 = |pos: usize| u16::from_be_bytes([payload[pos], payload[pos + 1]]);
    let be32 = |pos: usize| u32::from_be_bytes([payload[pos], payload[pos + 1], payload[pos + 2], payload[pos + 3]]);
    let mut is_a53 = false;
    match country_code {
        0xb5 => {
            if left(pos) < 2 {
                return Err(());
            }
            let provider_code = be16(pos);
            pos += 2;
            match provider_code {
                0x0031 => {
                    if left(pos) < 4 {
                        return Err(());
                    }
                    let identifier = be32(pos);
                    pos += 4;
                    match &identifier.to_be_bytes() {
                        b"DTG1" => {
                            if left(pos) < 2 {
                                return Err(());
                            }
                            if payload[pos] & 0x40 == 0 {
                                return Ok(());
                            }
                            pos += 1;
                        }
                        b"GA94" => is_a53 = true,
                        _ => return Ok(()),
                    }
                }
                0x5890 => {
                    if left(pos) < 1 {
                        return Err(());
                    }
                    if payload[pos] != 0x01 {
                        return Ok(());
                    }
                    pos += 1;
                }
                0x003c => {
                    if left(pos) < 3 {
                        return Err(());
                    }
                    if be16(pos) != 1 || payload[pos + 2] != 4 {
                        return Ok(());
                    }
                    pos += 3;
                }
                0x003b => {
                    if left(pos) < 4 {
                        return Err(());
                    }
                    if be32(pos) != 0x800 {
                        return Ok(());
                    }
                    pos += 4;
                }
                0x0090 => {
                    if left(pos) < 2 {
                        return Err(());
                    }
                    if be16(pos) != 1 {
                        return Ok(());
                    }
                    pos += 2;
                }
                _ => return Ok(()),
            }
        }
        0xb4 => {
            if left(pos) < 3 {
                return Err(());
            }
            let provider_code = be16(pos + 1);
            pos += 3;
            if provider_code != 0x5000 {
                return Ok(());
            }
        }
        0x26 => {
            if left(pos) < 2 {
                return Err(());
            }
            let provider_code = be16(pos);
            pos += 2;
            if provider_code != 0x0004 {
                return Ok(());
            }
            if left(pos) < 2 {
                return Err(());
            }
            if be16(pos) != 0x0005 {
                return Ok(());
            }
            pos += 2;
        }
        _ => return Ok(()),
    }
    if left(pos) == 0 {
        return Err(());
    }
    if !is_a53 {
        // Film grain, HDR, AFD and the like carry no captions.
        return Ok(());
    }
    parse_a53_cc(&payload[pos..], pending)
}

/// atsc_a53.c ff_parse_a53_cc.
fn parse_a53_cc(data: &[u8], pending: &mut Vec<u8>) -> Result<(), ()> {
    if data.len() < 3 {
        return Err(());
    }
    if data[0] != 0x03 || data[1] & 0x40 == 0 {
        return Ok(());
    }
    let cc_count = usize::from(data[1] & 0x1f);
    if cc_count == 0 {
        return Ok(());
    }
    // Three bytes a caption plus the marker byte at the end.
    if cc_count * 3 >= data.len() - 3 {
        return Err(());
    }
    if pending.len() + cc_count * 3 > MAX_PENDING_BYTES {
        return Ok(());
    }
    pending.extend_from_slice(&data[3..3 + cc_count * 3]);
    Ok(())
}

/// h2645_parse.c ff_h2645_packet_split: the NAL units of a packet that
/// `wanted` picks (by their first bytes), each as its RBSP (emulation
/// prevention removed) and whether its trailing zero bytes count as
/// padding. `nal_length_size` 0 means Annex B. `None` when a length prefix
/// is invalid, which fails the whole packet.
fn split_nals(buf: &[u8], nal_length_size: usize, wanted: impl Fn(&[u8]) -> bool) -> Option<Vec<(Vec<u8>, bool)>> {
    let mut nals = Vec::new();
    let length = buf.len();
    let mut pos = 0usize;
    let mut next_avc = if nal_length_size > 0 { 0 } else { length };
    while length - pos >= 4 {
        let extract_length;
        if pos == next_avc {
            // get_nalsize
            if pos >= length.saturating_sub(nal_length_size) {
                return None;
            }
            let mut size = 0usize;
            for &b in &buf[pos..pos + nal_length_size] {
                size = (size << 8) | usize::from(b);
            }
            pos += nal_length_size;
            if size == 0 || size > length - pos {
                return None;
            }
            extract_length = size;
            next_avc = pos + size;
        } else {
            // find_next_start_code
            let limit = next_avc;
            let skip = if pos + 3 >= limit {
                limit - pos
            } else {
                let mut i = 0usize;
                while pos + i + 3 < limit {
                    if buf[pos + i] == 0 && buf[pos + i + 1] == 0 && buf[pos + i + 2] == 1 {
                        break;
                    }
                    i += 1;
                }
                i + 3
            };
            pos += skip;
            if pos >= length {
                return Some(nals);
            }
            if pos >= next_avc {
                pos = next_avc;
                continue;
            }
            extract_length = (length - pos).min(next_avc - pos);
        }
        let nal = &buf[pos..pos + extract_length];
        let keep = wanted(nal);
        let (rbsp, consumed) = extract_rbsp(nal, keep);
        if keep {
            // As h2645_parse.c checks after the NAL ("see commit
            // 3566042a0"): zeros before a following PES video header
            // (00 00 01 E0) belong to the NAL.
            let skip_trailing_zeros = !buf[pos + consumed..].starts_with(&[0, 0, 1, 0xe0]);
            nals.push((rbsp, skip_trailing_zeros));
        }
        pos += consumed;
    }
    Some(nals)
}

/// h2645_parse.c ff_h2645_extract_rbsp (its byte-wise first pass): the
/// NAL's RBSP, escapes (00 00 03) removed, ending before the next start
/// code, and how many input bytes it took. Without `keep`, only the count:
/// the RBSP comes back empty.
fn extract_rbsp(src: &[u8], keep: bool) -> (Vec<u8>, usize) {
    let mut length = src.len();
    // First pass: the first escape or start code.
    let mut i = 0usize;
    while i + 1 < length {
        if src[i] != 0 {
            i += 2;
            continue;
        }
        if i > 0 && src[i - 1] == 0 {
            i -= 1;
        }
        if i + 2 < length && src[i + 1] == 0 && (src[i + 2] == 3 || src[i + 2] == 1) {
            if src[i + 2] == 1 {
                // A start code: the NAL ends before it.
                length = i;
            }
            break;
        }
        i += 2;
    }
    let i = i.min(length);
    let mut dst = Vec::with_capacity(if keep { length } else { 0 });
    if keep {
        dst.extend_from_slice(&src[..i]);
    }
    let mut si = i;
    while si + 2 < length {
        if src[si + 2] > 3 {
            if keep {
                dst.extend_from_slice(&src[si..si + 2]);
            }
            si += 2;
        } else if src[si] == 0 && src[si + 1] == 0 && src[si + 2] != 0 {
            if src[si + 2] == 3 {
                if keep {
                    dst.extend_from_slice(&[0, 0]);
                }
                si += 3;
                continue;
            }
            // The next start code.
            return (dst, si);
        } else {
            if keep {
                dst.push(src[si]);
            }
            si += 1;
        }
    }
    if keep {
        dst.extend_from_slice(&src[si..length]);
    }
    (dst, length)
}

/// The SEI bytes after the `header_len`-byte NAL header that FFmpeg reads
/// (`get_bits_left(gb) / 8` from h2645_parse.c get_bit_length): trailing
/// zero bytes (unless `skip_trailing_zeros` is false) and the stop bit's
/// byte excluded. `None` when FFmpeg skips the NAL.
fn sei_byte_len(rbsp: &[u8], header_len: usize, skip_trailing_zeros: bool) -> Option<usize> {
    let mut size = rbsp.len();
    while skip_trailing_zeros && size > 0 && rbsp[size - 1] == 0 {
        size -= 1;
    }
    if size == 0 {
        return None;
    }
    let min_size = header_len;
    let size_bits = if size <= min_size {
        if rbsp.len() < min_size {
            return None;
        }
        min_size * 8
    } else {
        let v = rbsp[size - 1];
        let trailing_padding = if v != 0 { v.trailing_zeros() as usize + 1 } else { 0 };
        size * 8 - trailing_padding
    };
    let bits_after_header = size_bits.checked_sub(header_len * 8)?;
    Some(bits_after_header / 8)
}

/// avpriv_find_start_code: the start code byte after the next 00 00 01 at
/// or after `pos`, and the index of the byte after it.
fn find_start_code(buf: &[u8], pos: usize) -> Option<(u32, usize)> {
    let mut i = pos;
    while i + 3 < buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
            return Some((0x100 | u32::from(buf[i + 3]), i + 4));
        }
        i += 1;
    }
    None
}

/// A big-endian bit reader that reads zeros past the end, as FFmpeg's
/// GetBitContext does into its padding.
struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit: 0 }
    }

    fn left(&self) -> isize {
        (self.data.len() * 8) as isize - self.bit as isize
    }

    fn read(&mut self, n: u32) -> u32 {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = self.data.get(self.bit / 8).copied().unwrap_or(0);
            v = (v << 1) | u32::from((byte >> (7 - self.bit % 8)) & 1);
            self.bit += 1;
        }
        v
    }

    fn skip(&mut self, n: usize) {
        self.bit += n;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sei_t35(cc: &[[u8; 3]]) -> Vec<u8> {
        let mut t35 = vec![0xb5, 0x00, 0x31, b'G', b'A', b'9', b'4', 0x03, 0x40 | cc.len() as u8, 0xff];
        for t in cc {
            t35.extend_from_slice(t);
        }
        t35.push(0xff);
        let mut sei = vec![4, t35.len() as u8];
        sei.extend_from_slice(&t35);
        sei
    }

    fn annex_b(nals: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        for nal in nals {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(nal);
        }
        out
    }

    /// NAL units behind 4-byte big-endian lengths (AVCC/HVCC).
    fn length_prefixed(nals: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        for nal in nals {
            out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            out.extend_from_slice(nal);
        }
        out
    }

    #[test]
    fn h264_sei_triplets_in_annex_b_and_avcc() {
        let cc = [[0xfc, 0x94, 0x2c], [0xfd, 0x80, 0x80]];
        let mut sei_nal = vec![0x06];
        sei_nal.extend(sei_t35(&cc));
        sei_nal.push(0x80);
        let slice = vec![0x65, 0x88, 0x84, 0x00, 0x33];
        let nals = [sei_nal, slice];
        assert_eq!(extract_a53("h264", &annex_b(&nals)), cc);
        let mut three_byte_start_codes = Vec::new();
        for nal in &nals {
            three_byte_start_codes.extend_from_slice(&[0, 0, 1]);
            three_byte_start_codes.extend_from_slice(nal);
        }
        assert_eq!(extract_a53("h264", &three_byte_start_codes), cc);
        let avcc = length_prefixed(&nals);
        assert_eq!(extract_a53("h264", &avcc), cc, "AVCC without extradata");
        let avcc_extradata = [1, 0x64, 0, 0x1f, 0xff, 0xe1, 0];
        let mut extractor = CcExtractor::new("h264", &avcc_extradata).unwrap();
        assert_eq!(extractor.extract(&avcc), cc);
    }

    /// A stream with 4-byte lengths: a first NAL unit of 256 to 511 bytes
    /// starts the packet with 00 00 01, and the packet is still read as
    /// lengths (h264dec.c decode_nal_units; HEVC keeps its extradata's
    /// framing). An Annex B packet in an H.264 AVCC stream is read as
    /// Annex B, and the next AVCC packet as lengths again.
    #[test]
    fn length_prefixed_streams_read_packets_as_ffmpeg_does() {
        let cc = [[0xfc, 0x94, 0x2c]];
        let filler = |header: &[u8]| {
            let mut nal = header.to_vec();
            nal.resize(299, 0xff);
            nal.push(0x80);
            nal
        };

        let mut sei_nal = vec![0x06];
        sei_nal.extend(sei_t35(&cc));
        sei_nal.push(0x80);
        let nals = [filler(&[0x0c]), sei_nal, vec![0x65, 0x88, 0x84, 0x00, 0x33]];
        let avcc = length_prefixed(&nals);
        assert_eq!(avcc[..3], [0, 0, 1], "the first length reads like a start code");
        let avcc_extradata = [1, 0x64, 0, 0x1f, 0xff, 0xe1, 0];
        let mut extractor = CcExtractor::new("h264", &avcc_extradata).unwrap();
        assert_eq!(extractor.extract(&avcc), cc);
        assert_eq!(extractor.extract(&annex_b(&nals)), cc);
        assert_eq!(extractor.extract(&avcc), cc);
        assert_eq!(extract_a53("h264", &avcc), cc);

        let mut prefix = vec![39 << 1, 1];
        prefix.extend(sei_t35(&cc));
        prefix.push(0x80);
        let hvcc = length_prefixed(&[filler(&[38 << 1, 1]), prefix]);
        let mut hvcc_extradata = vec![0u8; 23];
        hvcc_extradata[0] = 1;
        hvcc_extradata[21] = 0x0f;
        let mut extractor = CcExtractor::new("hevc", &hvcc_extradata).unwrap();
        assert_eq!(extractor.extract(&hvcc), cc);
        assert_eq!(extract_a53("hevc", &hvcc), cc);
    }

    #[test]
    fn a53_payload_without_its_marker_byte_stops_the_sei() {
        // cc_count 1 needs 3 + 3 + 1 bytes after GA94; give exactly 6.
        let t35 = [0xb5, 0x00, 0x31, b'G', b'A', b'9', b'4', 0x03, 0x41, 0xff, 0xfc, 0x94, 0x2c];
        let mut sei_nal = vec![0x06, 4, t35.len() as u8];
        sei_nal.extend_from_slice(&t35);
        sei_nal.extend(sei_t35(&[[0xfc, 0x80, 0x80]]));
        sei_nal.push(0x80);
        assert!(extract_a53("h264", &annex_b(&[sei_nal])).is_empty());
    }

    #[test]
    fn hevc_reads_prefix_sei_only() {
        let cc = [[0xfc, 0x94, 0x20]];
        let mut prefix = vec![39 << 1, 1];
        prefix.extend(sei_t35(&cc));
        prefix.push(0x80);
        let mut suffix = vec![40 << 1, 1];
        suffix.extend(sei_t35(&[[0xfc, 0x11, 0x22]]));
        suffix.push(0x80);
        let nals = [prefix, suffix];
        assert_eq!(extract_a53("hevc", &annex_b(&nals)), cc);
        assert_eq!(extract_a53("hevc", &length_prefixed(&nals)), cc, "HVCC without extradata");
    }

    #[test]
    fn emulation_prevention_is_removed_before_parsing() {
        // A triplet 00 00 01 would be escaped as 00 00 03 01.
        let cc = [[0xfc, 0x00, 0x00], [0x01, 0x80, 0x80]];
        let mut sei_nal = vec![0x06];
        let raw = sei_t35(&cc);
        let mut zeros = 0;
        for b in raw {
            if zeros >= 2 && b <= 3 {
                sei_nal.push(3);
                zeros = 0;
            }
            sei_nal.push(b);
            zeros = if b == 0 { zeros + 1 } else { 0 };
        }
        sei_nal.push(0x80);
        assert_eq!(extract_a53("h264", &annex_b(&[sei_nal])), cc);
    }

    #[test]
    fn mpeg2_scte20_maps_fields_and_reverses_bits() {
        // Sequence header (352x240), picture header (I), picture coding
        // extension (frame, top_field_first), SCTE-20 user data, a slice.
        let mut au = vec![0, 0, 1, 0xb3, 0x16, 0x00, 0xf0, 0x13, 0xff, 0xff, 0xe0, 0x18];
        au.extend_from_slice(&[0, 0, 1, 0x00, 0x00, 0x0f, 0xff, 0xf8]);
        au.extend_from_slice(&[0, 0, 1, 0xb5, 0x8f, 0xff, 0xf3, 0x80, 0x00]);
        // cc_count 2: (priority 0, field 1, line 0, cc1, cc2, marker) twice.
        let mut bits: Vec<u8> = Vec::new();
        let mut push = |v: u32, n: u32| {
            for i in (0..n).rev() {
                bits.push(((v >> i) & 1) as u8);
            }
        };
        push(2, 5);
        for (field, cc1, cc2) in [(1u32, 0x94u32, 0x2cu32), (2, 0x01, 0x80)] {
            push(0, 2);
            push(field, 2);
            push(0, 5);
            push(cc1, 8);
            push(cc2, 8);
            push(1, 1);
        }
        while bits.len() % 8 != 0 {
            bits.push(0);
        }
        let payload: Vec<u8> = bits.chunks(8).map(|c| c.iter().fold(0, |a, &b| (a << 1) | b)).collect();
        au.extend_from_slice(&[0, 0, 1, 0xb2, 0x03, 0x81]);
        au.extend_from_slice(&payload);
        au.extend_from_slice(&[0, 0, 1, 0x01, 0x0a, 0xbc, 0xde]);
        let got = extract_a53("mpeg2video", &au);
        assert_eq!(got, [[0x04, 0x94u8.reverse_bits(), 0x2cu8.reverse_bits()], [0x05, 0x01u8.reverse_bits(), 0x80u8.reverse_bits()]]);
    }
}
