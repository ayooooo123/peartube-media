// Ported from FFmpeg (commit 2da55bf): libavcodec/vp56.c (ff_vp56_init_dequant,
// vp56_get_vectors_predictors, vp56_parse_mb_type_models, vp56_parse_mb_type,
// vp56_decode_4mv, vp56_decode_mv, vp56_conceal_mv, vp56_add_predictors_dc,
// vp56_deblock_filter, vp56_mc, vp56_render_mb, vp56_decode_mb,
// vp56_conceal_mb, vp56_size_changed, ff_vp56_decode_mbs,
// ff_vp56_init_context), vp56.h (the context and models), vp5.c
// (vp5_parse_header, vp5_parse_vector_adjustment, vp5_parse_vector_models,
// vp5_parse_coeff_models, vp5_parse_coeff, vp5_default_models_init), vp6.c
// (vp6_parse_header, vp6_coeff_order_table_init, vp6_default_models_init,
// vp6_parse_vector_models, vp6_parse_coeff_models,
// vp6_parse_vector_adjustment, vp6_get_nb_null, vp6_parse_coeff_huffman,
// vp6_parse_coeff, vp6_filter, vp6_filter_diag2), vp56data.c and
// videodsp_template.c (emulated_edge_mc) and libavcodec/utils.c
// (ff_set_dimensions).
// License: LGPL-2.1-or-later

//! One VP5 or VP6 decoding context (VP6A runs a second one for the alpha
//! plane). Pictures are decoded in bitstream order, top row first; the
//! flipped codecs (VP5, VP6 outside Flash) come out flipped vertically at
//! output, which is what FFmpeg's negative strides from the bottom of its
//! buffer amount to.

use std::sync::Arc;

use crate::dsp;
use crate::huffman::Huffman;
use crate::rac::{Bits, Rac, Tree};
use crate::tables::*;

/// VP56Frame.
const FRAME_NONE: i8 = -1;
const FRAME_CURRENT: i8 = 0;
const FRAME_PREVIOUS: i8 = 1;
const FRAME_GOLDEN: i8 = 2;

/// VP56mb.
const MB_INTER_NOVEC_PF: u8 = 0;
const MB_INTRA: u8 = 1;
const MB_INTER_DELTA_PF: u8 = 2;
const MB_INTER_V1_PF: u8 = 3;
const MB_INTER_V2_PF: u8 = 4;
const MB_INTER_NOVEC_GF: u8 = 5;
const MB_INTER_DELTA_GF: u8 = 6;
const MB_INTER_4V: u8 = 7;
const MB_INTER_V1_GF: u8 = 8;
const MB_INTER_V2_GF: u8 = 9;

/// ff_vp56_reference_frame.
const REFERENCE_FRAME: [i8; 10] = [
    FRAME_PREVIOUS,
    FRAME_CURRENT,
    FRAME_PREVIOUS,
    FRAME_PREVIOUS,
    FRAME_PREVIOUS,
    FRAME_GOLDEN,
    FRAME_GOLDEN,
    FRAME_PREVIOUS,
    FRAME_GOLDEN,
    FRAME_GOLDEN,
];

/// ff_vp56_pva_tree.
const PVA_TREE: &Tree = &[(8, 0), (4, 1), (2, 2), (0, 0), (-1, 0), (2, 3), (-2, 0), (-3, 0), (4, 4), (2, 5), (-4, 0), (-5, 0), (2, 6), (-6, 0), (-7, 0)];
/// ff_vp56_pc_tree.
const PC_TREE: &Tree = &[(4, 6), (2, 7), (0, 0), (-1, 0), (4, 8), (2, 9), (-2, 0), (-3, 0), (2, 10), (-4, 0), (-5, 0)];
/// ff_vp56_pmbtm_tree.
const PMBTM_TREE: &Tree = &[(4, 0), (2, 1), (-8, 0), (-4, 0), (8, 2), (6, 3), (4, 4), (2, 5), (-24, 0), (-20, 0), (-16, 0), (-12, 0), (0, 0)];
/// ff_vp56_pmbt_tree.
const PMBT_TREE: &Tree = &[
    (8, 1),
    (4, 2),
    (2, 4),
    (-(MB_INTER_NOVEC_PF as i8), 0),
    (-(MB_INTER_DELTA_PF as i8), 0),
    (2, 5),
    (-(MB_INTER_V1_PF as i8), 0),
    (-(MB_INTER_V2_PF as i8), 0),
    (4, 3),
    (2, 6),
    (-(MB_INTRA as i8), 0),
    (-(MB_INTER_4V as i8), 0),
    (4, 7),
    (2, 8),
    (-(MB_INTER_NOVEC_GF as i8), 0),
    (-(MB_INTER_DELTA_GF as i8), 0),
    (2, 9),
    (-(MB_INTER_V1_GF as i8), 0),
    (-(MB_INTER_V2_GF as i8), 0),
];
/// vp6_pcr_tree.
const PCR_TREE: &Tree = &[(8, 0), (4, 1), (2, 2), (-1, 0), (-2, 0), (2, 3), (-3, 0), (-4, 0), (8, 4), (4, 5), (2, 6), (-5, 0), (-6, 0), (2, 7), (-7, 0), (-8, 0), (0, 0)];

/// VP5 or VP6.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Flavor {
    Vp5,
    Vp6,
}

/// What the decoder's AVCodecContext holds: the picture size, the coded
/// size and the extradata, shared by the main and alpha contexts.
#[derive(Clone, Default, Debug)]
pub(crate) struct Avctx {
    pub width: i32,
    pub height: i32,
    pub coded_width: i32,
    pub coded_height: i32,
    pub extradata: Vec<u8>,
}

impl Avctx {
    /// ff_set_dimensions (av_image_check_size2's limits).
    fn set_dimensions(&mut self, w: i32, h: i32) -> bool {
        let ok = w > 0 && h > 0 && ((w as i64 + 128) * (h as i64 + 128)) < (i32::MAX as i64 / 8);
        let (w, h) = if ok { (w, h) } else { (0, 0) };
        self.width = w;
        self.coded_width = w;
        self.height = h;
        self.coded_height = h;
        ok
    }
}

/// A decoded picture at the coded size: Y, U, V and, for VP6A, alpha.
#[derive(Clone)]
pub(crate) struct Picture {
    pub planes: [Vec<u8>; 4],
    pub stride: [usize; 4],
    pub height: [usize; 4],
}

impl Picture {
    pub fn new(width: usize, height: usize, alpha: bool) -> Self {
        let (cw, ch) = (width / 2, height / 2);
        let a = if alpha { width * height } else { 0 };
        Self {
            planes: [vec![0; width * height], vec![0; cw * ch], vec![0; cw * ch], vec![0; a]],
            stride: [width, cw, cw, if alpha { width } else { 0 }],
            height: [height, ch, ch, if alpha { height } else { 0 }],
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct Mv {
    x: i16,
    y: i16,
}

#[derive(Clone, Copy)]
struct RefDc {
    not_null_dc: u8,
    ref_frame: i8,
    dc_coeff: i16,
}

impl Default for RefDc {
    fn default() -> Self {
        Self { not_null_dc: 0, ref_frame: FRAME_NONE, dc_coeff: 0 }
    }
}

#[derive(Clone, Copy, Default)]
struct Macroblock {
    ty: u8,
    mv: Mv,
}

/// VP56Model.
#[derive(Clone)]
struct Model {
    coeff_reorder: [u8; 64],
    coeff_index_to_pos: [u8; 64],
    coeff_index_to_idct_selector: [u8; 64],
    vector_sig: [u8; 2],
    vector_dct: [u8; 2],
    vector_pdi: [[u8; 2]; 2],
    vector_pdv: [[u8; 7]; 2],
    vector_fdv: [[u8; 8]; 2],
    coeff_dccv: [[u8; 11]; 2],
    coeff_ract: [[[[u8; 11]; 6]; 3]; 2],
    coeff_acct: [[[[[u8; 5]; 6]; 3]; 3]; 2],
    coeff_dcct: [[[u8; 5]; 36]; 2],
    coeff_runv: [[u8; 14]; 2],
    mb_type: [[[u8; 10]; 10]; 3],
    mb_types_stats: [[[u8; 2]; 10]; 3],
}

impl Default for Model {
    fn default() -> Self {
        Self {
            coeff_reorder: [0; 64],
            coeff_index_to_pos: [0; 64],
            coeff_index_to_idct_selector: [0; 64],
            vector_sig: [0; 2],
            vector_dct: [0; 2],
            vector_pdi: [[0; 2]; 2],
            vector_pdv: [[0; 7]; 2],
            vector_fdv: [[0; 8]; 2],
            coeff_dccv: [[0; 11]; 2],
            coeff_ract: [[[[0; 11]; 6]; 3]; 2],
            coeff_acct: [[[[[0; 5]; 6]; 3]; 3]; 2],
            coeff_dcct: [[[0; 5]; 36]; 2],
            coeff_runv: [[0; 14]; 2],
            mb_type: [[[0; 10]; 10]; 3],
            mb_types_stats: [[[0; 2]; 10]; 3],
        }
    }
}

/// A decoding error: FFmpeg's AVERROR_INVALIDDATA.
#[derive(Debug)]
pub(crate) struct Invalid(pub &'static str);

type Res<T> = Result<T, Invalid>;

/// RSHIFT (libavutil/common.h).
fn rshift(a: i32, b: u32) -> i32 {
    if a > 0 {
        (a + ((1 << b) >> 1)) >> b
    } else {
        (a + ((1 << b) >> 1) - 1) >> b
    }
}

/// VP56Context.
pub(crate) struct Vp56 {
    flavor: Flavor,
    /// This context decodes VP6A's alpha plane (blocks 6.. of b2p).
    is_alpha: bool,
    pub key: bool,
    c: Rac,
    cc: Rac,
    use_cc: bool,
    gb: Bits,
    sub_version: i32,
    pub golden_frame: bool,
    plane_width: [usize; 4],
    plane_height: [usize; 4],
    mb_width: usize,
    mb_height: usize,
    block_offset: [isize; 6],
    quantizer: i32,
    dequant_dc: i32,
    dequant_ac: i32,
    above_blocks: Vec<RefDc>,
    left_block: [RefDc; 4],
    above_block_idx: [usize; 6],
    prev_dc: [[i16; 3]; 3],
    mb_type: u8,
    macroblocks: Vec<Macroblock>,
    block_coeff: [[i16; 64]; 6],
    idct_selector: [i32; 6],
    mv: [Mv; 6],
    vector_candidate: [Mv; 2],
    vector_candidate_pos: usize,
    filter_header: i32,
    deblock_filtering: bool,
    filter_selection: i32,
    filter_mode: i32,
    max_vector_length: i32,
    sample_variance_threshold: i32,
    bounding_values: [i32; 256],
    coeff_ctx: [[u8; 64]; 4],
    coeff_ctx_last: [u8; 4],
    interlaced: bool,
    il_coeff_reorder: bool,
    il_prob: i32,
    il_block: bool,
    stride: [isize; 4],
    model: Box<Model>,
    use_huffman: bool,
    /// Coefficients come from the Huffman bit reader (a separate partition
    /// of a Huffman frame); else from the range coder.
    huffman_coeffs: bool,
    dccv_vlc: [Huffman; 2],
    runv_vlc: [Huffman; 2],
    ract_vlc: [[[Huffman; 4]; 3]; 2],
    nb_null: [[u32; 2]; 2],
    have_undamaged_frame: bool,
    pub discard_frame: bool,
    idct_scantable: [u8; 64],
    /// edge_emu_buffer.
    edge: Vec<u8>,
    /// The frames FFmpeg keeps: previous and golden.
    pub prev: Option<Arc<Picture>>,
    pub golden: Option<Arc<Picture>>,
}

impl Vp56 {
    /// ff_vp56_init_context and vp5/vp6_decode_init.
    pub fn new(flavor: Flavor, is_alpha: bool) -> Self {
        let mut idct_scantable = [0u8; 64];
        for (i, t) in idct_scantable.iter_mut().enumerate() {
            let x = ZIGZAG_DIRECT[i];
            *t = (x >> 3) | ((x & 7) << 3);
        }
        Self {
            flavor,
            is_alpha,
            key: false,
            c: Rac::default(),
            cc: Rac::default(),
            use_cc: false,
            gb: Bits::default(),
            sub_version: 0,
            golden_frame: false,
            plane_width: [0; 4],
            plane_height: [0; 4],
            mb_width: 0,
            mb_height: 0,
            block_offset: [0; 6],
            quantizer: -1,
            dequant_dc: 0,
            dequant_ac: 0,
            above_blocks: Vec::new(),
            left_block: [RefDc::default(); 4],
            above_block_idx: [0; 6],
            prev_dc: [[0; 3]; 3],
            mb_type: 0,
            macroblocks: Vec::new(),
            block_coeff: [[0; 64]; 6],
            idct_selector: [0; 6],
            mv: [Mv::default(); 6],
            vector_candidate: [Mv::default(); 2],
            vector_candidate_pos: 0,
            filter_header: 0,
            // ff_vp56_init_context sets 1; vp6_decode_init_context 0.
            deblock_filtering: flavor == Flavor::Vp5,
            filter_selection: 0,
            filter_mode: 0,
            max_vector_length: 0,
            sample_variance_threshold: 0,
            bounding_values: [0; 256],
            coeff_ctx: [[0; 64]; 4],
            coeff_ctx_last: [0; 4],
            interlaced: false,
            il_coeff_reorder: false,
            il_prob: 0,
            il_block: false,
            stride: [0; 4],
            model: Box::default(),
            use_huffman: false,
            huffman_coeffs: false,
            dccv_vlc: Default::default(),
            runv_vlc: Default::default(),
            ract_vlc: Default::default(),
            nb_null: [[0; 2]; 2],
            have_undamaged_frame: false,
            discard_frame: false,
            idct_scantable,
            edge: Vec::new(),
            prev: None,
            golden: None,
        }
    }

    /// Whether the macroblock arrays exist (FFmpeg's first-frame test).
    fn has_macroblocks(&self) -> bool {
        !self.macroblocks.is_empty()
    }

    /// ff_vp56_init_dequant.
    fn init_dequant(&mut self, quantizer: i32) {
        let q = quantizer as usize & 63;
        if self.quantizer != quantizer {
            self.bounding_values = dsp::bounding_values(i32::from(FILTER_THRESHOLD[q]));
        }
        self.quantizer = quantizer;
        self.dequant_dc = i32::from(DC_DEQUANT[q]) << 2;
        self.dequant_ac = i32::from(AC_DEQUANT[q]) << 2;
    }

    /// The coefficient range coder (VP6's ccp; the main one for VP5).
    fn ccp(&mut self) -> &mut Rac {
        if self.use_cc {
            &mut self.cc
        } else {
            &mut self.c
        }
    }

    // ───────────────────────── headers ─────────────────────────

    /// The codec's parse_header on `buf` (the packet from the frame's
    /// start) of `size` bytes: whether the picture size changed.
    pub fn parse_header(&mut self, avctx: &mut Avctx, buf: &[u8], size: i64) -> Res<bool> {
        match self.flavor {
            Flavor::Vp5 => self.vp5_parse_header(avctx, buf, size),
            Flavor::Vp6 => self.vp6_parse_header(avctx, buf, size),
        }
    }

    /// vp5_parse_header.
    fn vp5_parse_header(&mut self, avctx: &mut Avctx, buf: &[u8], size: i64) -> Res<bool> {
        self.c = Rac::new(buf, size).ok_or(Invalid("vp5: empty frame"))?;
        self.key = !self.c.get();
        self.c.get();
        let q = self.c.gets(6);
        self.init_dequant(q);
        if self.key {
            self.c.gets(8);
            if self.c.gets(5) > 5 {
                return Err(Invalid("vp5: unsupported version"));
            }
            self.c.gets(2);
            self.interlaced = self.c.gets(1) != 0;
            let rows = self.c.gets(8);
            let cols = self.c.gets(8);
            if rows == 0 || cols == 0 {
                return Err(Invalid("vp5: invalid size"));
            }
            let render_y = self.c.gets(8);
            let render_x = self.c.gets(8);
            if render_x == 0 || render_x > cols || render_y == 0 || render_y > rows {
                return Err(Invalid("vp5: invalid render size"));
            }
            self.c.gets(2);
            if !self.has_macroblocks() || 16 * cols != avctx.coded_width || 16 * rows != avctx.coded_height {
                if !avctx.set_dimensions(16 * cols, 16 * rows) {
                    return Err(Invalid("vp5: invalid dimensions"));
                }
                return Ok(true);
            }
        } else if !self.has_macroblocks() {
            return Err(Invalid("vp5: inter frame first"));
        }
        Ok(false)
    }

    /// vp6_parse_header.
    fn vp6_parse_header(&mut self, avctx: &mut Avctx, buf: &[u8], size: i64) -> Res<bool> {
        let byte = |b: &[u8], i: usize| b.get(i).copied().unwrap_or(0);
        let mut buf = buf;
        let mut size = size;
        let mut res = false;
        let mut coeff_offset: i64 = 0;
        let mut vrt_shift = 0;
        let mut parse_filter_info = false;
        let separated_coeff = byte(buf, 0) & 1 != 0;
        self.key = byte(buf, 0) & 0x80 == 0;
        self.init_dequant(i32::from((byte(buf, 0) >> 1) & 0x3F));
        if self.key {
            let sub_version = i32::from(byte(buf, 1) >> 3);
            if sub_version > 8 {
                return Err(Invalid("vp6: unsupported sub_version"));
            }
            self.filter_header = i32::from(byte(buf, 1) & 0x06);
            self.interlaced = byte(buf, 1) & 1 != 0;
            self.il_coeff_reorder = self.interlaced;
            if separated_coeff || self.filter_header == 0 {
                coeff_offset = i64::from(u16::from_be_bytes([byte(buf, 2), byte(buf, 3)])) - 2;
                buf = buf.get(2..).unwrap_or(&[]);
                size -= 2;
            }
            let rows = i32::from(byte(buf, 2));
            let cols = i32::from(byte(buf, 3));
            if rows == 0 || cols == 0 {
                return Err(Invalid("vp6: invalid size"));
            }
            if !self.has_macroblocks() || 16 * cols != avctx.coded_width || 16 * rows != avctx.coded_height {
                let align = |v: i32| (v + 15) & !15;
                if avctx.extradata.is_empty() && align(avctx.width) == 16 * cols && align(avctx.height) == 16 * rows {
                    // Container cropping (F4V): only the coded size changes.
                    avctx.coded_width = 16 * cols;
                    avctx.coded_height = 16 * rows;
                } else {
                    if !avctx.set_dimensions(16 * cols, 16 * rows) {
                        return Err(Invalid("vp6: invalid dimensions"));
                    }
                    if avctx.extradata.len() == 1 {
                        avctx.width -= i32::from(avctx.extradata[0] >> 4);
                        avctx.height -= i32::from(avctx.extradata[0] & 0x0F);
                    }
                }
                res = true;
            }
            match Rac::new(buf.get(6..).unwrap_or(&[]), size - 6) {
                Some(c) => self.c = c,
                None => {
                    if res {
                        avctx.set_dimensions(0, 0);
                    }
                    return Err(Invalid("vp6: empty frame"));
                }
            }
            self.c.gets(2);
            parse_filter_info = self.filter_header != 0;
            if sub_version < 8 {
                vrt_shift = 5;
            }
            self.sub_version = sub_version;
            self.golden_frame = false;
        } else {
            if self.sub_version == 0 || avctx.coded_width == 0 || avctx.coded_height == 0 {
                return Err(Invalid("vp6: inter frame without a key frame"));
            }
            if separated_coeff || self.filter_header == 0 {
                coeff_offset = i64::from(u16::from_be_bytes([byte(buf, 1), byte(buf, 2)])) - 2;
                buf = buf.get(2..).unwrap_or(&[]);
                size -= 2;
            }
            self.c = Rac::new(buf.get(1..).unwrap_or(&[]), size - 1).ok_or(Invalid("vp6: empty frame"))?;
            self.golden_frame = self.c.get();
            if self.filter_header != 0 {
                self.deblock_filtering = self.c.get();
                if self.deblock_filtering {
                    self.c.get();
                }
                if self.sub_version > 7 {
                    parse_filter_info = self.c.get();
                }
            }
        }
        if parse_filter_info {
            if self.c.get() {
                self.filter_mode = 2;
                self.sample_variance_threshold = self.c.gets(5) << vrt_shift;
                self.max_vector_length = 2 << self.c.gets(3);
            } else if self.c.get() {
                self.filter_mode = 1;
            } else {
                self.filter_mode = 0;
            }
            self.filter_selection = if self.sub_version > 7 { self.c.gets(4) } else { 16 };
        }
        self.use_huffman = self.c.get();
        self.huffman_coeffs = false;
        self.use_cc = false;
        if coeff_offset != 0 {
            size -= coeff_offset;
            if size < 0 {
                if res {
                    avctx.set_dimensions(0, 0);
                }
                return Err(Invalid("vp6: coefficient partition past the frame"));
            }
            let rest = usize::try_from(coeff_offset).ok().and_then(|o| buf.get(o..)).unwrap_or(&[]);
            if self.use_huffman {
                self.huffman_coeffs = true;
                self.gb = Bits::new(rest, size as usize);
            } else {
                match Rac::new(rest, size) {
                    Some(cc) => self.cc = cc,
                    None => {
                        if res {
                            avctx.set_dimensions(0, 0);
                        }
                        return Err(Invalid("vp6: empty coefficient partition"));
                    }
                }
                self.use_cc = true;
            }
        }
        Ok(res)
    }

    // ───────────────────────── size ─────────────────────────

    /// vp56_size_changed: planes and macroblock arrays for the coded size.
    pub fn size_changed(&mut self, avctx: &Avctx) -> Res<()> {
        let (w, h) = (avctx.coded_width.max(0) as usize, avctx.coded_height.max(0) as usize);
        self.plane_width = [w, w / 2, w / 2, w];
        self.plane_height = [h, h / 2, h / 2, h];
        self.have_undamaged_frame = false;
        self.stride = [w as isize, (w / 2) as isize, (w / 2) as isize, w as isize];
        self.mb_width = w.div_ceil(16);
        self.mb_height = h.div_ceil(16);
        if self.mb_width > 1000 || self.mb_height > 1000 {
            return Err(Invalid("vp56: picture too large"));
        }
        self.above_blocks = vec![RefDc::default(); 4 * self.mb_width + 6];
        self.macroblocks = vec![Macroblock::default(); self.mb_width * self.mb_height];
        self.edge = vec![0; 16 * w * 2];
        Ok(())
    }

    // ───────────────────────── models ─────────────────────────

    fn default_models_init(&mut self) {
        let m = &mut *self.model;
        match self.flavor {
            Flavor::Vp5 => {
                for i in 0..2 {
                    m.vector_sig[i] = 0x80;
                    m.vector_dct[i] = 0x80;
                    m.vector_pdi[i][0] = 0x55;
                    m.vector_pdi[i][1] = 0x80;
                }
                m.mb_types_stats = DEF_MB_TYPES_STATS;
                m.vector_pdv = [[0x80; 7]; 2];
            }
            Flavor::Vp6 => {
                m.vector_dct = [0xA2, 0xA4];
                m.vector_sig = [0x80, 0x80];
                m.mb_types_stats = DEF_MB_TYPES_STATS;
                m.vector_fdv = VP6_DEF_FDV_VECTOR_MODEL;
                m.vector_pdv = VP6_DEF_PDV_VECTOR_MODEL;
                m.coeff_runv = VP6_DEF_RUNV_COEFF_MODEL;
                m.coeff_reorder = if self.il_coeff_reorder { VP6_IL_COEFF_REORDER } else { VP6_DEF_COEFF_REORDER };
                self.coeff_order_table_init();
            }
        }
    }

    /// vp6_coeff_order_table_init.
    fn coeff_order_table_init(&mut self) {
        let m = &mut *self.model;
        let mut idx = 1;
        m.coeff_index_to_pos[0] = 0;
        for i in 0..16 {
            for pos in 1..64 {
                if m.coeff_reorder[pos] == i && idx < 64 {
                    m.coeff_index_to_pos[idx] = pos as u8;
                    idx += 1;
                }
            }
        }
        for idx in 0..64 {
            let mut max = (0..=idx).map(|i| m.coeff_index_to_pos[i]).max().unwrap_or(0);
            if self.sub_version > 6 {
                max += 1;
            }
            m.coeff_index_to_idct_selector[idx] = max;
        }
    }

    /// vp56_parse_mb_type_models.
    fn parse_mb_type_models(&mut self) {
        let c = &mut self.c;
        let m = &mut *self.model;
        for ctx in 0..3 {
            if c.get_prob(174) {
                let idx = c.gets(4) as usize;
                m.mb_types_stats[ctx] = PRE_DEF_MB_TYPE_STATS[idx][ctx];
            }
            if c.get_prob(254) {
                for ty in 0..10 {
                    for i in 0..2 {
                        if c.get_prob(205) {
                            let sign = i32::from(c.get());
                            let mut delta = c.get_tree(PMBTM_TREE, &MB_TYPE_MODEL_MODEL);
                            if delta == 0 {
                                delta = 4 * c.gets(7);
                            }
                            let v = (delta ^ -sign) + sign;
                            m.mb_types_stats[ctx][ty][i] = m.mb_types_stats[ctx][ty][i].wrapping_add(v as u8);
                        }
                    }
                }
            }
        }
        // MB type probabilities by the previous MB type.
        for ctx in 0..3 {
            let stats = m.mb_types_stats[ctx];
            let mut p = [0i32; 10];
            for ty in 0..10 {
                p[ty] = 100 * i32::from(stats[ty][1]);
            }
            for ty in 0..10 {
                let (s0, s1) = (i32::from(stats[ty][0]), i32::from(stats[ty][1]));
                m.mb_type[ctx][ty][0] = (255 - (255 * s0) / (1 + s0 + s1)) as u8;
                p[ty] = 0;
                let p02 = p[0] + p[2];
                let p34 = p[3] + p[4];
                let p0234 = p02 + p34;
                let p17 = p[1] + p[7];
                let p56 = p[5] + p[6];
                let p89 = p[8] + p[9];
                let p5689 = p56 + p89;
                let p156789 = p17 + p5689;
                let t = &mut m.mb_type[ctx][ty];
                t[1] = (1 + 255 * p0234 / (1 + p0234 + p156789)) as u8;
                t[2] = (1 + 255 * p02 / (1 + p0234)) as u8;
                t[3] = (1 + 255 * p17 / (1 + p156789)) as u8;
                t[4] = (1 + 255 * p[0] / (1 + p02)) as u8;
                t[5] = (1 + 255 * p[3] / (1 + p34)) as u8;
                t[6] = (1 + 255 * p[1] / (1 + p17)) as u8;
                t[7] = (1 + 255 * p56 / (1 + p5689)) as u8;
                t[8] = (1 + 255 * p[5] / (1 + p56)) as u8;
                t[9] = (1 + 255 * p[8] / (1 + p89)) as u8;
                p[ty] = 100 * s1;
            }
        }
    }

    fn parse_vector_models(&mut self) {
        let c = &mut self.c;
        let m = &mut *self.model;
        match self.flavor {
            Flavor::Vp5 => {
                for comp in 0..2 {
                    if c.get_prob(VP5_VMC_PCT[comp][0]) {
                        m.vector_dct[comp] = c.gets_nn();
                    }
                    if c.get_prob(VP5_VMC_PCT[comp][1]) {
                        m.vector_sig[comp] = c.gets_nn();
                    }
                    if c.get_prob(VP5_VMC_PCT[comp][2]) {
                        m.vector_pdi[comp][0] = c.gets_nn();
                    }
                    if c.get_prob(VP5_VMC_PCT[comp][3]) {
                        m.vector_pdi[comp][1] = c.gets_nn();
                    }
                }
                for comp in 0..2 {
                    for node in 0..7 {
                        if c.get_prob(VP5_VMC_PCT[comp][4 + node]) {
                            m.vector_pdv[comp][node] = c.gets_nn();
                        }
                    }
                }
            }
            Flavor::Vp6 => {
                for comp in 0..2 {
                    if c.get_prob(VP6_SIG_DCT_PCT[comp][0]) {
                        m.vector_dct[comp] = c.gets_nn();
                    }
                    if c.get_prob(VP6_SIG_DCT_PCT[comp][1]) {
                        m.vector_sig[comp] = c.gets_nn();
                    }
                }
                for comp in 0..2 {
                    for node in 0..7 {
                        if c.get_prob(VP6_PDV_PCT[comp][node]) {
                            m.vector_pdv[comp][node] = c.gets_nn();
                        }
                    }
                }
                for comp in 0..2 {
                    for node in 0..8 {
                        if c.get_prob(VP6_FDV_PCT[comp][node]) {
                            m.vector_fdv[comp][node] = c.gets_nn();
                        }
                    }
                }
            }
        }
    }

    /// The codec's parse_coeff_models; false where FFmpeg's fails (a
    /// Huffman table it cannot build).
    fn parse_coeff_models(&mut self) -> bool {
        let key = self.key;
        let c = &mut self.c;
        let m = &mut *self.model;
        let mut def_prob = [0x80u8; 11];
        let (dccv_pct, ract_pct) = match self.flavor {
            Flavor::Vp5 => (&VP5_DCCV_PCT, &VP5_RACT_PCT),
            Flavor::Vp6 => (&VP6_DCCV_PCT, &VP6_RACT_PCT),
        };
        for pt in 0..2 {
            for node in 0..11 {
                if c.get_prob(dccv_pct[pt][node]) {
                    def_prob[node] = c.gets_nn();
                    m.coeff_dccv[pt][node] = def_prob[node];
                } else if key {
                    m.coeff_dccv[pt][node] = def_prob[node];
                }
            }
        }
        if self.flavor == Flavor::Vp6 {
            if c.get() {
                for pos in 1..64 {
                    if c.get_prob(VP6_COEFF_REORDER_PCT[pos]) {
                        m.coeff_reorder[pos] = c.gets(4) as u8;
                    }
                }
                self.coeff_order_table_init();
            }
            let c = &mut self.c;
            let m = &mut *self.model;
            for cg in 0..2 {
                for node in 0..14 {
                    if c.get_prob(VP6_RUNV_PCT[cg][node]) {
                        m.coeff_runv[cg][node] = c.gets_nn();
                    }
                }
            }
        }
        let c = &mut self.c;
        let m = &mut *self.model;
        for ct in 0..3 {
            for pt in 0..2 {
                for cg in 0..6 {
                    for node in 0..11 {
                        if c.get_prob(ract_pct[ct][pt][cg][node]) {
                            def_prob[node] = c.gets_nn();
                            m.coeff_ract[pt][ct][cg][node] = def_prob[node];
                        } else if key {
                            m.coeff_ract[pt][ct][cg][node] = def_prob[node];
                        }
                    }
                }
            }
        }
        let clip = |v: i32, hi: i32| v.clamp(1, hi) as u8;
        match self.flavor {
            Flavor::Vp5 => {
                for pt in 0..2 {
                    for ctx in 0..36 {
                        for node in 0..5 {
                            let lc = VP5_DCCV_LC[node][ctx];
                            m.coeff_dcct[pt][ctx][node] =
                                clip(((i32::from(m.coeff_dccv[pt][node]) * i32::from(lc[0]) + 128) >> 8) + i32::from(lc[1]), 254);
                        }
                    }
                }
                for ct in 0..3 {
                    for pt in 0..2 {
                        for cg in 0..3 {
                            for ctx in 0..6 {
                                for node in 0..5 {
                                    let lc = VP5_RACT_LC[ct][cg][node][ctx];
                                    m.coeff_acct[pt][ct][cg][ctx][node] =
                                        clip(((i32::from(m.coeff_ract[pt][ct][cg][node]) * i32::from(lc[0]) + 128) >> 8) + i32::from(lc[1]), 254);
                                }
                            }
                        }
                    }
                }
            }
            Flavor::Vp6 => {
                if self.use_huffman {
                    for pt in 0..2 {
                        let Some(t) = Huffman::build(&m.coeff_dccv[pt], &VP6_HUFF_COEFF_MAP, 12) else { return false };
                        self.dccv_vlc[pt] = t;
                        let Some(t) = Huffman::build(&m.coeff_runv[pt], &VP6_HUFF_RUN_MAP, 9) else { return false };
                        self.runv_vlc[pt] = t;
                        for ct in 0..3 {
                            for cg in 0..4 {
                                let Some(t) = Huffman::build(&m.coeff_ract[pt][ct][cg], &VP6_HUFF_COEFF_MAP, 12) else { return false };
                                self.ract_vlc[pt][ct][cg] = t;
                            }
                        }
                    }
                    self.nb_null = [[0; 2]; 2];
                } else {
                    for pt in 0..2 {
                        for ctx in 0..3 {
                            for node in 0..5 {
                                let lc = VP6_DCCV_LC[ctx][node];
                                m.coeff_dcct[pt][ctx][node] = clip(((i32::from(m.coeff_dccv[pt][node]) * lc[0] + 128) >> 8) + lc[1], 255);
                            }
                        }
                    }
                }
            }
        }
        true
    }

    // ───────────────────────── vectors ─────────────────────────

    /// vp56_get_vectors_predictors.
    fn get_vectors_predictors(&mut self, row: usize, col: usize, ref_frame: i8) -> usize {
        let mut nb_pred: i32 = 0;
        let mut vect = [Mv::default(); 2];
        for (pos, &[px, py]) in CANDIDATE_PREDICTOR_POS.iter().enumerate() {
            let x = col as i32 + i32::from(px);
            let y = row as i32 + i32::from(py);
            if x < 0 || x >= self.mb_width as i32 || y < 0 || y >= self.mb_height as i32 {
                continue;
            }
            let mb = self.macroblocks[x as usize + self.mb_width * y as usize];
            if REFERENCE_FRAME[usize::from(mb.ty).min(9)] != ref_frame {
                continue;
            }
            if mb.mv == vect[0] || mb.mv == Mv::default() {
                continue;
            }
            vect[nb_pred as usize] = mb.mv;
            nb_pred += 1;
            if nb_pred > 1 {
                nb_pred = -1;
                break;
            }
            self.vector_candidate_pos = pos;
        }
        self.vector_candidate = vect;
        (nb_pred + 1) as usize
    }

    /// The codec's parse_vector_adjustment.
    fn parse_vector_adjustment(&mut self) -> Mv {
        let c = &mut self.c;
        let m = &self.model;
        match self.flavor {
            Flavor::Vp5 => {
                let mut v = Mv::default();
                for comp in 0..2 {
                    let mut delta = 0;
                    if c.get_prob(m.vector_dct[comp]) {
                        let sign = i32::from(c.get_prob(m.vector_sig[comp]));
                        let mut di = i32::from(c.get_prob(m.vector_pdi[comp][0]));
                        di |= i32::from(c.get_prob(m.vector_pdi[comp][1])) << 1;
                        delta = c.get_tree(PVA_TREE, &m.vector_pdv[comp]);
                        delta = di | (delta << 2);
                        delta = (delta ^ -sign) + sign;
                    }
                    if comp == 0 {
                        v.x = delta as i16;
                    } else {
                        v.y = delta as i16;
                    }
                }
                v
            }
            Flavor::Vp6 => {
                let mut v = Mv::default();
                if self.vector_candidate_pos < 2 {
                    v = self.vector_candidate[0];
                }
                for comp in 0..2 {
                    let mut delta: i32 = 0;
                    if c.get_prob(m.vector_dct[comp]) {
                        const PROB_ORDER: [usize; 7] = [0, 1, 2, 7, 6, 5, 4];
                        for j in PROB_ORDER {
                            delta |= i32::from(c.get_prob(m.vector_fdv[comp][j])) << j;
                        }
                        if delta & 0xF0 != 0 {
                            delta |= i32::from(c.get_prob(m.vector_fdv[comp][3])) << 3;
                        } else {
                            delta |= 8;
                        }
                    } else {
                        delta = c.get_tree(PVA_TREE, &m.vector_pdv[comp]);
                    }
                    if delta != 0 && c.get_prob(m.vector_sig[comp]) {
                        delta = -delta;
                    }
                    if comp == 0 {
                        v.x = v.x.wrapping_add(delta as i16);
                    } else {
                        v.y = v.y.wrapping_add(delta as i16);
                    }
                }
                v
            }
        }
    }

    /// vp56_parse_mb_type.
    fn parse_mb_type(&mut self, prev_type: u8, ctx: usize) -> u8 {
        let model = self.model.mb_type[ctx][usize::from(prev_type).min(9)];
        if self.c.get_prob(model[0]) {
            prev_type
        } else {
            self.c.get_tree(PMBT_TREE, &model) as u8
        }
    }

    /// vp56_decode_4mv.
    fn decode_4mv(&mut self, row: usize, col: usize) {
        let mut sum = (0i32, 0i32);
        let mut ty = [0u8; 4];
        for t in &mut ty {
            *t = self.c.gets(2) as u8;
            if *t != 0 {
                *t += 1;
            }
        }
        for b in 0..4 {
            match ty[b] {
                MB_INTER_NOVEC_PF => self.mv[b] = Mv::default(),
                MB_INTER_DELTA_PF => self.mv[b] = self.parse_vector_adjustment(),
                MB_INTER_V1_PF => self.mv[b] = self.vector_candidate[0],
                MB_INTER_V2_PF => self.mv[b] = self.vector_candidate[1],
                _ => {}
            }
            // VP56mv components are int16_t.
            sum.0 = i32::from((sum.0 as i16).wrapping_add(self.mv[b].x));
            sum.1 = i32::from((sum.1 as i16).wrapping_add(self.mv[b].y));
        }
        self.macroblocks[row * self.mb_width + col].mv = self.mv[3];
        let chroma = Mv { x: rshift(sum.0, 2) as i16, y: rshift(sum.1, 2) as i16 };
        self.mv[4] = chroma;
        self.mv[5] = chroma;
    }

    /// vp56_decode_mv.
    fn decode_mv(&mut self, row: usize, col: usize) -> u8 {
        let ctx = self.get_vectors_predictors(row, col, FRAME_PREVIOUS);
        self.mb_type = self.parse_mb_type(self.mb_type, ctx);
        let at = row * self.mb_width + col;
        self.macroblocks[at].ty = self.mb_type;
        let mv = match self.mb_type {
            MB_INTER_V1_PF => self.vector_candidate[0],
            MB_INTER_V2_PF => self.vector_candidate[1],
            MB_INTER_V1_GF => {
                self.get_vectors_predictors(row, col, FRAME_GOLDEN);
                self.vector_candidate[0]
            }
            MB_INTER_V2_GF => {
                self.get_vectors_predictors(row, col, FRAME_GOLDEN);
                self.vector_candidate[1]
            }
            MB_INTER_DELTA_PF => self.parse_vector_adjustment(),
            MB_INTER_DELTA_GF => {
                self.get_vectors_predictors(row, col, FRAME_GOLDEN);
                self.parse_vector_adjustment()
            }
            MB_INTER_4V => {
                self.decode_4mv(row, col);
                return self.mb_type;
            }
            _ => Mv::default(),
        };
        self.macroblocks[at].mv = mv;
        self.mv = [mv; 6];
        self.mb_type
    }

    /// vp56_conceal_mv.
    fn conceal_mv(&mut self, row: usize, col: usize) -> u8 {
        self.mb_type = MB_INTER_NOVEC_PF;
        let at = row * self.mb_width + col;
        self.macroblocks[at] = Macroblock { ty: self.mb_type, mv: Mv::default() };
        self.mv = [Mv::default(); 6];
        self.mb_type
    }

    // ───────────────────────── coefficients ─────────────────────────

    fn parse_coeff(&mut self) -> Res<()> {
        match self.flavor {
            Flavor::Vp5 => self.vp5_parse_coeff(),
            Flavor::Vp6 if self.huffman_coeffs => self.vp6_parse_coeff_huffman(),
            Flavor::Vp6 => self.vp6_parse_coeff(),
        }
    }

    /// vp5_parse_coeff.
    fn vp5_parse_coeff(&mut self) -> Res<()> {
        if self.c.is_end() {
            return Err(Invalid("vp5: coefficients past the frame"));
        }
        let mut pt = 0;
        for b in 0..6 {
            let mut ct = 1usize;
            if b > 3 {
                pt = 1;
            }
            let b4 = usize::from(B6TO4[b]);
            let ctx = 6 * usize::from(self.coeff_ctx[b4][0]) + usize::from(self.above_blocks[self.above_block_idx[b]].not_null_dc);
            let mut model1 = self.model.coeff_dccv[pt];
            let mut model2 = {
                let mut m = [0u8; 11];
                m[..5].copy_from_slice(&self.model.coeff_dcct[pt][ctx.min(35)]);
                m
            };
            let mut coeff_idx = 0usize;
            loop {
                if self.c.get_prob(model2[0]) {
                    let coeff;
                    let sign;
                    if self.c.get_prob(model2[2]) {
                        if self.c.get_prob(model2[3]) {
                            self.coeff_ctx[b4][coeff_idx] = 4;
                            let idx = self.c.get_tree(PC_TREE, &model1) as usize;
                            sign = i32::from(self.c.get());
                            let mut v = i32::from(COEFF_BIAS[idx + 5]);
                            for i in (0..=COEFF_BIT_LENGTH[idx]).rev() {
                                v += i32::from(self.c.get_prob(COEFF_PARSE_TABLE[idx][usize::from(i)])) << i;
                            }
                            coeff = v;
                        } else {
                            if self.c.get_prob(model2[4]) {
                                coeff = 3 + i32::from(self.c.get_prob(model1[5]));
                                self.coeff_ctx[b4][coeff_idx] = 3;
                            } else {
                                coeff = 2;
                                self.coeff_ctx[b4][coeff_idx] = 2;
                            }
                            sign = i32::from(self.c.get());
                        }
                        ct = 2;
                    } else {
                        ct = 1;
                        self.coeff_ctx[b4][coeff_idx] = 1;
                        sign = i32::from(self.c.get());
                        coeff = 1;
                    }
                    let mut coeff = (coeff ^ -sign) + sign;
                    if coeff_idx != 0 {
                        coeff = coeff.wrapping_mul(self.dequant_ac);
                    }
                    self.block_coeff[b][usize::from(self.idct_scantable[coeff_idx])] = coeff as i16;
                } else {
                    if ct != 0 && !self.c.get_prob(model2[1]) {
                        break;
                    }
                    ct = 0;
                    self.coeff_ctx[b4][coeff_idx] = 0;
                }
                coeff_idx += 1;
                if coeff_idx >= 64 {
                    break;
                }
                let cg = usize::from(VP5_COEFF_GROUPS[coeff_idx]);
                let ctx = usize::from(self.coeff_ctx[b4][coeff_idx]);
                model1 = self.model.coeff_ract[pt][ct][cg];
                model2 = if cg > 2 {
                    model1
                } else {
                    let a = self.model.coeff_acct[pt][ct][cg][ctx.min(5)];
                    let mut m = [0u8; 11];
                    m[..5].copy_from_slice(&a);
                    m
                };
            }
            let ctx_last = usize::from(self.coeff_ctx_last[b4].min(24));
            self.coeff_ctx_last[b4] = coeff_idx as u8;
            if coeff_idx < ctx_last {
                for i in coeff_idx..=ctx_last {
                    self.coeff_ctx[b4][i] = 5;
                }
            }
            let ab = self.above_block_idx[b];
            self.above_blocks[ab].not_null_dc = self.coeff_ctx[b4][0];
            self.idct_selector[b] = 63;
        }
        Ok(())
    }

    /// vp6_parse_coeff.
    fn vp6_parse_coeff(&mut self) -> Res<()> {
        if self.ccp().is_end() {
            return Err(Invalid("vp6: coefficients past the frame"));
        }
        let mut pt = 0;
        for b in 0..6 {
            let mut ct = 1usize;
            let mut run;
            if b > 3 {
                pt = 1;
            }
            let b4 = usize::from(B6TO4[b]);
            let ab = self.above_block_idx[b];
            let ctx = usize::from(self.left_block[b4].not_null_dc) + usize::from(self.above_blocks[ab].not_null_dc);
            let mut model1 = self.model.coeff_dccv[pt];
            let mut model2 = {
                let mut m = [0u8; 11];
                m[..5].copy_from_slice(&self.model.coeff_dcct[pt][ctx.min(35)]);
                m
            };
            let mut coeff_idx = 0usize;
            loop {
                let model2_0 = model2[0];
                let parse_coeff = (coeff_idx > 1 && ct == 0) || self.ccp().get_prob(model2_0);
                if parse_coeff {
                    let mut coeff;
                    let c = if self.use_cc { &mut self.cc } else { &mut self.c };
                    if c.get_prob(model2[2]) {
                        if c.get_prob(model2[3]) {
                            let idx = c.get_tree(PC_TREE, &model1) as usize;
                            coeff = i32::from(COEFF_BIAS[idx + 5]);
                            for i in (0..=COEFF_BIT_LENGTH[idx]).rev() {
                                coeff += i32::from(c.get_prob(COEFF_PARSE_TABLE[idx][usize::from(i)])) << i;
                            }
                        } else if c.get_prob(model2[4]) {
                            coeff = 3 + i32::from(c.get_prob(model1[5]));
                        } else {
                            coeff = 2;
                        }
                        ct = 2;
                    } else {
                        ct = 1;
                        coeff = 1;
                    }
                    let sign = i32::from(c.get());
                    coeff = (coeff ^ -sign) + sign;
                    if coeff_idx != 0 {
                        coeff = coeff.wrapping_mul(self.dequant_ac);
                    }
                    let idx = usize::from(self.model.coeff_index_to_pos[coeff_idx]);
                    self.block_coeff[b][usize::from(self.idct_scantable[idx])] = coeff as i16;
                    run = 1;
                } else {
                    ct = 0;
                    run = 1;
                    if coeff_idx > 0 {
                        let c = if self.use_cc { &mut self.cc } else { &mut self.c };
                        if !c.get_prob(model2[1]) {
                            break;
                        }
                        let model3 = self.model.coeff_runv[usize::from(coeff_idx >= 6)];
                        run = c.get_tree(PCR_TREE, &model3) as usize;
                        if run == 0 {
                            run = 9;
                            for i in 0..6 {
                                run += usize::from(c.get_prob(model3[i + 8])) << i;
                            }
                        }
                    }
                }
                coeff_idx += run;
                if coeff_idx >= 64 {
                    break;
                }
                let cg = usize::from(VP6_COEFF_GROUPS[coeff_idx]);
                model1 = self.model.coeff_ract[pt][ct][cg];
                model2 = model1;
            }
            let nn = u8::from(self.block_coeff[b][0] != 0);
            self.left_block[b4].not_null_dc = nn;
            self.above_blocks[ab].not_null_dc = nn;
            self.idct_selector[b] = i32::from(self.model.coeff_index_to_idct_selector[coeff_idx.min(63)]);
        }
        Ok(())
    }

    /// vp6_get_nb_null.
    fn get_nb_null(&mut self) -> u32 {
        let mut val = self.gb.get(2);
        if val == 2 {
            val += self.gb.get(2);
        } else if val == 3 {
            val = self.gb.get(1) << 2;
            val = 6 + val + self.gb.get(2 + val);
        }
        val
    }

    /// vp6_parse_coeff_huffman.
    fn vp6_parse_coeff_huffman(&mut self) -> Res<()> {
        let mut pt = 0;
        for b in 0..6 {
            let mut ct = 0usize;
            if b > 3 {
                pt = 1;
            }
            // dccv_vlc[pt] first, then ract_vlc[pt][ct][cg].
            let mut table: (bool, usize, usize) = (true, 0, 0);
            let mut coeff_idx = 0usize;
            loop {
                let mut run = 1;
                if coeff_idx < 2 && self.nb_null[coeff_idx][pt] != 0 {
                    self.nb_null[coeff_idx][pt] -= 1;
                    if coeff_idx != 0 {
                        break;
                    }
                } else {
                    if self.gb.bits_left() <= 0 {
                        return Err(Invalid("vp6: coefficients past the frame"));
                    }
                    let vlc = if table.0 { &self.dccv_vlc[pt] } else { &self.ract_vlc[pt][table.1][table.2] };
                    let coeff = vlc.decode(&mut self.gb);
                    if coeff == 0 {
                        if coeff_idx != 0 {
                            let pt2 = usize::from(coeff_idx >= 6);
                            run += self.runv_vlc[pt2].decode(&mut self.gb) as usize;
                            if run >= 9 {
                                run += self.gb.get(6) as usize;
                            }
                        } else {
                            self.nb_null[0][pt] = self.get_nb_null();
                        }
                        ct = 0;
                    } else if coeff == 11 {
                        // end of block
                        if coeff_idx == 1 {
                            self.nb_null[1][pt] = self.get_nb_null();
                        }
                        break;
                    } else {
                        let coeff = coeff.clamp(0, 10) as usize;
                        let mut coeff2 = i32::from(COEFF_BIAS[coeff]);
                        if coeff > 4 {
                            coeff2 += self.gb.get(if coeff <= 9 { coeff as u32 - 4 } else { 11 }) as i32;
                        }
                        ct = 1 + usize::from(coeff2 > 1);
                        let sign = self.gb.get(1) as i32;
                        coeff2 = (coeff2 ^ -sign) + sign;
                        if coeff_idx != 0 {
                            coeff2 = coeff2.wrapping_mul(self.dequant_ac);
                        }
                        let idx = usize::from(self.model.coeff_index_to_pos[coeff_idx]);
                        self.block_coeff[b][usize::from(self.idct_scantable[idx])] = coeff2 as i16;
                    }
                }
                coeff_idx += run;
                if coeff_idx >= 64 {
                    break;
                }
                let cg = usize::from(VP6_COEFF_GROUPS[coeff_idx]).min(3);
                table = (false, ct, cg);
            }
            self.idct_selector[b] = i32::from(self.model.coeff_index_to_idct_selector[coeff_idx.min(63)]);
        }
        Ok(())
    }

    // ───────────────────────── rendering ─────────────────────────

    /// vp56_add_predictors_dc.
    fn add_predictors_dc(&mut self, ref_frame: i8) {
        let idx = usize::from(self.idct_scantable[0]);
        for b in 0..6 {
            let ab = self.above_block_idx[b];
            let lb = usize::from(B6TO4[b]);
            let (mut count, mut dc) = (0, 0i32);
            if ref_frame == self.left_block[lb].ref_frame {
                dc += i32::from(self.left_block[lb].dc_coeff);
                count += 1;
            }
            if ref_frame == self.above_blocks[ab].ref_frame {
                dc += i32::from(self.above_blocks[ab].dc_coeff);
                count += 1;
            }
            if self.flavor == Flavor::Vp5 {
                for i in 0..2 {
                    let n = ab + 2 * i - 1;
                    if count < 2 && ref_frame == self.above_blocks[n].ref_frame {
                        dc += i32::from(self.above_blocks[n].dc_coeff);
                        count += 1;
                    }
                }
            }
            let plane = usize::from(B2P[b]);
            let rf = ref_frame.max(0) as usize;
            if count == 0 {
                dc = i32::from(self.prev_dc[plane][rf]);
            } else if count == 2 {
                dc /= 2;
            }
            let v = self.block_coeff[b][idx].wrapping_add(dc as i16);
            self.block_coeff[b][idx] = v;
            self.prev_dc[plane][rf] = v;
            self.above_blocks[ab].dc_coeff = v;
            self.above_blocks[ab].ref_frame = ref_frame;
            self.left_block[lb].dc_coeff = v;
            self.left_block[lb].ref_frame = ref_frame;
            self.block_coeff[b][idx] = v.wrapping_mul(self.dequant_dc as i16);
        }
    }

    /// vp56_deblock_filter on `buf` (an edge buffer) with its stride.
    fn deblock_filter(&self, buf: &mut [u8], stride: isize, dx: i32, dy: i32) {
        match self.flavor {
            Flavor::Vp5 => {
                let t = i32::from(FILTER_THRESHOLD[self.quantizer as usize & 63]);
                if dx != 0 {
                    dsp::vp5_edge_filter(buf, (10 - dx) as isize, 1, stride, t);
                }
                if dy != 0 {
                    dsp::vp5_edge_filter(buf, stride * (10 - dy) as isize, stride, 1, t);
                }
            }
            Flavor::Vp6 => {
                if dx != 0 {
                    dsp::vp3_loop_filter_12(buf, (10 - dx) as isize, 1, stride, &self.bounding_values);
                }
                if dy != 0 {
                    dsp::vp3_loop_filter_12(buf, stride * (10 - dy) as isize, stride, 1, &self.bounding_values);
                }
            }
        }
    }

    /// emulated_edge_mc into the edge buffer (rows `pitch` apart): the
    /// `bw` x `bh` block at (`x`, `y`) of `plane`, its edges replicated.
    fn emulated_edge(&mut self, reference: &Picture, plane: usize, pitch: isize, bw: i32, bh: i32, x: i32, y: i32) {
        let (w, h) = (self.plane_width[plane] as i32, self.plane_height[plane] as i32);
        if w == 0 || h == 0 {
            return;
        }
        let src = &reference.planes[plane];
        let ps = reference.stride[plane] as isize;
        for j in 0..bh {
            let sy = (y + j).clamp(0, h - 1) as isize;
            for i in 0..bw {
                let sx = (x + i).clamp(0, w - 1) as isize;
                dsp::set(&mut self.edge, j as isize * pitch + i as isize, dsp::get(src, sy * ps + sx) as u8);
            }
        }
    }

    /// vp6_filter.
    #[allow(clippy::too_many_arguments)]
    fn vp6_filter(&self, dst: &mut [u8], d: isize, src: &[u8], offset1: isize, offset2: isize, stride: isize, mv: Mv, mask: i32, select: i32, luma: bool) {
        let mut filter4 = 0;
        let mut x8 = i32::from(mv.x) & mask;
        let mut y8 = i32::from(mv.y) & mask;
        let mut offset1 = offset1;
        if luma {
            x8 *= 2;
            y8 *= 2;
            filter4 = self.filter_mode;
            if filter4 == 2 {
                let (ax, ay) = (i32::from(mv.x).abs(), i32::from(mv.y).abs());
                if self.max_vector_length != 0 && (ax > self.max_vector_length || ay > self.max_vector_length) {
                    filter4 = 0;
                } else if self.sample_variance_threshold != 0 && dsp::vp6_block_variance(src, offset1, stride) < self.sample_variance_threshold {
                    filter4 = 0;
                }
            }
        }
        // FFmpeg multiplies the offsets' difference by its flip sign; in
        // bitstream order that sign is always +1.
        if (y8 != 0 && offset2 - offset1 < 0) || (y8 == 0 && offset1 > offset2) {
            offset1 = offset2;
        }
        let select = select.clamp(0, 16) as usize;
        let weights = |v: i32| &VP6_BLOCK_COPY_FILTER[select][(v & 7) as usize];
        let diag = (i32::from(mv.x) ^ i32::from(mv.y)) >> 31;
        if filter4 != 0 {
            if y8 == 0 {
                dsp::vp6_filter_hv4(dst, d, src, offset1, stride, 1, weights(x8));
            } else if x8 == 0 {
                dsp::vp6_filter_hv4(dst, d, src, offset1, stride, stride, weights(y8));
            } else {
                dsp::vp6_filter_diag4(dst, d, src, offset1 + diag as isize, stride, weights(x8), weights(y8));
            }
        } else if x8 == 0 || y8 == 0 {
            dsp::h264_chroma_mc8(dst, d, src, offset1, stride, 8, x8, y8);
        } else {
            dsp::vp6_filter_diag2(dst, d, src, offset1 + diag as isize, stride, x8, y8);
        }
    }

    /// vp56_mc for block `b` of `plane` at (`x`, `y`) in the picture.
    #[allow(clippy::too_many_arguments)]
    fn mc(&mut self, cur: &mut Picture, reference: &Picture, b: usize, plane: usize, x: i32, y: i32, ref_stride: isize) {
        let stride = self.stride[plane];
        let dst_off = self.block_offset[b];
        let coord_div = i32::from(match self.flavor {
            Flavor::Vp5 => VP5_COORD_DIV[b],
            Flavor::Vp6 => VP6_COORD_DIV[b],
        });
        let mask = coord_div - 1;
        let deblock = self.deblock_filtering;
        let mv = self.mv[b];
        let dx = i32::from(mv.x) / coord_div;
        let dy = i32::from(mv.y) / coord_div;
        let (mut x, mut y) = if b >= 4 { (x / 2, y / 2) } else { (x, y) };
        x += dx - 2;
        y += dy - 2;
        let (pw, ph) = (self.plane_width[plane] as i32, self.plane_height[plane] as i32);
        // The source: the edge buffer or the reference plane itself.
        let mut edge = std::mem::take(&mut self.edge);
        let need = (32 * ref_stride.unsigned_abs().max(stride.unsigned_abs())) + 64;
        if edge.len() < need {
            edge.resize(need, 0);
        }
        self.edge = edge;
        let use_edge;
        let src_offset;
        if self.interlaced && self.il_block {
            // 12 x 24 rows of both fields; the block reads one field.
            self.emulated_edge(reference, plane, ref_stride, 12, 24, x, y - 2);
            use_edge = true;
            src_offset = 2 + 4 * ref_stride;
        } else if x < 0 || x + 12 >= pw || y < 0 || y + 12 >= ph {
            self.emulated_edge(reference, plane, stride, 12, 12, x, y);
            use_edge = true;
            src_offset = 2 + 2 * stride;
        } else if deblock {
            // FFmpeg copies 16 x 12 (only 12 x 12 is used).
            let src = &reference.planes[plane];
            let at = self.block_offset[b] + (dy as isize - 2) * stride + (dx as isize - 2);
            let mut edge = std::mem::take(&mut self.edge);
            dsp::copy(&mut edge, 0, stride, src, at, stride, 16, 12);
            self.edge = edge;
            use_edge = true;
            src_offset = 2 + 2 * stride;
        } else {
            use_edge = false;
            src_offset = self.block_offset[b] + dy as isize * stride + dx as isize;
        }
        if deblock && use_edge {
            let mut edge = std::mem::take(&mut self.edge);
            self.deblock_filter(&mut edge, stride, dx & 7, dy & 7);
            self.edge = edge;
        }
        let mut overlap: isize = 0;
        if i32::from(mv.x) & mask != 0 {
            overlap += if mv.x > 0 { 1 } else { -1 };
        }
        if i32::from(mv.y) & mask != 0 {
            overlap += if mv.y > 0 { stride } else { -stride };
        }
        let src: &[u8] = if use_edge { &self.edge } else { &reference.planes[plane] };
        let dst = &mut cur.planes[plane];
        if overlap != 0 {
            match self.flavor {
                Flavor::Vp6 => {
                    self.vp6_filter(dst, dst_off, src, src_offset, src_offset + overlap, stride, mv, mask, self.filter_selection, b < 4)
                }
                Flavor::Vp5 => dsp::put_no_rnd_pixels_l2(dst, dst_off, src, src_offset, src_offset + overlap, stride),
            }
        } else {
            dsp::copy(dst, dst_off, stride, src, src_offset, stride, 8, 8);
        }
    }

    /// vp56_render_mb.
    fn render_mb(&mut self, cur: &mut Picture, row: usize, col: usize, mb_type: u8) {
        let ref_frame = REFERENCE_FRAME[usize::from(mb_type).min(9)];
        self.add_predictors_dc(ref_frame);
        let reference = match ref_frame {
            FRAME_PREVIOUS => self.prev.clone(),
            FRAME_GOLDEN => self.golden.clone(),
            _ => None,
        };
        if mb_type != MB_INTRA && reference.is_none() {
            return;
        }
        let ref_stride = self.stride;
        let il = self.interlaced && self.il_block;
        if il {
            self.block_offset[2] -= self.stride[0] * 7;
            self.block_offset[3] -= self.stride[0] * 7;
            self.stride[0] *= 2;
        }
        let ab = if self.is_alpha { 6 } else { 0 };
        let b_max = if self.is_alpha { 4 } else { 6 };
        match mb_type {
            MB_INTRA => {
                for b in 0..b_max {
                    let plane = usize::from(B2P[b + ab]);
                    let mut block = self.block_coeff[b];
                    dsp::idct_put(&mut cur.planes[plane], self.block_offset[b], self.stride[plane], &mut block, self.idct_selector[b]);
                    self.block_coeff[b] = block;
                }
            }
            MB_INTER_NOVEC_PF | MB_INTER_NOVEC_GF => {
                let reference = reference.as_deref().expect("checked above");
                for b in 0..b_max {
                    let plane = usize::from(B2P[b + ab]);
                    let off = self.block_offset[b];
                    let stride = self.stride[plane];
                    dsp::copy(&mut cur.planes[plane], off, stride, &reference.planes[plane], off, stride, 8, 8);
                    let mut block = self.block_coeff[b];
                    dsp::idct_add(&mut cur.planes[plane], off, stride, &mut block, self.idct_selector[b]);
                    self.block_coeff[b] = block;
                }
            }
            _ => {
                let reference = reference.as_deref().expect("checked above");
                for b in 0..b_max {
                    let x_off = if b == 1 || b == 3 { 8 } else { 0 };
                    let y_off = if b == 2 || b == 3 { if il { 1 } else { 8 } } else { 0 };
                    let plane = usize::from(B2P[b + ab]);
                    self.mc(cur, reference, b, plane, 16 * col as i32 + x_off, 16 * row as i32 + y_off, ref_stride[plane]);
                    let mut block = self.block_coeff[b];
                    dsp::idct_add(&mut cur.planes[plane], self.block_offset[b], self.stride[plane], &mut block, self.idct_selector[b]);
                    self.block_coeff[b] = block;
                }
            }
        }
        if self.is_alpha {
            self.block_coeff[4][0] = 0;
            self.block_coeff[5][0] = 0;
        }
        if il {
            self.stride[0] /= 2;
            self.block_offset[2] += self.stride[0] * 7;
            self.block_offset[3] += self.stride[0] * 7;
        }
    }

    /// vp56_decode_mb.
    fn decode_mb(&mut self, cur: &mut Picture, row: usize, col: usize) -> Res<()> {
        if self.interlaced {
            let mut prob = self.il_prob;
            if col > 0 {
                if self.il_block {
                    prob -= prob >> 1;
                } else {
                    prob += (256 - prob) >> 1;
                }
            }
            self.il_block = self.c.get_prob(prob as u8);
        }
        let mb_type = if self.key { MB_INTRA } else { self.decode_mv(row, col) };
        self.parse_coeff()?;
        self.render_mb(cur, row, col, mb_type);
        Ok(())
    }

    /// vp56_conceal_mb.
    fn conceal_mb(&mut self, cur: &mut Picture, row: usize, col: usize) {
        let mb_type = if self.key { MB_INTRA } else { self.conceal_mv(row, col) };
        self.render_mb(cur, row, col, mb_type);
    }

    /// ff_vp56_decode_mbs into `cur`: whether FFmpeg goes on to keep the
    /// picture as its previous (and golden) frame.
    pub fn decode_mbs(&mut self, cur: &mut Picture) -> bool {
        if self.key {
            self.default_models_init();
            for mb in &mut self.macroblocks {
                mb.ty = MB_INTRA;
            }
        } else {
            self.parse_mb_type_models();
            self.parse_vector_models();
            self.mb_type = MB_INTER_NOVEC_PF;
        }
        if !self.parse_coeff_models() {
            return true;
        }
        if self.interlaced {
            self.il_prob = self.c.gets(8);
        }
        self.prev_dc = [[0; 3]; 3];
        self.prev_dc[1][FRAME_CURRENT as usize] = 128;
        self.prev_dc[2][FRAME_CURRENT as usize] = 128;
        for ab in &mut self.above_blocks {
            *ab = RefDc::default();
        }
        let mbw = self.mb_width;
        if let Some(ab) = self.above_blocks.get_mut(2 * mbw + 2) {
            ab.ref_frame = FRAME_CURRENT;
        }
        if let Some(ab) = self.above_blocks.get_mut(3 * mbw + 4) {
            ab.ref_frame = FRAME_CURRENT;
        }
        let (stride_y, stride_uv) = (self.stride[0], self.stride[1]);
        let mut damaged = false;
        for mb_row in 0..self.mb_height {
            self.left_block = [RefDc::default(); 4];
            self.coeff_ctx = [[0; 64]; 4];
            self.coeff_ctx_last = [24; 4];
            self.above_block_idx = [1, 2, 1, 2, 2 * mbw + 3, 3 * mbw + 5];
            let r = mb_row as isize;
            self.block_offset[0] = r * 16 * stride_y;
            self.block_offset[2] = self.block_offset[0] + 8 * stride_y;
            self.block_offset[1] = self.block_offset[0] + 8;
            self.block_offset[3] = self.block_offset[2] + 8;
            self.block_offset[4] = r * 8 * stride_uv;
            self.block_offset[5] = self.block_offset[4];
            for mb_col in 0..mbw {
                if !damaged && self.decode_mb(cur, mb_row, mb_col).is_err() {
                    damaged = true;
                    // FFmpeg conceals only after an undamaged frame
                    // (error_concealment is on by default).
                    if !self.have_undamaged_frame {
                        self.discard_frame = true;
                        return false;
                    }
                }
                if damaged {
                    self.conceal_mb(cur, mb_row, mb_col);
                }
                for y in 0..4 {
                    self.above_block_idx[y] += 2;
                    self.block_offset[y] += 16;
                }
                for uv in 4..6 {
                    self.above_block_idx[uv] += 1;
                    self.block_offset[uv] += 8;
                }
            }
        }
        if !damaged {
            self.have_undamaged_frame = true;
        }
        true
    }
}
