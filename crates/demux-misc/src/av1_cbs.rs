// Ported from FFmpeg libavcodec/cbs_av1.c and cbs_av1_syntax_template.c
// (the read side of what av1_parser.c decomposes: OBU headers, temporal
// delimiters, sequence headers, frame headers, and the tile group header
// of frame OBUs) and the key-frame rule of av1_parser.c, commit 2da55bf.
// License: LGPL-2.1-or-later
//
// FFmpeg's av1 parser flags a temporal unit key only when ff_cbs_read
// reads all of it: every OBU header and size, and every temporal
// delimiter, sequence header, frame header and frame OBU in full, with
// their range checks and trailing bits. The coded bitstream context's
// state carries over between temporal units (the sequence header, the
// reference frames, the frame size), and so does it here. Only what
// decides whether a read succeeds is kept; values that only feed the
// decoder are read past.

/// A read ff_cbs_read fails on.
#[derive(Debug)]
struct Fail;

type R<T> = std::result::Result<T, Fail>;

const OBU_SEQUENCE_HEADER: u32 = 1;
const OBU_TEMPORAL_DELIMITER: u32 = 2;
const OBU_FRAME_HEADER: u32 = 3;
const OBU_TILE_GROUP: u32 = 4;
const OBU_FRAME: u32 = 6;
const OBU_REDUNDANT_FRAME_HEADER: u32 = 7;
const OBU_TILE_LIST: u32 = 8;

const FRAME_KEY: u32 = 0;
const FRAME_INTER: u32 = 1;
const FRAME_INTRA_ONLY: u32 = 2;
const FRAME_SWITCH: u32 = 3;

const NUM_REF_FRAMES: usize = 8;
const REFS_PER_FRAME: usize = 7;
const PRIMARY_REF_NONE: u32 = 7;
const SELECT_SCREEN_CONTENT_TOOLS: u32 = 2;
const SELECT_INTEGER_MV: u32 = 2;
const SUPERRES_NUM: i64 = 8;
const SUPERRES_DENOM_MIN: i64 = 9;
const MAX_TILE_WIDTH: i64 = 4096;
const MAX_TILE_AREA: i64 = 4096 * 2304;
const MAX_TILE_ROWS: i64 = 64;
const MAX_TILE_COLS: i64 = 64;
const MAX_SEGMENTS: usize = 8;
const SEG_LVL_MAX: usize = 8;
const SEG_LVL_ALT_Q: usize = 0;
const REF_FRAME_LAST: usize = 1;
const REF_FRAME_LAST2: usize = 2;
const REF_FRAME_LAST3: usize = 3;
const REF_FRAME_GOLDEN: usize = 4;
const REF_FRAME_BWDREF: usize = 5;
const REF_FRAME_ALTREF2: usize = 6;
const REF_FRAME_ALTREF: usize = 7;
const WARP_MODEL_TRANSLATION: u32 = 1;
const WARP_MODEL_ROTZOOM: u32 = 2;
const WARP_MODEL_AFFINE: u32 = 3;
const GM_ABS_ALPHA_BITS: u32 = 12;
const GM_ABS_TRANS_ONLY_BITS: u32 = 9;
const GM_ABS_TRANS_BITS: u32 = 12;
const PROFILE_MAIN: u32 = 0;
const PROFILE_HIGH: u32 = 1;
const PROFILE_PROFESSIONAL: u32 = 2;
const PRI_BT709: u32 = 1;
const TRC_IEC61966_2_1: u32 = 13;
const SPC_RGB: u32 = 0;
const CSP_UNKNOWN: u32 = 0;
const CSP_COLOCATED: u32 = 2;

/// av_log2
fn log2(x: u32) -> u32 {
    31 - (x | 1).leading_zeros()
}

/// cbs_av1_tile_log2
fn tile_log2(blksize: i64, target: i64) -> u32 {
    let mut k = 0;
    while (blksize << k) < target {
        k += 1;
    }
    k
}

/// A GetBitContext with CBS's checked reads: running out of bits, or a
/// value outside its range, fails the read.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn left(&self) -> usize {
        self.data.len() * 8 - self.pos
    }

    fn bit(&mut self) -> u32 {
        let b = (self.data[self.pos / 8] >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        u32::from(b)
    }

    /// `n` bits (0..=32) known to be there.
    fn take(&mut self, n: u32) -> u32 {
        let mut v = 0u64;
        for _ in 0..n {
            v = (v << 1) | u64::from(self.bit());
        }
        v as u32
    }

    /// fb / read_simple_unsigned: `n` bits.
    fn f(&mut self, n: u32) -> R<u32> {
        if self.left() < n as usize {
            return Err(Fail);
        }
        Ok(self.take(n))
    }

    /// fc / xf / read_unsigned: `n` bits within `min..=max`.
    fn fc(&mut self, n: u32, min: u32, max: u32) -> R<u32> {
        let v = self.f(n)?;
        if v < min || v > max {
            return Err(Fail);
        }
        Ok(v)
    }

    fn flag(&mut self) -> R<bool> {
        Ok(self.f(1)? != 0)
    }

    /// su / read_signed: `n` bits, two's complement.
    fn su(&mut self, n: u32) -> R<i32> {
        let v = self.f(n)?;
        Ok(((v << (32 - n)) as i32) >> (32 - n))
    }

    /// cbs_av1_read_uvlc
    fn uvlc(&mut self, min: u32, max: u32) -> R<u32> {
        let mut zeroes = 0;
        while zeroes < 32 {
            if self.left() < 1 {
                return Err(Fail);
            }
            if self.bit() == 1 {
                break;
            }
            zeroes += 1;
        }
        if zeroes >= 32 || self.left() < zeroes as usize {
            return Err(Fail);
        }
        let value = self.take(zeroes).wrapping_add((1u32 << zeroes) - 1);
        if value < min || value > max {
            return Err(Fail);
        }
        Ok(value)
    }

    /// cbs_av1_read_leb128
    fn leb128(&mut self) -> R<u64> {
        let mut value = 0u64;
        for i in 0..8 {
            let byte = self.f(8)?;
            value |= u64::from(byte & 0x7f) << (i * 7);
            if byte & 0x80 == 0 {
                break;
            }
        }
        if value > u64::from(u32::MAX) {
            return Err(Fail);
        }
        Ok(value)
    }

    /// cbs_av1_read_ns
    fn ns(&mut self, n: u32) -> R<u32> {
        let w = log2(n) + 1;
        let m = (1u32 << w) - n;
        if self.left() < w as usize {
            return Err(Fail);
        }
        let v = if w > 1 { self.take(w - 1) } else { 0 };
        if v < m {
            return Ok(v);
        }
        Ok((v << 1) - m + self.take(1))
    }

    /// cbs_av1_read_increment
    fn increment(&mut self, min: u32, max: u32) -> R<u32> {
        let mut value = min;
        while value < max {
            if self.left() < 1 {
                return Err(Fail);
            }
            if self.bit() == 1 {
                value += 1;
            } else {
                break;
            }
        }
        Ok(value)
    }

    /// cbs_av1_read_subexp
    fn subexp(&mut self, range_max: u32) -> R<u32> {
        let max_len = log2(range_max - 1) - 3;
        let len = self.increment(0, max_len)?;
        let (range_bits, range_offset) = if len > 0 { (2 + len, 1 << (2 + len)) } else { (3, 0) };
        let value = if len < max_len { self.f(range_bits)? } else { self.ns(range_max - range_offset)? };
        Ok(value + range_offset)
    }

    /// delta_q: a coded flag, then a 7-bit signed value.
    fn delta_q(&mut self) -> R<i32> {
        if self.flag()? { self.su(7) } else { Ok(0) }
    }

    fn byte_alignment(&mut self) -> R<()> {
        while self.pos % 8 != 0 {
            self.fc(1, 0, 0)?;
        }
        Ok(())
    }

    fn trailing_bits(&mut self, nb_bits: i64) -> R<()> {
        self.fc(1, 1, 1)?;
        for _ in 1..nb_bits {
            self.fc(1, 0, 0)?;
        }
        Ok(())
    }
}

/// What later reads use of a sequence header.
#[derive(Clone, Default)]
struct Sequence {
    reduced_still_picture_header: bool,
    equal_picture_interval: bool,
    decoder_model_info_present_flag: bool,
    buffer_removal_time_length_minus_1: u32,
    frame_presentation_time_length_minus_1: u32,
    operating_points_cnt_minus_1: u32,
    operating_point_idc: [u32; 32],
    decoder_model_present_for_this_op: [bool; 32],
    frame_width_bits_minus_1: u32,
    frame_height_bits_minus_1: u32,
    max_frame_width_minus_1: u32,
    max_frame_height_minus_1: u32,
    frame_id_numbers_present_flag: bool,
    delta_frame_id_length_minus_2: u32,
    additional_frame_id_length_minus_1: u32,
    use_128x128_superblock: bool,
    enable_warped_motion: bool,
    enable_order_hint: bool,
    enable_ref_frame_mvs: bool,
    seq_force_screen_content_tools: u32,
    seq_force_integer_mv: u32,
    order_hint_bits_minus_1: u32,
    enable_superres: bool,
    enable_cdef: bool,
    enable_restoration: bool,
    mono_chrome: bool,
    subsampling_x: bool,
    subsampling_y: bool,
    separate_uv_delta_q: bool,
    film_grain_params_present: bool,
}

/// AV1ReferenceFrameState, as far as later reads use it.
#[derive(Clone, Copy, Default)]
struct Reference {
    valid: bool,
    frame_id: i64,
    upscaled_width: i64,
    frame_width: i64,
    frame_height: i64,
    render_width: i64,
    render_height: i64,
    frame_type: u32,
    order_hint: u32,
    feature_enabled: [[bool; SEG_LVL_MAX]; MAX_SEGMENTS],
    feature_value: [[i32; SEG_LVL_MAX]; MAX_SEGMENTS],
}

/// The frame header fields later parts of the header read.
#[derive(Default)]
struct Header {
    show_existing_frame: bool,
    frame_type: u32,
    show_frame: bool,
    showable_frame: bool,
    error_resilient_mode: bool,
    allow_screen_content_tools: bool,
    force_integer_mv: bool,
    current_frame_id: i64,
    frame_size_override_flag: bool,
    order_hint: u32,
    primary_ref_frame: u32,
    refresh_frame_flags: u32,
    allow_intrabc: bool,
    last_frame_idx: u32,
    golden_frame_idx: u32,
    ref_frame_idx: [u32; REFS_PER_FRAME],
    allow_high_precision_mv: bool,
    base_q_idx: u32,
    delta_q_y_dc: i32,
    delta_q_u_dc: i32,
    delta_q_u_ac: i32,
    delta_q_v_dc: i32,
    delta_q_v_ac: i32,
    feature_enabled: [[bool; SEG_LVL_MAX]; MAX_SEGMENTS],
    feature_value: [[i32; SEG_LVL_MAX]; MAX_SEGMENTS],
    delta_q_present: bool,
    reference_select: bool,
}

/// What av1_parser.c looks at in a frame or frame header OBU.
struct Frame {
    spatial_id: u32,
    show_frame: bool,
    show_existing_frame: bool,
    frame_type: u32,
}

/// CodedBitstreamAV1Context: the av1 parser's coded bitstream state, a
/// new one after every seek (ff_read_frame_flush closes the parser).
#[derive(Clone, Default)]
pub(crate) struct Av1Parser {
    sequence: Option<Box<Sequence>>,
    seen_frame_header: bool,
    /// The last frame header's bits, which a redundant one must repeat.
    frame_header: Vec<u8>,
    frame_header_bits: usize,
    temporal_id: u32,
    spatial_id: u32,
    num_planes: u32,
    order_hint: u32,
    upscaled_width: i64,
    frame_width: i64,
    frame_height: i64,
    render_width: i64,
    render_height: i64,
    tile_cols: i64,
    tile_rows: i64,
    tile_num: i64,
    coded_lossless: bool,
    all_lossless: bool,
    feature_enabled: [[bool; SEG_LVL_MAX]; MAX_SEGMENTS],
    feature_value: [[i32; SEG_LVL_MAX]; MAX_SEGMENTS],
    refs: [Reference; NUM_REF_FRAMES],
}

/// One OBU header (cbs_av1 obu_header), the unit's type and its layers.
struct ObuHeader {
    obu_type: u32,
    extension: bool,
    has_size: bool,
}

impl Av1Parser {
    /// av1_parser_parse on one temporal unit: key when ff_cbs_read reads
    /// it all and its last shown frame of spatial layer 0 is a key frame
    /// not shown again (show_existing_frame).
    pub(crate) fn key(&mut self, unit: &[u8]) -> bool {
        let Ok(obus) = self.split(unit) else { return false };
        let mut frames = Vec::new();
        for obu in obus {
            match self.read_unit(obu) {
                Ok(Some(frame)) => frames.push(frame),
                Ok(None) => {}
                Err(Fail) => return false,
            }
        }
        if self.sequence.is_none() {
            return false;
        }
        let mut key = false;
        for f in frames.iter().filter(|f| f.spatial_id == 0 && (f.show_frame || f.show_existing_frame)) {
            key = f.frame_type == FRAME_KEY && !f.show_existing_frame;
        }
        key
    }

    /// obu_header
    fn obu_header(&mut self, r: &mut Bits) -> R<ObuHeader> {
        r.fc(1, 0, 0)?; // obu_forbidden_bit
        let obu_type = r.fc(4, 0, 15)?;
        let extension = r.flag()?;
        let has_size = r.flag()?;
        r.fc(1, 0, 0)?; // obu_reserved_1bit
        let (temporal_id, spatial_id) = if extension {
            let t = r.f(3)?;
            let s = r.f(2)?;
            r.fc(3, 0, 0)?; // extension_header_reserved_3bits
            (t, s)
        } else {
            (0, 0)
        };
        self.temporal_id = temporal_id;
        self.spatial_id = spatial_id;
        Ok(ObuHeader { obu_type, extension, has_size })
    }

    /// cbs_av1_split_fragment: the temporal unit's OBUs.
    fn split<'a>(&mut self, mut data: &'a [u8]) -> R<Vec<&'a [u8]>> {
        if i32::MAX as usize / 8 < data.len() {
            return Err(Fail);
        }
        let mut obus = Vec::new();
        while !data.is_empty() {
            let mut r = Bits::new(data);
            let header = self.obu_header(&mut r)?;
            let obu_size = if header.has_size {
                if r.left() < 8 {
                    return Err(Fail);
                }
                r.leb128()?
            } else {
                (data.len() as u64).wrapping_sub(1 + u64::from(header.extension))
            };
            let length = ((r.pos / 8) as u64).checked_add(obu_size).filter(|&l| l <= data.len() as u64).ok_or(Fail)?;
            let (obu, rest) = data.split_at(length as usize);
            obus.push(obu);
            data = rest;
        }
        Ok(obus)
    }

    /// cbs_av1_read_unit for the types the av1 parser decomposes
    /// (av1_parser.c:189-196): temporal delimiter, sequence header, frame
    /// header, tile group, frame, redundant frame header.
    fn read_unit(&mut self, unit: &[u8]) -> R<Option<Frame>> {
        let mut r = Bits::new(unit);
        let mut peek = Bits::new(unit);
        let obu_type = peek.f(8).map(|b| (b >> 3) & 15).unwrap_or(0);
        let decomposed = [
            OBU_TEMPORAL_DELIMITER,
            OBU_SEQUENCE_HEADER,
            OBU_FRAME_HEADER,
            OBU_TILE_GROUP,
            OBU_FRAME,
            OBU_REDUNDANT_FRAME_HEADER,
        ];
        if !decomposed.contains(&obu_type) {
            return Ok(None);
        }
        let header = self.obu_header(&mut r)?;
        let obu_size = if header.has_size {
            r.leb128()? as i64
        } else {
            if unit.len() < 1 + usize::from(header.extension) {
                return Err(Fail);
            }
            (unit.len() - 1 - usize::from(header.extension)) as i64
        };
        let start = r.pos as i64;
        let spatial_id = self.spatial_id;
        let mut frame = None;
        let shown = |h: Header| Frame {
            spatial_id,
            show_frame: h.show_frame,
            show_existing_frame: h.show_existing_frame,
            frame_type: h.frame_type,
        };
        match header.obu_type {
            OBU_SEQUENCE_HEADER => {
                let sequence = self.sequence_header_obu(&mut r)?;
                self.sequence = Some(Box::new(sequence));
            }
            OBU_TEMPORAL_DELIMITER => self.seen_frame_header = false,
            OBU_FRAME_HEADER => frame = self.frame_header_obu(&mut r, false)?.map(shown),
            OBU_REDUNDANT_FRAME_HEADER => {
                self.frame_header_obu(&mut r, true)?;
            }
            _ => {
                // OBU_FRAME: the frame header, then as OBU_TILE_GROUP the
                // tile group header between tile data that must be there.
                if header.obu_type == OBU_FRAME {
                    frame = self.frame_header_obu(&mut r, false)?.map(shown);
                    r.byte_alignment()?;
                }
                if !self.seen_frame_header || r.pos >= 8 * unit.len() {
                    return Err(Fail);
                }
                self.tile_group_obu(&mut r)?;
                if r.pos >= 8 * unit.len() {
                    return Err(Fail);
                }
            }
        }
        if obu_size > 0 && ![OBU_TILE_GROUP, OBU_TILE_LIST, OBU_FRAME].contains(&header.obu_type) {
            let nb_bits = obu_size * 8 + start - r.pos as i64;
            if nb_bits <= 0 {
                return Err(Fail);
            }
            r.trailing_bits(nb_bits)?;
        }
        Ok(frame)
    }

    /// sequence_header_obu; color_config's bit depth and plane count
    /// stay even where a later field fails.
    fn sequence_header_obu(&mut self, r: &mut Bits) -> R<Sequence> {
        self.seen_frame_header = false;
        let mut s = Sequence::default();
        let seq_profile = r.fc(3, PROFILE_MAIN, PROFILE_PROFESSIONAL)?;
        r.flag()?; // still_picture
        s.reduced_still_picture_header = r.flag()?;
        if s.reduced_still_picture_header {
            r.f(5)?; // seq_level_idx[0]
        } else {
            let mut buffer_delay_length = 0;
            // timing_info_present_flag
            if r.flag()? {
                r.fc(32, 1, u32::MAX)?; // num_units_in_display_tick
                r.fc(32, 1, u32::MAX)?; // time_scale
                s.equal_picture_interval = r.flag()?;
                if s.equal_picture_interval {
                    r.uvlc(0, u32::MAX - 1)?;
                }
                s.decoder_model_info_present_flag = r.flag()?;
                if s.decoder_model_info_present_flag {
                    buffer_delay_length = r.f(5)? + 1;
                    r.fc(32, 1, u32::MAX)?; // num_units_in_decoding_tick
                    s.buffer_removal_time_length_minus_1 = r.f(5)?;
                    s.frame_presentation_time_length_minus_1 = r.f(5)?;
                }
            }
            let initial_display_delay_present_flag = r.flag()?;
            s.operating_points_cnt_minus_1 = r.f(5)?;
            for i in 0..=s.operating_points_cnt_minus_1 as usize {
                s.operating_point_idc[i] = r.f(12)?;
                let seq_level_idx = r.f(5)?;
                if seq_level_idx > 7 {
                    r.flag()?; // seq_tier
                }
                if s.decoder_model_info_present_flag {
                    s.decoder_model_present_for_this_op[i] = r.flag()?;
                    if s.decoder_model_present_for_this_op[i] {
                        r.f(buffer_delay_length)?; // decoder_buffer_delay
                        r.f(buffer_delay_length)?; // encoder_buffer_delay
                        r.flag()?; // low_delay_mode_flag
                    }
                }
                if initial_display_delay_present_flag && r.flag()? {
                    r.f(4)?; // initial_display_delay_minus_1
                }
            }
        }
        s.frame_width_bits_minus_1 = r.f(4)?;
        s.frame_height_bits_minus_1 = r.f(4)?;
        s.max_frame_width_minus_1 = r.f(s.frame_width_bits_minus_1 + 1)?;
        s.max_frame_height_minus_1 = r.f(s.frame_height_bits_minus_1 + 1)?;
        s.frame_id_numbers_present_flag = !s.reduced_still_picture_header && r.flag()?;
        if s.frame_id_numbers_present_flag {
            s.delta_frame_id_length_minus_2 = r.f(4)?;
            s.additional_frame_id_length_minus_1 = r.f(3)?;
        }
        s.use_128x128_superblock = r.flag()?;
        r.flag()?; // enable_filter_intra
        r.flag()?; // enable_intra_edge_filter
        if s.reduced_still_picture_header {
            s.seq_force_screen_content_tools = SELECT_SCREEN_CONTENT_TOOLS;
            s.seq_force_integer_mv = SELECT_INTEGER_MV;
        } else {
            r.flag()?; // enable_interintra_compound
            r.flag()?; // enable_masked_compound
            s.enable_warped_motion = r.flag()?;
            r.flag()?; // enable_dual_filter
            s.enable_order_hint = r.flag()?;
            if s.enable_order_hint {
                r.flag()?; // enable_jnt_comp
                s.enable_ref_frame_mvs = r.flag()?;
            }
            let seq_choose_screen_content_tools = r.flag()?;
            s.seq_force_screen_content_tools =
                if seq_choose_screen_content_tools { SELECT_SCREEN_CONTENT_TOOLS } else { r.f(1)? };
            s.seq_force_integer_mv = if s.seq_force_screen_content_tools > 0 {
                let seq_choose_integer_mv = r.flag()?;
                if seq_choose_integer_mv { SELECT_INTEGER_MV } else { r.f(1)? }
            } else {
                SELECT_INTEGER_MV
            };
            if s.enable_order_hint {
                s.order_hint_bits_minus_1 = r.f(3)?;
            }
        }
        s.enable_superres = r.flag()?;
        s.enable_cdef = r.flag()?;
        s.enable_restoration = r.flag()?;
        self.color_config(r, &mut s, seq_profile)?;
        s.film_grain_params_present = r.flag()?;
        Ok(s)
    }

    /// color_config
    fn color_config(&mut self, r: &mut Bits, s: &mut Sequence, seq_profile: u32) -> R<()> {
        let high_bitdepth = r.flag()?;
        let bit_depth = if seq_profile == PROFILE_PROFESSIONAL && high_bitdepth {
            if r.flag()? { 12 } else { 10 }
        } else if high_bitdepth {
            10
        } else {
            8
        };
        s.mono_chrome = seq_profile != PROFILE_HIGH && r.flag()?;
        self.num_planes = if s.mono_chrome { 1 } else { 3 };
        let (mut primaries, mut transfer, mut matrix) = (2, 2, 2);
        if r.flag()? {
            primaries = r.f(8)?;
            transfer = r.f(8)?;
            matrix = r.f(8)?;
        }
        if s.mono_chrome {
            r.flag()?; // color_range
            (s.subsampling_x, s.subsampling_y) = (true, true);
            s.separate_uv_delta_q = false;
        } else if primaries == PRI_BT709 && transfer == TRC_IEC61966_2_1 && matrix == SPC_RGB {
            (s.subsampling_x, s.subsampling_y) = (false, false);
            s.separate_uv_delta_q = r.flag()?;
        } else {
            r.flag()?; // color_range
            if seq_profile == PROFILE_MAIN {
                (s.subsampling_x, s.subsampling_y) = (true, true);
            } else if seq_profile == PROFILE_HIGH {
                (s.subsampling_x, s.subsampling_y) = (false, false);
            } else if bit_depth == 12 {
                s.subsampling_x = r.flag()?;
                s.subsampling_y = s.subsampling_x && r.flag()?;
            } else {
                (s.subsampling_x, s.subsampling_y) = (true, false);
            }
            if s.subsampling_x && s.subsampling_y {
                r.fc(2, CSP_UNKNOWN, CSP_COLOCATED)?; // chroma_sample_position
            }
            s.separate_uv_delta_q = r.flag()?;
        }
        Ok(())
    }

    /// frame_header_obu: the uncompressed header, or for a redundant one
    /// after a frame header the same bits again. None for a redundant
    /// copy.
    fn frame_header_obu(&mut self, r: &mut Bits, redundant: bool) -> R<Option<Header>> {
        if self.seen_frame_header {
            if !redundant {
                // "Invalid repeated frame header OBU."
                return Err(Fail);
            }
            let copy = std::mem::take(&mut self.frame_header);
            let mut fh = Bits::new(&copy);
            let mut i = 0;
            let repeated = loop {
                if i >= self.frame_header_bits {
                    break Ok(());
                }
                let b = (self.frame_header_bits - i).min(8) as u32;
                let value = fh.take(b);
                if let Err(e) = r.fc(b, value, value) {
                    break Err(e);
                }
                i += 8;
            };
            self.frame_header = copy;
            return repeated.map(|()| None);
        }
        let start = r.pos;
        let h = self.uncompressed_header(r)?;
        self.tile_num = 0;
        if h.show_existing_frame {
            self.seen_frame_header = false;
        } else {
            self.seen_frame_header = true;
            let bits = r.pos - start;
            self.frame_header = r.data[start / 8..(start + bits).div_ceil(8)].to_vec();
            self.frame_header_bits = bits;
        }
        Ok(Some(h))
    }

    /// cbs_av1_get_relative_dist
    fn relative_dist(s: &Sequence, a: u32, b: u32) -> i32 {
        if !s.enable_order_hint {
            return 0;
        }
        let diff = a.wrapping_sub(b);
        let m = 1u32 << s.order_hint_bits_minus_1;
        (diff & (m - 1)) as i32 - (diff & m) as i32
    }

    fn uncompressed_header(&mut self, r: &mut Bits) -> R<Header> {
        let Some(s) = self.sequence.clone() else {
            // "No sequence header available: unable to decode frame header."
            return Err(Fail);
        };
        let mut h = Header::default();
        let id_len = s.additional_frame_id_length_minus_1 + s.delta_frame_id_length_minus_2 + 3;
        let all_frames = (1u32 << NUM_REF_FRAMES) - 1;
        let frame_is_intra;
        if s.reduced_still_picture_header {
            h.frame_type = FRAME_KEY;
            h.show_frame = true;
            frame_is_intra = true;
        } else {
            h.show_existing_frame = r.flag()?;
            if h.show_existing_frame {
                let frame_to_show_map_idx = r.f(3)? as usize;
                let rf = self.refs[frame_to_show_map_idx];
                if !rf.valid {
                    // "Missing reference frame needed for show_existing_frame"
                    return Err(Fail);
                }
                if s.decoder_model_info_present_flag && !s.equal_picture_interval {
                    r.f(s.frame_presentation_time_length_minus_1 + 1)?;
                }
                if s.frame_id_numbers_present_flag {
                    r.f(id_len)?; // display_frame_id
                }
                h.frame_type = rf.frame_type;
                if h.frame_type == FRAME_KEY {
                    h.refresh_frame_flags = all_frames;
                    // Section 7.21
                    h.current_frame_id = rf.frame_id;
                    self.upscaled_width = rf.upscaled_width;
                    self.frame_width = rf.frame_width;
                    self.frame_height = rf.frame_height;
                    self.render_width = rf.render_width;
                    self.render_height = rf.render_height;
                    self.order_hint = rf.order_hint;
                    self.feature_enabled = rf.feature_enabled;
                    self.feature_value = rf.feature_value;
                }
                // Section 7.20
                self.update_refs(&h);
                return Ok(h);
            }
            h.frame_type = r.f(2)?;
            frame_is_intra = h.frame_type == FRAME_INTRA_ONLY || h.frame_type == FRAME_KEY;
            h.show_frame = r.flag()?;
            if h.show_frame && s.decoder_model_info_present_flag && !s.equal_picture_interval {
                r.f(s.frame_presentation_time_length_minus_1 + 1)?;
            }
            h.showable_frame = if h.show_frame { h.frame_type != FRAME_KEY } else { r.flag()? };
            h.error_resilient_mode = h.frame_type == FRAME_SWITCH || (h.frame_type == FRAME_KEY && h.show_frame) || r.flag()?;
        }
        if h.frame_type == FRAME_KEY && h.show_frame {
            for rf in &mut self.refs {
                rf.valid = false;
                rf.order_hint = 0;
            }
        }
        let disable_cdf_update = r.flag()?;
        h.allow_screen_content_tools = if s.seq_force_screen_content_tools == SELECT_SCREEN_CONTENT_TOOLS {
            r.flag()?
        } else {
            s.seq_force_screen_content_tools != 0
        };
        if h.allow_screen_content_tools {
            h.force_integer_mv = if s.seq_force_integer_mv == SELECT_INTEGER_MV { r.flag()? } else { s.seq_force_integer_mv != 0 };
        }
        if s.frame_id_numbers_present_flag {
            h.current_frame_id = i64::from(r.f(id_len)?);
            let diff_len = s.delta_frame_id_length_minus_2 + 2;
            let (cur, d) = (h.current_frame_id, 1i64 << diff_len);
            for rf in &mut self.refs {
                if cur > d {
                    if rf.frame_id > cur || rf.frame_id < cur - d {
                        rf.valid = false;
                    }
                } else if rf.frame_id > cur && rf.frame_id < (1i64 << id_len) + cur - d {
                    rf.valid = false;
                }
            }
        }
        h.frame_size_override_flag = if h.frame_type == FRAME_SWITCH {
            true
        } else {
            !s.reduced_still_picture_header && r.flag()?
        };
        let order_hint_bits = if s.enable_order_hint { s.order_hint_bits_minus_1 + 1 } else { 0 };
        h.order_hint = if order_hint_bits > 0 { r.f(order_hint_bits)? } else { 0 };
        self.order_hint = h.order_hint;
        h.primary_ref_frame = if frame_is_intra || h.error_resilient_mode { PRIMARY_REF_NONE } else { r.f(3)? };
        if s.decoder_model_info_present_flag && r.flag()? {
            for i in 0..=s.operating_points_cnt_minus_1 as usize {
                if s.decoder_model_present_for_this_op[i] {
                    let idc = s.operating_point_idc[i];
                    let in_temporal_layer = (idc >> self.temporal_id) & 1 != 0;
                    let in_spatial_layer = (idc >> (self.spatial_id + 8)) & 1 != 0;
                    if idc == 0 || (in_temporal_layer && in_spatial_layer) {
                        r.f(s.buffer_removal_time_length_minus_1 + 1)?;
                    }
                }
            }
        }
        h.refresh_frame_flags =
            if h.frame_type == FRAME_SWITCH || (h.frame_type == FRAME_KEY && h.show_frame) { all_frames } else { r.f(8)? };
        if (!frame_is_intra || h.refresh_frame_flags != all_frames) && s.enable_order_hint {
            for i in 0..NUM_REF_FRAMES {
                let ref_order_hint = if h.error_resilient_mode { r.f(order_hint_bits)? } else { self.refs[i].order_hint };
                if ref_order_hint != self.refs[i].order_hint {
                    self.refs[i].valid = false;
                }
            }
        }
        if frame_is_intra {
            self.frame_size(r, &s, &h)?;
            self.render_size(r)?;
            h.allow_intrabc = h.allow_screen_content_tools && self.upscaled_width == self.frame_width && r.flag()?;
        } else {
            let frame_refs_short_signaling = s.enable_order_hint && r.flag()?;
            if frame_refs_short_signaling {
                h.last_frame_idx = r.f(3)?;
                h.golden_frame_idx = r.f(3)?;
                self.set_frame_refs(&s, &mut h);
            }
            for i in 0..REFS_PER_FRAME {
                if !frame_refs_short_signaling {
                    h.ref_frame_idx[i] = r.f(3)?;
                }
                if s.frame_id_numbers_present_flag {
                    r.f(s.delta_frame_id_length_minus_2 + 2)?; // delta_frame_id_minus1
                }
            }
            if h.frame_size_override_flag && !h.error_resilient_mode {
                self.frame_size_with_refs(r, &s, &h)?;
            } else {
                self.frame_size(r, &s, &h)?;
                self.render_size(r)?;
            }
            h.allow_high_precision_mv = !h.force_integer_mv && r.flag()?;
            // interpolation_filter
            if !r.flag()? {
                r.f(2)?;
            }
            r.flag()?; // is_motion_mode_switchable
            if !h.error_resilient_mode && s.enable_ref_frame_mvs {
                r.flag()?; // use_ref_frame_mvs
            }
        }
        if !s.reduced_still_picture_header && !disable_cdf_update {
            r.flag()?; // disable_frame_end_update_cdf
        }
        self.tile_info(r, &s)?;
        self.quantization_params(r, &s, &mut h)?;
        self.segmentation_params(r, &mut h)?;
        // delta_q_params
        h.delta_q_present = h.base_q_idx > 0 && r.flag()?;
        if h.delta_q_present {
            r.f(2)?; // delta_q_res
        }
        // delta_lf_params
        if h.delta_q_present && !h.allow_intrabc && r.flag()? {
            r.f(2)?; // delta_lf_res
            r.flag()?; // delta_lf_multi
        }
        self.coded_lossless = (0..MAX_SEGMENTS).all(|i| {
            let mut qindex = h.base_q_idx as i32;
            if h.feature_enabled[i][SEG_LVL_ALT_Q] {
                qindex += h.feature_value[i][SEG_LVL_ALT_Q];
            }
            let qindex = qindex.clamp(0, 255);
            qindex == 0
                && h.delta_q_y_dc == 0
                && h.delta_q_u_ac == 0
                && h.delta_q_u_dc == 0
                && h.delta_q_v_ac == 0
                && h.delta_q_v_dc == 0
        });
        self.all_lossless = self.coded_lossless && self.frame_width == self.upscaled_width;
        self.loop_filter_params(r, &h)?;
        self.cdef_params(r, &s, &h)?;
        self.lr_params(r, &s, &h)?;
        // read_tx_mode
        if !self.coded_lossless {
            r.increment(1, 2)?;
        }
        // frame_reference_mode
        if !frame_is_intra {
            h.reference_select = r.flag()?;
        }
        self.skip_mode_params(r, &s, &h)?;
        if !frame_is_intra && !h.error_resilient_mode && s.enable_warped_motion {
            r.flag()?; // allow_warped_motion
        }
        r.flag()?; // reduced_tx_set
        self.global_motion_params(r, &h)?;
        self.film_grain_params(r, &s, &h)?;
        self.update_refs(&h);
        Ok(h)
    }

    /// update_refs: the reference slots this frame refreshes take its state.
    fn update_refs(&mut self, h: &Header) {
        for i in 0..NUM_REF_FRAMES {
            if h.refresh_frame_flags & (1 << i) != 0 {
                let (feature_enabled, feature_value) = if h.show_existing_frame {
                    (self.feature_enabled, self.feature_value)
                } else {
                    (h.feature_enabled, h.feature_value)
                };
                self.refs[i] = Reference {
                    valid: true,
                    frame_id: h.current_frame_id,
                    upscaled_width: self.upscaled_width,
                    frame_width: self.frame_width,
                    frame_height: self.frame_height,
                    render_width: self.render_width,
                    render_height: self.render_height,
                    frame_type: h.frame_type,
                    order_hint: self.order_hint,
                    feature_enabled,
                    feature_value,
                };
            }
        }
    }

    /// set_frame_refs (7.8)
    fn set_frame_refs(&self, s: &Sequence, h: &mut Header) {
        let mut ref_frame_idx = [-1i32; REFS_PER_FRAME];
        ref_frame_idx[0] = h.last_frame_idx as i32;
        ref_frame_idx[REF_FRAME_GOLDEN - REF_FRAME_LAST] = h.golden_frame_idx as i32;
        let mut used_frame = [false; NUM_REF_FRAMES];
        used_frame[h.last_frame_idx as usize] = true;
        used_frame[h.golden_frame_idx as usize] = true;
        let cur_frame_hint = 1i32 << s.order_hint_bits_minus_1;
        let mut shifted_order_hints = [0i32; NUM_REF_FRAMES];
        for (i, hint) in shifted_order_hints.iter_mut().enumerate() {
            *hint = cur_frame_hint + Self::relative_dist(s, self.refs[i].order_hint, self.order_hint);
        }
        let mut latest_order_hint = shifted_order_hints[h.last_frame_idx as usize];
        let mut earliest_order_hint = shifted_order_hints[h.golden_frame_idx as usize];

        let pick = |used: &mut [bool; NUM_REF_FRAMES], better: &mut dyn FnMut(i32, i32) -> bool, bound: &mut i32, forward: bool| {
            let mut found = -1i32;
            for i in 0..NUM_REF_FRAMES {
                let hint = shifted_order_hints[i];
                let side = if forward { hint >= cur_frame_hint } else { hint < cur_frame_hint };
                if !used[i] && side && (found < 0 || better(hint, *bound)) {
                    found = i as i32;
                    *bound = hint;
                }
            }
            if found >= 0 {
                used[found as usize] = true;
            }
            found
        };
        let altref = pick(&mut used_frame, &mut |hint, b| hint >= b, &mut latest_order_hint, true);
        if altref >= 0 {
            ref_frame_idx[REF_FRAME_ALTREF - REF_FRAME_LAST] = altref;
        }
        let bwdref = pick(&mut used_frame, &mut |hint, b| hint < b, &mut earliest_order_hint, true);
        if bwdref >= 0 {
            ref_frame_idx[REF_FRAME_BWDREF - REF_FRAME_LAST] = bwdref;
        }
        let altref2 = pick(&mut used_frame, &mut |hint, b| hint < b, &mut earliest_order_hint, true);
        if altref2 >= 0 {
            ref_frame_idx[REF_FRAME_ALTREF2 - REF_FRAME_LAST] = altref2;
        }
        for ref_frame in [REF_FRAME_LAST2, REF_FRAME_LAST3, REF_FRAME_BWDREF, REF_FRAME_ALTREF2, REF_FRAME_ALTREF] {
            if ref_frame_idx[ref_frame - REF_FRAME_LAST] < 0 {
                let found = pick(&mut used_frame, &mut |hint, b| hint >= b, &mut latest_order_hint, false);
                if found >= 0 {
                    ref_frame_idx[ref_frame - REF_FRAME_LAST] = found;
                }
            }
        }
        let mut earliest = -1i32;
        for i in 0..NUM_REF_FRAMES {
            let hint = shifted_order_hints[i];
            if earliest < 0 || hint < earliest_order_hint {
                earliest = i as i32;
                earliest_order_hint = hint;
            }
        }
        for (i, idx) in ref_frame_idx.iter().enumerate() {
            h.ref_frame_idx[i] = if *idx < 0 { earliest as u32 } else { *idx as u32 };
        }
    }

    /// superres_params
    fn superres_params(&mut self, r: &mut Bits, s: &Sequence) -> R<()> {
        let use_superres = s.enable_superres && r.flag()?;
        let denom = if use_superres { i64::from(r.f(3)?) + SUPERRES_DENOM_MIN } else { SUPERRES_NUM };
        self.upscaled_width = self.frame_width;
        self.frame_width = (self.upscaled_width * SUPERRES_NUM + denom / 2) / denom;
        Ok(())
    }

    /// frame_size
    fn frame_size(&mut self, r: &mut Bits, s: &Sequence, h: &Header) -> R<()> {
        let (w, hgt) = if h.frame_size_override_flag {
            (r.f(s.frame_width_bits_minus_1 + 1)?, r.f(s.frame_height_bits_minus_1 + 1)?)
        } else {
            (s.max_frame_width_minus_1, s.max_frame_height_minus_1)
        };
        self.frame_width = i64::from(w) + 1;
        self.frame_height = i64::from(hgt) + 1;
        self.superres_params(r, s)
    }

    /// render_size
    fn render_size(&mut self, r: &mut Bits) -> R<()> {
        if r.flag()? {
            self.render_width = i64::from(r.f(16)?) + 1;
            self.render_height = i64::from(r.f(16)?) + 1;
        } else {
            // the frame size before superres
            self.render_width = self.upscaled_width;
            self.render_height = self.frame_height;
        }
        Ok(())
    }

    /// frame_size_with_refs
    fn frame_size_with_refs(&mut self, r: &mut Bits, s: &Sequence, h: &Header) -> R<()> {
        for i in 0..REFS_PER_FRAME {
            if r.flag()? {
                let rf = self.refs[h.ref_frame_idx[i] as usize];
                if !rf.valid {
                    // "Missing reference frame needed for frame size"
                    return Err(Fail);
                }
                self.upscaled_width = rf.upscaled_width;
                self.frame_width = self.upscaled_width;
                self.frame_height = rf.frame_height;
                self.render_width = rf.render_width;
                self.render_height = rf.render_height;
                return self.superres_params(r, s);
            }
        }
        self.frame_size(r, s, h)?;
        self.render_size(r)
    }

    /// tile_info
    fn tile_info(&mut self, r: &mut Bits, s: &Sequence) -> R<()> {
        let mi_cols = 2 * ((self.frame_width + 7) >> 3);
        let mi_rows = 2 * ((self.frame_height + 7) >> 3);
        let (sb_cols, sb_rows, sb_shift) = if s.use_128x128_superblock {
            ((mi_cols + 31) >> 5, (mi_rows + 31) >> 5, 5)
        } else {
            ((mi_cols + 15) >> 4, (mi_rows + 15) >> 4, 4)
        };
        let sb_size = sb_shift + 2;
        let max_tile_width_sb = MAX_TILE_WIDTH >> sb_size;
        let mut max_tile_area_sb = MAX_TILE_AREA >> (2 * sb_size);
        let min_log2_tile_cols = tile_log2(max_tile_width_sb, sb_cols);
        let max_log2_tile_cols = tile_log2(1, sb_cols.min(MAX_TILE_COLS));
        let max_log2_tile_rows = tile_log2(1, sb_rows.min(MAX_TILE_ROWS));
        let min_log2_tiles = min_log2_tile_cols.max(tile_log2(max_tile_area_sb, sb_rows * sb_cols));
        let (tile_cols_log2, tile_rows_log2, tile_cols, tile_rows);
        if r.flag()? {
            // uniform_tile_spacing_flag
            tile_cols_log2 = r.increment(min_log2_tile_cols, max_log2_tile_cols)?;
            let tile_width_sb = (sb_cols + (1 << tile_cols_log2) - 1) >> tile_cols_log2;
            tile_cols = (sb_cols + tile_width_sb - 1) / tile_width_sb;
            let min_log2_tile_rows = min_log2_tiles.saturating_sub(tile_cols_log2);
            tile_rows_log2 = r.increment(min_log2_tile_rows, max_log2_tile_rows)?;
            let tile_height_sb = (sb_rows + (1 << tile_rows_log2) - 1) >> tile_rows_log2;
            tile_rows = (sb_rows + tile_height_sb - 1) / tile_height_sb;
        } else {
            let (mut widest_tile_sb, mut start_sb, mut i) = (0, 0, 0);
            while start_sb < sb_cols && i < MAX_TILE_COLS {
                let max_width = (sb_cols - start_sb).min(max_tile_width_sb);
                let size_sb = i64::from(r.ns(max_width as u32)?) + 1;
                widest_tile_sb = widest_tile_sb.max(size_sb);
                start_sb += size_sb;
                i += 1;
            }
            tile_cols_log2 = tile_log2(1, i);
            tile_cols = i;
            max_tile_area_sb = if min_log2_tiles > 0 {
                (sb_rows * sb_cols) >> (min_log2_tiles + 1)
            } else {
                sb_rows * sb_cols
            };
            let max_tile_height_sb = (max_tile_area_sb / widest_tile_sb).max(1);
            let (mut start_sb, mut i) = (0, 0);
            while start_sb < sb_rows && i < MAX_TILE_ROWS {
                let max_height = (sb_rows - start_sb).min(max_tile_height_sb);
                start_sb += i64::from(r.ns(max_height as u32)?) + 1;
                i += 1;
            }
            tile_rows_log2 = tile_log2(1, i);
            tile_rows = i;
        }
        if tile_cols_log2 > 0 || tile_rows_log2 > 0 {
            r.f(tile_cols_log2 + tile_rows_log2)?; // context_update_tile_id
            r.f(2)?; // tile_size_bytes_minus1
        }
        self.tile_cols = tile_cols;
        self.tile_rows = tile_rows;
        Ok(())
    }

    /// quantization_params
    fn quantization_params(&mut self, r: &mut Bits, s: &Sequence, h: &mut Header) -> R<()> {
        h.base_q_idx = r.f(8)?;
        h.delta_q_y_dc = r.delta_q()?;
        if self.num_planes > 1 {
            let diff_uv_delta = s.separate_uv_delta_q && r.flag()?;
            h.delta_q_u_dc = r.delta_q()?;
            h.delta_q_u_ac = r.delta_q()?;
            if diff_uv_delta {
                h.delta_q_v_dc = r.delta_q()?;
                h.delta_q_v_ac = r.delta_q()?;
            } else {
                (h.delta_q_v_dc, h.delta_q_v_ac) = (h.delta_q_u_dc, h.delta_q_u_ac);
            }
        }
        if r.flag()? {
            // using_qmatrix: qm_y, qm_u, qm_v
            r.f(4)?;
            r.f(4)?;
            if s.separate_uv_delta_q {
                r.f(4)?;
            }
        }
        Ok(())
    }

    /// segmentation_params
    fn segmentation_params(&mut self, r: &mut Bits, h: &mut Header) -> R<()> {
        const BITS: [u32; SEG_LVL_MAX] = [8, 6, 6, 6, 6, 3, 0, 0];
        const SIGNED: [bool; SEG_LVL_MAX] = [true, true, true, true, true, false, false, false];
        if !r.flag()? {
            // segmentation_enabled: every feature off
            return Ok(());
        }
        let update_data = if h.primary_ref_frame == PRIMARY_REF_NONE {
            true
        } else {
            if r.flag()? {
                r.flag()?; // segmentation_temporal_update
            }
            r.flag()?
        };
        let reference = (h.primary_ref_frame != PRIMARY_REF_NONE)
            .then(|| self.refs[h.ref_frame_idx[h.primary_ref_frame as usize] as usize]);
        for i in 0..MAX_SEGMENTS {
            for j in 0..SEG_LVL_MAX {
                if update_data {
                    h.feature_enabled[i][j] = r.flag()?;
                    h.feature_value[i][j] = if h.feature_enabled[i][j] && BITS[j] > 0 {
                        if SIGNED[j] { r.su(1 + BITS[j])? } else { r.f(BITS[j])? as i32 }
                    } else {
                        0
                    };
                } else if let Some(rf) = reference {
                    h.feature_enabled[i][j] = rf.feature_enabled[i][j];
                    h.feature_value[i][j] = rf.feature_value[i][j];
                }
            }
        }
        Ok(())
    }

    /// loop_filter_params, the values themselves read past.
    fn loop_filter_params(&mut self, r: &mut Bits, h: &Header) -> R<()> {
        if self.coded_lossless || h.allow_intrabc {
            return Ok(());
        }
        let level0 = r.f(6)?;
        let level1 = r.f(6)?;
        if self.num_planes > 1 && (level0 != 0 || level1 != 0) {
            r.f(6)?;
            r.f(6)?;
        }
        r.f(3)?; // loop_filter_sharpness
        if r.flag()? {
            // loop_filter_delta_enabled
            let delta_update = r.flag()?;
            for _ in 0..NUM_REF_FRAMES + 2 {
                if delta_update && r.flag()? {
                    r.su(7)?;
                }
            }
        }
        Ok(())
    }

    /// cdef_params
    fn cdef_params(&mut self, r: &mut Bits, s: &Sequence, h: &Header) -> R<()> {
        if self.coded_lossless || h.allow_intrabc || !s.enable_cdef {
            return Ok(());
        }
        r.f(2)?; // cdef_damping_minus_3
        let cdef_bits = r.f(2)?;
        for _ in 0..1 << cdef_bits {
            r.f(4)?;
            r.f(2)?;
            if self.num_planes > 1 {
                r.f(4)?;
                r.f(2)?;
            }
        }
        Ok(())
    }

    /// lr_params
    fn lr_params(&mut self, r: &mut Bits, s: &Sequence, h: &Header) -> R<()> {
        if self.all_lossless || h.allow_intrabc || !s.enable_restoration {
            return Ok(());
        }
        let (mut uses_lr, mut uses_chroma_lr) = (false, false);
        for i in 0..self.num_planes {
            if r.f(2)? != 0 {
                uses_lr = true;
                uses_chroma_lr |= i > 0;
            }
        }
        if uses_lr {
            if s.use_128x128_superblock {
                r.increment(1, 2)?;
            } else {
                r.increment(0, 2)?;
            }
            if s.subsampling_x && s.subsampling_y && uses_chroma_lr {
                r.f(1)?; // lr_uv_shift
            }
        }
        Ok(())
    }

    /// skip_mode_params
    fn skip_mode_params(&mut self, r: &mut Bits, s: &Sequence, h: &Header) -> R<()> {
        let skip_mode_allowed = if h.frame_type == FRAME_KEY
            || h.frame_type == FRAME_INTRA_ONLY
            || !h.reference_select
            || !s.enable_order_hint
        {
            false
        } else {
            let (mut forward_idx, mut backward_idx) = (-1i32, -1i32);
            let (mut forward_hint, mut backward_hint) = (0u32, 0u32);
            for i in 0..REFS_PER_FRAME {
                let ref_hint = self.refs[h.ref_frame_idx[i] as usize].order_hint;
                let dist = Self::relative_dist(s, ref_hint, self.order_hint);
                if dist < 0 {
                    if forward_idx < 0 || Self::relative_dist(s, ref_hint, forward_hint) > 0 {
                        forward_idx = i as i32;
                        forward_hint = ref_hint;
                    }
                } else if dist > 0 && (backward_idx < 0 || Self::relative_dist(s, ref_hint, backward_hint) < 0) {
                    backward_idx = i as i32;
                    backward_hint = ref_hint;
                }
            }
            if forward_idx < 0 {
                false
            } else if backward_idx >= 0 {
                true
            } else {
                let mut second_forward: Option<u32> = None;
                for i in 0..REFS_PER_FRAME {
                    let ref_hint = self.refs[h.ref_frame_idx[i] as usize].order_hint;
                    if Self::relative_dist(s, ref_hint, forward_hint) < 0
                        && second_forward.is_none_or(|second| Self::relative_dist(s, ref_hint, second) > 0)
                    {
                        second_forward = Some(ref_hint);
                    }
                }
                second_forward.is_some()
            }
        };
        if skip_mode_allowed {
            r.flag()?; // skip_mode_present
        }
        Ok(())
    }

    /// global_motion_params
    fn global_motion_params(&mut self, r: &mut Bits, h: &Header) -> R<()> {
        if h.frame_type == FRAME_KEY || h.frame_type == FRAME_INTRA_ONLY {
            return Ok(());
        }
        let param = |r: &mut Bits, kind: u32, idx: u32| -> R<()> {
            let abs_bits = if idx < 2 {
                if kind == WARP_MODEL_TRANSLATION {
                    GM_ABS_TRANS_ONLY_BITS - u32::from(!h.allow_high_precision_mv)
                } else {
                    GM_ABS_TRANS_BITS
                }
            } else {
                GM_ABS_ALPHA_BITS
            };
            r.subexp(2 * (1 << abs_bits) + 1)?;
            Ok(())
        };
        for _ in REF_FRAME_LAST..=REF_FRAME_ALTREF {
            let kind = if r.flag()? {
                if r.flag()? {
                    WARP_MODEL_ROTZOOM
                } else if r.flag()? {
                    WARP_MODEL_TRANSLATION
                } else {
                    WARP_MODEL_AFFINE
                }
            } else {
                0
            };
            if kind >= WARP_MODEL_ROTZOOM {
                param(r, kind, 2)?;
                param(r, kind, 3)?;
                if kind == WARP_MODEL_AFFINE {
                    param(r, kind, 4)?;
                    param(r, kind, 5)?;
                }
            }
            if kind >= WARP_MODEL_TRANSLATION {
                param(r, kind, 0)?;
                param(r, kind, 1)?;
            }
        }
        Ok(())
    }

    /// film_grain_params
    fn film_grain_params(&mut self, r: &mut Bits, s: &Sequence, h: &Header) -> R<()> {
        if !s.film_grain_params_present || (!h.show_frame && !h.showable_frame) {
            return Ok(());
        }
        if !r.flag()? {
            // apply_grain
            return Ok(());
        }
        r.f(16)?; // grain_seed
        let update_grain = h.frame_type != FRAME_INTER || r.flag()?;
        if !update_grain {
            r.f(3)?; // film_grain_params_ref_idx
            return Ok(());
        }
        let points = |r: &mut Bits, max: u32| -> R<u32> {
            let n = r.fc(4, 0, max)?;
            let mut prev: Option<u32> = None;
            for i in 0..n {
                let value = r.fc(8, prev.map_or(0, |p| p + 1), 255 - (n - i - 1))?;
                prev = Some(value);
                r.f(8)?; // scaling
            }
            Ok(n)
        };
        let num_y_points = points(r, 14)?;
        let chroma_scaling_from_luma = !s.mono_chrome && r.flag()?;
        let (num_cb_points, num_cr_points) = if s.mono_chrome
            || chroma_scaling_from_luma
            || (s.subsampling_x && s.subsampling_y && num_y_points == 0)
        {
            (0, 0)
        } else {
            let cb = points(r, 10)?;
            (cb, points(r, 10)?)
        };
        r.f(2)?; // grain_scaling_minus_8
        let ar_coeff_lag = r.f(2)?;
        let num_pos_luma = 2 * ar_coeff_lag * (ar_coeff_lag + 1);
        let num_pos_chroma = if num_y_points > 0 {
            for _ in 0..num_pos_luma {
                r.f(8)?;
            }
            num_pos_luma + 1
        } else {
            num_pos_luma
        };
        for present in [chroma_scaling_from_luma || num_cb_points > 0, chroma_scaling_from_luma || num_cr_points > 0] {
            if present {
                for _ in 0..num_pos_chroma {
                    r.f(8)?;
                }
            }
        }
        r.f(2)?; // ar_coeff_shift_minus_6
        r.f(2)?; // grain_scale_shift
        for present in [num_cb_points > 0, num_cr_points > 0] {
            if present {
                r.f(8)?;
                r.f(8)?;
                r.f(9)?;
            }
        }
        r.flag()?; // overlap_flag
        r.flag()?; // clip_to_restricted_range
        Ok(())
    }

    /// tile_group_obu: the header before the tile data.
    fn tile_group_obu(&mut self, r: &mut Bits) -> R<()> {
        let num_tiles = self.tile_cols * self.tile_rows;
        let tile_start_and_end_present_flag = num_tiles > 1 && r.flag()?;
        let tg_end = if num_tiles == 1 || !tile_start_and_end_present_flag {
            num_tiles - 1
        } else {
            let tile_bits = tile_log2(1, self.tile_cols) + tile_log2(1, self.tile_rows);
            let max = u32::try_from(num_tiles - 1).map_err(|_| Fail)?;
            let min = u32::try_from(self.tile_num).map_err(|_| Fail)?;
            let tg_start = r.fc(tile_bits, min, max)?;
            i64::from(r.fc(tile_bits, tg_start, max)?)
        };
        self.tile_num = tg_end + 1;
        r.byte_alignment()?;
        if tg_end == num_tiles - 1 {
            self.seen_frame_header = false;
        }
        Ok(())
    }
}
