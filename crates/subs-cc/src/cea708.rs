// CEA-708 (DTVCC) closed caption decoder.
//
// Ported from VLC modules/codec/cea708.c and cea708.h (vlc-src 2e358f3,
// LGPL-2.1-or-later), driven the way modules/codec/cc.c (same revision,
// LGPL-2.1-or-later) drives it: each valid triplet is timed 1/400 s after
// the one before it in its packet, cc_type 2/3 triplets go to the DTVCC
// packet demuxer, and service 1 (the primary caption service) is decoded.

//! CEA-708 captions from A/53 `cc_data` triplets.
//!
//! [`Cea708`] reproduces VLC's decoder output: each [`Output`] is the
//! subpicture VLC queues (start, the 10 s stop, one [`Region`] per visible
//! window with its styled text and placement). VLC shows each one until the
//! next replaces it; the `cea_708` [`Decoder`] turns them into cues that
//! end where the next output starts.

use std::collections::VecDeque;

use oxideav_core::{
    CodecId, CodecParameters, CuePosition, Decoder, Error, Frame, Packet, Result, Segment, SubtitleCue, TextAlign,
};

use crate::eia608::ticks_to_us;

/// The codec id: FFmpeg's `AV_CODEC_ID_EIA_608` carries both services;
/// this decoder takes the same triplets and decodes CEA-708.
pub const CODEC_ID: &str = "cea_708";

const DTVCC_MAX_PKT_SIZE: usize = 128;
const SERVICE_INPUT_BUFFER: usize = 128;
const WINDOWS_COUNT: usize = 8;
const SCREEN_ROWS: f32 = 75.0;
const SCREEN_COLS_169: f32 = 210.0;
const SCREEN_SAFE_MARGIN_RATIO: f64 = 0.10;
const SAFE_AREA_REL: f64 = 1.0 - SCREEN_SAFE_MARGIN_RATIO;
const WINDOW_MAX_COLS: u8 = 42;
const WINDOW_MAX_ROWS: u8 = 15;
const ROW_HEIGHT_STANDARD: f64 = SAFE_AREA_REL / WINDOW_MAX_ROWS as f64;
const FONT_TO_LINE_HEIGHT_RATIO: f64 = 1.06;
const CENTER_ANCHOR_START: f32 = 0.25;
const CENTER_ANCHOR_RANGE: f32 = 0.5;
const FONTRELSIZE_STANDARD: f64 = 100.0 * ROW_HEIGHT_STANDARD / FONT_TO_LINE_HEIGHT_RATIO;
const FONTRELSIZE_SMALL: f64 = FONTRELSIZE_STANDARD * 0.7;
const FONTRELSIZE_LARGE: f64 = FONTRELSIZE_STANDARD * 1.3;

/// VLC ticks are microseconds (CLOCK_FREQ 1000000).
const CLOCK_FREQ: i64 = 1_000_000;
const TICK_INVALID: i64 = 0;
const fn tick_from_samples(samples: i64, rate: i64) -> i64 {
    CLOCK_FREQ * samples / rate
}

const STATUS_OK: u8 = 1 << 0;
const STATUS_STARVING: u8 = 1 << 1;
const STATUS_OUTPUT: u8 = 1 << 2;

const OPACITY_SOLID: u8 = 0;
const OPACITY_FLASH: u8 = 1;
const OPACITY_TRANSLUCENT: u8 = 2;
const OPACITY_TRANSPARENT: u8 = 3;

const EDGE_NONE: u8 = 0;
const EDGE_UNIFORM: u8 = 3;

const DIRECTION_LTR: u8 = 0;
const DIRECTION_RTL: u8 = 1;
const DIRECTION_TB: u8 = 2;
const DIRECTION_BT: u8 = 3;

const JUSTIFY_LEFT: u8 = 0;
const JUSTIFY_CENTER: u8 = 2;

// vlc_text_style.h
/// `STYLE_ALPHA_OPAQUE`
pub const STYLE_ALPHA_OPAQUE: u8 = 0xff;
/// `STYLE_ALPHA_TRANSPARENT`
pub const STYLE_ALPHA_TRANSPARENT: u8 = 0x00;
/// `STYLE_HAS_FONT_COLOR`
pub const STYLE_HAS_FONT_COLOR: u32 = 1 << 0;
/// `STYLE_HAS_FONT_ALPHA`
pub const STYLE_HAS_FONT_ALPHA: u32 = 1 << 1;
/// `STYLE_HAS_BACKGROUND_COLOR`
pub const STYLE_HAS_BACKGROUND_COLOR: u32 = 1 << 7;
/// `STYLE_HAS_BACKGROUND_ALPHA`
pub const STYLE_HAS_BACKGROUND_ALPHA: u32 = 1 << 8;
/// `STYLE_ITALIC`
pub const STYLE_ITALIC: u32 = 1 << 1;
/// `STYLE_BACKGROUND`
pub const STYLE_BACKGROUND: u32 = 1 << 4;
/// `STYLE_UNDERLINE`
pub const STYLE_UNDERLINE: u32 = 1 << 5;
/// `STYLE_MONOSPACED`
pub const STYLE_MONOSPACED: u32 = 1 << 8;
/// `STYLE_BLINK_FOREGROUND`
pub const STYLE_BLINK_FOREGROUND: u32 = 1 << 10;
/// `STYLE_BLINK_BACKGROUND`
pub const STYLE_BLINK_BACKGROUND: u32 = 1 << 11;

// vlc_subpicture.h
/// `SUBPICTURE_ALIGN_LEFT`
pub const ALIGN_LEFT: i32 = 0x1;
/// `SUBPICTURE_ALIGN_RIGHT`
pub const ALIGN_RIGHT: i32 = 0x2;
/// `SUBPICTURE_ALIGN_TOP`
pub const ALIGN_TOP: i32 = 0x4;
/// `SUBPICTURE_ALIGN_BOTTOM`
pub const ALIGN_BOTTOM: i32 = 0x8;

// substext.h
/// `UPDT_REGION_ORIGIN_X_IS_RATIO`
pub const REGION_ORIGIN_X_IS_RATIO: u32 = 1 << 0;
/// `UPDT_REGION_ORIGIN_Y_IS_RATIO`
pub const REGION_ORIGIN_Y_IS_RATIO: u32 = 1 << 1;
/// `UPDT_REGION_USES_GRID_COORDINATES`
pub const REGION_USES_GRID_COORDINATES: u32 = 1 << 5;
/// `UPDT_REGION_USES_16_9_GRID`
pub const REGION_USES_16_9_GRID: u32 = 1 << 6;

// ---- DTVCC packet demuxing (CEA708_DTVCC_Demuxer_*) ------------------

/// Reassembles DTVCC packets from cc_type 2/3 triplets and splits them
/// into service blocks.
#[derive(Clone, Debug)]
struct DtvccDemuxer {
    pkt_sequence: i8,
    total_data: u8,
    data_len: u8,
    data: [u8; DTVCC_MAX_PKT_SIZE],
    time: i64,
}

impl DtvccDemuxer {
    fn new() -> Self {
        Self { pkt_sequence: -1, total_data: 0, data_len: 0, data: [0; DTVCC_MAX_PKT_SIZE], time: 0 }
    }

    fn flush(&mut self) {
        self.pkt_sequence = -1;
        self.total_data = 0;
        self.data_len = 0;
    }

    fn put(&mut self, byte: u8) {
        if let Some(slot) = self.data.get_mut(usize::from(self.data_len)) {
            *slot = byte;
        }
        self.data_len = self.data_len.wrapping_add(1);
    }

    /// One triplet; the service blocks of each packet it completes go to
    /// `blocks` as (service, time, data).
    fn push(&mut self, start: i64, data: [u8; 3], blocks: &mut Vec<(u8, i64, Vec<u8>)>) {
        if data[0] & 0x03 == 3 {
            // Packet header.
            let pkt_sequence = (data[1] >> 6) as i8;
            // Loss or discontinuity: drop the buffer.
            if pkt_sequence > 0 && (self.pkt_sequence.wrapping_add(1)).rem_euclid(4) != pkt_sequence {
                self.data_len = 0;
                self.total_data = 0;
                self.pkt_sequence = pkt_sequence;
                return;
            }
            let pktsize = data[1] & 63;
            let pktsize = if pktsize == 0 { 127 } else { pktsize * 2 - 1 };
            self.pkt_sequence = pkt_sequence;
            self.total_data = pktsize;
            self.data_len = 0;
            self.time = start;
            self.put(data[2]);
        } else if self.total_data > 0 {
            self.put(data[1]);
            self.put(data[2]);
        }
        if self.data_len > 0 && self.data_len >= self.total_data {
            if self.data_len == self.total_data {
                let len = usize::from(self.data_len).min(DTVCC_MAX_PKT_SIZE);
                service_blocks(self.time, &self.data[..len], blocks);
            }
            self.total_data = 0;
            self.data_len = 0;
        }
    }
}

/// CEA708_DTVCC_Demux_ServiceBlocks.
fn service_blocks(start: i64, mut data: &[u8], blocks: &mut Vec<(u8, i64, Vec<u8>)>) {
    while data.len() >= 2 {
        let mut sid = data[0] >> 5;
        let block_size = usize::from(data[0] & 0x1f);
        if sid == 0x07 {
            sid = data[1] & 0x3f;
            if sid < 0x07 {
                return;
            }
            data = &data[1..];
        }
        if block_size == 0 || block_size > data.len() - 1 {
            return;
        }
        data = &data[1..];
        blocks.push((sid, start, data[..block_size].to_vec()));
        data = &data[block_size..];
    }
}

// ---- Service decoding state -----------------------------------------

/// cea708_pen_style_t
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PenStyle {
    size: u8,
    font: u8,
    text_tag: u8,
    offset: u8,
    italics: bool,
    underline: bool,
    fg_color: u8,
    fg_opacity: u8,
    bg_color: u8,
    bg_opacity: u8,
    edge_color: u8,
    edge_type: u8,
}

const fn default_ntsc_style(font: u8, edge: u8, bg_opacity: u8) -> PenStyle {
    PenStyle {
        size: 1, // standard
        font,
        text_tag: 0,
        offset: 1, // normal
        italics: false,
        underline: false,
        fg_color: 0x2a,
        fg_opacity: OPACITY_SOLID,
        bg_color: 0x00,
        bg_opacity,
        edge_color: 0x00,
        edge_type: edge,
    }
}

const DEFAULT_PEN_STYLES: [PenStyle; 7] = [
    default_ntsc_style(0, EDGE_NONE, OPACITY_SOLID),
    default_ntsc_style(1, EDGE_NONE, OPACITY_SOLID),
    default_ntsc_style(2, EDGE_NONE, OPACITY_SOLID),
    default_ntsc_style(3, EDGE_NONE, OPACITY_SOLID),
    default_ntsc_style(4, EDGE_NONE, OPACITY_SOLID),
    default_ntsc_style(3, EDGE_UNIFORM, OPACITY_TRANSPARENT),
    default_ntsc_style(4, EDGE_UNIFORM, OPACITY_TRANSPARENT),
];

/// cea708_window_style_t
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WindowStyle {
    justify: u8,
    print_direction: u8,
    scroll_direction: u8,
    effect_direction: u8,
    word_wrap: bool,
    display_effect: u8,
    effect_speed: u8,
    fill_color: u8,
    fill_opacity: u8,
    border_type: u8,
    border_color: u8,
}

const fn default_ntsc_wa_style(justify: u8, print: u8, scroll: u8, wrap: bool, opacity: u8) -> WindowStyle {
    WindowStyle {
        justify,
        print_direction: print,
        scroll_direction: scroll,
        effect_direction: DIRECTION_LTR,
        word_wrap: wrap,
        display_effect: 0,
        effect_speed: 1,
        fill_color: 0x00,
        fill_opacity: opacity,
        border_type: EDGE_NONE,
        border_color: 0x00,
    }
}

const DEFAULT_WINDOW_STYLES: [WindowStyle; 7] = [
    default_ntsc_wa_style(JUSTIFY_LEFT, DIRECTION_LTR, DIRECTION_BT, false, OPACITY_SOLID),
    default_ntsc_wa_style(JUSTIFY_LEFT, DIRECTION_LTR, DIRECTION_BT, false, OPACITY_TRANSPARENT),
    default_ntsc_wa_style(JUSTIFY_CENTER, DIRECTION_LTR, DIRECTION_BT, false, OPACITY_SOLID),
    default_ntsc_wa_style(JUSTIFY_LEFT, DIRECTION_LTR, DIRECTION_BT, true, OPACITY_SOLID),
    default_ntsc_wa_style(JUSTIFY_LEFT, DIRECTION_LTR, DIRECTION_BT, true, OPACITY_TRANSPARENT),
    default_ntsc_wa_style(JUSTIFY_CENTER, DIRECTION_LTR, DIRECTION_BT, true, OPACITY_SOLID),
    default_ntsc_wa_style(JUSTIFY_LEFT, DIRECTION_TB, DIRECTION_RTL, false, OPACITY_SOLID),
];

/// cea708_text_row_t
#[derive(Clone, Debug)]
struct TextRow {
    characters: [u8; WINDOW_MAX_COLS as usize * 4],
    styles: [PenStyle; WINDOW_MAX_COLS as usize],
    firstcol: u8,
    lastcol: u8,
}

impl TextRow {
    fn new() -> Box<Self> {
        Box::new(Self {
            characters: [0; WINDOW_MAX_COLS as usize * 4],
            styles: [DEFAULT_PEN_STYLES[0]; WINDOW_MAX_COLS as usize],
            firstcol: WINDOW_MAX_COLS,
            lastcol: 0,
        })
    }
}

/// cea708_window_t
#[derive(Clone, Debug)]
struct Window {
    rows: [Option<Box<TextRow>>; WINDOW_MAX_ROWS as usize],
    firstrow: u8,
    lastrow: u8,
    priority: u8,
    anchor_point: u8,
    anchor_offset_v: u8,
    anchor_offset_h: u8,
    row_count: u8,
    col_count: u8,
    relative: bool,
    row_lock: bool,
    column_lock: bool,
    visible: bool,
    style: WindowStyle,
    pen: PenStyle,
    row: u8,
    col: u8,
    defined: bool,
}

impl Window {
    /// CEA708_Window_Init
    fn new() -> Self {
        Self {
            rows: Default::default(),
            firstrow: WINDOW_MAX_ROWS,
            lastrow: 0,
            priority: 0,
            anchor_point: 0,
            anchor_offset_v: 0,
            anchor_offset_h: 0,
            row_count: 0,
            col_count: 0,
            relative: false,
            row_lock: true,
            column_lock: true,
            visible: false,
            style: DEFAULT_WINDOW_STYLES[0],
            pen: DEFAULT_PEN_STYLES[0],
            row: 0,
            col: 0,
            defined: false,
        }
    }

    fn row_at(&self, i: usize) -> Option<&TextRow> {
        self.rows.get(i).and_then(|r| r.as_deref())
    }

    fn row_slot(&mut self, i: usize) -> Option<&mut Option<Box<TextRow>>> {
        self.rows.get_mut(i)
    }

    /// CEA708_Window_ClearText
    fn clear_text(&mut self) {
        let mut i = self.firstrow;
        while i <= self.lastrow {
            if let Some(slot) = self.row_slot(usize::from(i)) {
                *slot = None;
            }
            match i.checked_add(1) {
                Some(next) => i = next,
                None => break,
            }
        }
        self.lastrow = 0;
        self.firstrow = WINDOW_MAX_ROWS;
    }

    /// CEA708_Window_Reset
    fn reset(&mut self) {
        self.clear_text();
        *self = Window::new();
    }

    fn rows_range(&self) -> std::ops::RangeInclusive<usize> {
        usize::from(self.firstrow)..=usize::from(self.lastrow)
    }

    fn min_col(&self) -> u8 {
        let mut min = WINDOW_MAX_COLS;
        for i in self.rows_range() {
            if let Some(row) = self.row_at(i) {
                if row.firstcol < min {
                    min = row.firstcol;
                }
            }
        }
        min
    }

    fn max_col(&self) -> u8 {
        let mut max = 0;
        for i in self.rows_range() {
            if let Some(row) = self.row_at(i) {
                if row.lastcol > max {
                    max = row.lastcol;
                }
            }
        }
        max
    }

    fn col_count_of_row(&self) -> u8 {
        match self.row_at(usize::from(self.row)) {
            Some(row) if row.firstcol <= row.lastcol => 1 + row.lastcol - row.firstcol,
            _ => 0,
        }
    }

    fn row_count_used(&self) -> u8 {
        if self.firstrow > self.lastrow {
            return 0;
        }
        (1u16 + u16::from(self.lastrow) - u16::from(self.firstrow)) as u8
    }

    /// CEA708_Window_Truncate
    fn truncate(&mut self, direction: u8) {
        match direction {
            DIRECTION_LTR => {
                let max = self.max_col();
                for i in self.rows_range() {
                    let (first, last) = match self.row_at(i) {
                        Some(row) => (row.firstcol, row.lastcol),
                        None => continue,
                    };
                    if last == max {
                        if first >= last {
                            self.rows[i] = None;
                            if i == usize::from(self.firstrow) {
                                self.firstrow = self.firstrow.wrapping_add(1);
                            } else if i == usize::from(self.lastrow) {
                                self.lastrow = self.lastrow.wrapping_sub(1);
                            }
                        } else if let Some(row) = self.rows[i].as_deref_mut() {
                            row.lastcol -= 1;
                        }
                    }
                }
            }
            DIRECTION_RTL => {
                let min = self.min_col();
                for i in self.rows_range() {
                    let (first, last) = match self.row_at(i) {
                        Some(row) => (row.firstcol, row.lastcol),
                        None => continue,
                    };
                    if first == min {
                        if first >= last {
                            self.rows[i] = None;
                            if i == usize::from(self.firstrow) {
                                self.firstrow = self.firstrow.wrapping_add(1);
                            } else if i == usize::from(self.lastrow) {
                                self.lastrow = self.lastrow.wrapping_sub(1);
                            }
                        } else if let Some(row) = self.rows[i].as_deref_mut() {
                            row.firstcol += 1;
                        }
                    }
                }
            }
            DIRECTION_TB => {
                if self.row_count_used() > 0 {
                    if let Some(slot) = self.row_slot(usize::from(self.lastrow)) {
                        *slot = None;
                    }
                    self.lastrow = self.lastrow.wrapping_sub(1);
                }
            }
            DIRECTION_BT => {
                if self.row_count_used() > 0 {
                    if let Some(slot) = self.row_slot(usize::from(self.firstrow)) {
                        *slot = None;
                    }
                    self.firstrow = self.firstrow.wrapping_add(1);
                }
            }
            _ => {}
        }
    }

    /// CEA708_Window_Scroll
    fn scroll(&mut self) {
        if self.row_count_used() == 0 {
            return;
        }
        match self.style.scroll_direction {
            DIRECTION_LTR => {
                // Move right.
                if self.max_col() == WINDOW_MAX_COLS - 1 {
                    self.truncate(DIRECTION_LTR);
                }
                for i in self.rows_range() {
                    let Some(row) = self.rows.get_mut(i).and_then(|r| r.as_deref_mut()) else { continue };
                    if row.lastcol < row.firstcol {
                        continue;
                    }
                    let (first, last) = (usize::from(row.firstcol), usize::from(row.lastcol));
                    if (last + 2) * 4 > row.characters.len() {
                        continue;
                    }
                    row.characters.copy_within(first * 4..(last + 1) * 4, first * 4 + 4);
                    row.styles.copy_within(first..last + 1, first + 1);
                    row.firstcol += 1;
                    row.lastcol += 1;
                }
            }
            DIRECTION_RTL => {
                // Move left.
                if self.min_col() == 0 {
                    self.truncate(DIRECTION_RTL);
                }
                for i in self.rows_range() {
                    let Some(row) = self.rows.get_mut(i).and_then(|r| r.as_deref_mut()) else { continue };
                    if row.lastcol < row.firstcol {
                        continue;
                    }
                    if row.firstcol > 0 {
                        let (first, last) = (usize::from(row.firstcol), usize::from(row.lastcol));
                        if (last + 1) * 4 > row.characters.len() {
                            continue;
                        }
                        row.characters.copy_within(first * 4..(last + 1) * 4, first * 4 - 4);
                        row.styles.copy_within(first..last + 1, first - 1);
                        row.firstcol -= 1;
                        row.lastcol -= 1;
                    }
                }
            }
            DIRECTION_TB => {
                // Move down.
                if self.lastrow == WINDOW_MAX_ROWS - 1 {
                    self.truncate(DIRECTION_TB);
                }
                let mut i = i32::from(self.lastrow);
                while i >= i32::from(self.firstrow) {
                    let moved = self.rows.get_mut(i as usize).and_then(|r| r.take());
                    if let Some(slot) = self.row_slot(i as usize + 1) {
                        *slot = moved;
                    }
                    i -= 1;
                }
                if let Some(slot) = self.row_slot(usize::from(self.firstrow)) {
                    *slot = None;
                }
                self.firstrow = self.firstrow.wrapping_add(1);
                self.lastrow = self.lastrow.wrapping_add(1);
            }
            DIRECTION_BT => {
                // Move up.
                if self.firstrow == 0 {
                    self.truncate(DIRECTION_BT);
                }
                for i in self.rows_range() {
                    if i == 0 {
                        continue;
                    }
                    let moved = self.rows.get_mut(i).and_then(|r| r.take());
                    if let Some(slot) = self.row_slot(i - 1) {
                        *slot = moved;
                    }
                }
                if let Some(slot) = self.row_slot(usize::from(self.lastrow)) {
                    *slot = None;
                }
                if self.firstrow != 0 {
                    self.firstrow -= 1;
                }
                if self.lastrow != 0 {
                    self.lastrow -= 1;
                }
            }
            _ => {}
        }
    }

    /// CEA708_Window_CarriageReturn
    fn carriage_return(&mut self) {
        match self.style.scroll_direction {
            DIRECTION_LTR => {
                if self.col > 0 && self.col_count_of_row() < self.col_count {
                    self.col -= 1;
                } else {
                    self.scroll();
                }
                self.row = if self.style.print_direction == DIRECTION_TB { 0 } else { WINDOW_MAX_ROWS - 1 };
            }
            DIRECTION_RTL => {
                if self.col + 1 < WINDOW_MAX_COLS && self.col_count_of_row() < self.col_count {
                    self.col += 1;
                } else {
                    self.scroll();
                }
                self.row = if self.style.print_direction == DIRECTION_TB { 0 } else { WINDOW_MAX_ROWS - 1 };
            }
            DIRECTION_TB => {
                if self.row > 0 && self.row_count_used() < self.row_count {
                    self.row -= 1;
                } else {
                    self.scroll();
                }
                self.col = if self.style.print_direction == DIRECTION_LTR { 0 } else { WINDOW_MAX_COLS - 1 };
            }
            DIRECTION_BT => {
                if u16::from(self.row) + 1 < u16::from(self.row_count) {
                    self.row += 1;
                } else {
                    self.scroll();
                }
                self.col = if self.style.print_direction == DIRECTION_LTR { 0 } else { WINDOW_MAX_COLS - 1 };
            }
            _ => {}
        }
    }

    /// CEA708_Window_Forward
    fn forward(&mut self) {
        match self.style.print_direction {
            DIRECTION_LTR => {
                if self.col + 1 < WINDOW_MAX_COLS {
                    self.col += 1;
                } else {
                    self.carriage_return();
                }
            }
            DIRECTION_RTL => {
                if self.col > 0 {
                    self.col -= 1;
                } else {
                    self.carriage_return();
                }
            }
            DIRECTION_TB => {
                if self.row + 1 < WINDOW_MAX_ROWS {
                    self.row += 1;
                } else {
                    self.carriage_return();
                }
            }
            DIRECTION_BT => {
                if self.row > 0 {
                    self.row -= 1;
                } else {
                    self.carriage_return();
                }
            }
            _ => {}
        }
    }

    /// CEA708_Window_Backward
    fn backward(&mut self) {
        const REVERSE: [u8; 4] = [DIRECTION_RTL, DIRECTION_LTR, DIRECTION_BT, DIRECTION_TB];
        let save = self.style.print_direction;
        self.style.print_direction = REVERSE[usize::from(save & 3)];
        self.forward();
        self.style.print_direction = save;
    }

    /// CEA708_Window_Write
    fn write(&mut self, c: [u8; 4]) {
        if !self.defined {
            return;
        }
        if self.row >= WINDOW_MAX_ROWS || self.col >= WINDOW_MAX_COLS {
            return;
        }
        let (row_i, col) = (usize::from(self.row), usize::from(self.col));
        if self.rows[row_i].is_none() {
            self.rows[row_i] = Some(TextRow::new());
            if self.row < self.firstrow {
                self.firstrow = self.row;
            }
            if self.row > self.lastrow {
                self.lastrow = self.row;
            }
        }
        let pen = self.pen;
        if let Some(row) = self.rows[row_i].as_deref_mut() {
            row.characters[col * 4..col * 4 + 4].copy_from_slice(&c);
            row.styles[col] = pen;
            if self.col < row.firstcol {
                row.firstcol = self.col;
            }
            if self.col > row.lastcol {
                row.lastcol = self.col;
            }
        }
        self.forward();
    }
}

/// The input ring buffer (cea708_input_buffer_t), with the C uint8_t
/// arithmetic.
#[derive(Clone, Debug)]
struct InputBuffer {
    ring: [u8; SERVICE_INPUT_BUFFER],
    start: u8,
    capacity: u8,
}

impl InputBuffer {
    fn new() -> Self {
        Self { ring: [0; SERVICE_INPUT_BUFFER], start: 0, capacity: 0 }
    }
    fn size(&self) -> u8 {
        self.capacity
    }
    fn remain(&self) -> u8 {
        (SERVICE_INPUT_BUFFER as u8).wrapping_sub(self.capacity)
    }
    fn add(&mut self, a: u8) {
        if self.remain() > 0 {
            let at = (usize::from(self.start) + usize::from(self.capacity)) % SERVICE_INPUT_BUFFER;
            self.ring[at] = a;
            self.capacity = self.capacity.wrapping_add(1);
        }
    }
    fn peek(&self, off: u8) -> u8 {
        if u16::from(off) + 1 > u16::from(self.capacity) {
            return 0;
        }
        self.ring[(usize::from(self.start) + usize::from(off)) % SERVICE_INPUT_BUFFER]
    }
    fn get(&mut self) -> u8 {
        let a = self.peek(0);
        self.start = ((usize::from(self.start) + 1) % SERVICE_INPUT_BUFFER) as u8;
        self.capacity = self.capacity.wrapping_sub(1);
        a
    }
    fn pop(&mut self, n: usize) {
        for _ in 0..n {
            self.get();
        }
    }
}

// ---- Output ----------------------------------------------------------

/// A text style (`text_style_t`, the fields the decoder sets).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TextStyle {
    pub style_flags: u32,
    pub features: u32,
    pub font_color: u32,
    pub font_alpha: u8,
    pub background_color: u32,
    pub background_alpha: u8,
    pub font_relsize: f32,
}

/// A run of text in one style (`text_segment_t`). The bytes are what VLC
/// builds: UTF-8 as the stream encodes it, `\n` ending each row but a
/// window's last.
#[derive(Clone, Debug, PartialEq)]
pub struct StyledText {
    pub text: Vec<u8>,
    pub style: TextStyle,
}

/// One window's text and placement (`substext_updater_region_t`).
#[derive(Clone, Debug, PartialEq)]
pub struct Region {
    pub origin_x: f32,
    pub origin_y: f32,
    pub flags: u32,
    pub align: i32,
    pub inner_align: i32,
    pub segments: Vec<StyledText>,
}

impl Region {
    /// SubpictureUpdaterSysRegionInit
    fn new() -> Self {
        Self { origin_x: 0.0, origin_y: 0.0, flags: 0, align: ALIGN_BOTTOM, inner_align: 0, segments: Vec::new() }
    }
}

/// One subpicture VLC queues: shown from `start` until the next output
/// replaces it, `stop` (10 s on) at the latest. Times in microseconds.
#[derive(Clone, Debug, PartialEq)]
pub struct Output {
    pub start: i64,
    pub stop: i64,
    pub regions: Vec<Region>,
}

// ---- The decoder -------------------------------------------------------

/// The CEA-708 service decoder (cea708_t) with the DTVCC demuxer and the
/// triplet timing of cc.c.
#[derive(Clone, Debug)]
pub struct Cea708 {
    service: u8,
    demux: DtvccDemuxer,
    windows: [Window; WINDOWS_COUNT],
    input: InputBuffer,
    cw: usize,
    suspended_deadline: i64,
    clock: i64,
    text_waiting: bool,
    outputs: Vec<Output>,
}

impl Default for Cea708 {
    fn default() -> Self {
        Self::new(1)
    }
}

impl Cea708 {
    /// A decoder for caption `service` (1..=63; 1 is the primary).
    pub fn new(service: u8) -> Self {
        Self {
            service,
            demux: DtvccDemuxer::new(),
            windows: std::array::from_fn(|_| Window::new()),
            input: InputBuffer::new(),
            cw: 0,
            suspended_deadline: TICK_INVALID,
            clock: 0,
            text_waiting: false,
            outputs: Vec::new(),
        }
    }

    /// Flush (cc.c Flush): the DTVCC demuxer and the decoder start over.
    pub fn flush(&mut self) {
        self.demux.flush();
        for window in &mut self.windows {
            window.reset();
        }
        self.input = InputBuffer::new();
        self.cw = 0;
        self.suspended_deadline = TICK_INVALID;
        self.text_waiting = false;
        self.clock = 0;
    }

    /// One packet of triplets at `pts_us` (cc.c Convert): the outputs it
    /// queues.
    pub fn decode(&mut self, data: &[u8], pts_us: i64) -> Vec<Output> {
        let mut ticks = 0i64;
        for triplet in data.chunks_exact(3) {
            if triplet[0] & 0x04 != 0 {
                let spu_pts = pts_us.wrapping_add(tick_from_samples(ticks, 1200 / 3));
                if triplet[0] & 0x03 >= 2 {
                    let mut blocks = Vec::new();
                    self.demux.push(spu_pts, [triplet[0], triplet[1], triplet[2]], &mut blocks);
                    for (sid, time, block) in blocks {
                        if sid == self.service {
                            self.push(time, &block);
                        }
                    }
                }
            }
            ticks += 1;
        }
        std::mem::take(&mut self.outputs)
    }

    /// CEA708_Decoder_Push
    fn push(&mut self, time: i64, data: &[u8]) {
        self.clock = time;
        let mut i = 0usize;
        while i < data.len() {
            let mut push = usize::from(self.input.remain());
            if data.len() - i < push {
                push = data.len() - i;
            } else {
                // A full buffer cancels a pause.
                self.suspended_deadline = TICK_INVALID;
            }
            for &byte in &data[i..i + push] {
                self.input.add(byte);
            }
            if self.suspended_deadline != TICK_INVALID {
                if self.suspended_deadline > self.clock {
                    // Paused: the clock moves on, the same bytes are pushed
                    // again on the next turn (as VLC's loop does).
                    if push > 0 {
                        self.clock = self.clock.wrapping_add(tick_from_samples(1, 1200) * push as i64);
                    }
                    continue;
                }
                self.suspended_deadline = TICK_INVALID;
            }
            let before = self.input.size();
            self.decode_service_buffer();
            if push == 0 && self.input.size() == before {
                // A full buffer that decodes nothing would never move.
                break;
            }
            i += push;
        }
    }

    /// CEA708_Decode_ServiceBuffer
    fn decode_service_buffer(&mut self) {
        loop {
            let input = self.input.size();
            if input == 0 {
                break;
            }
            let c = self.input.peek(0);
            let ret = if c < 0x20 {
                self.decode_c0(c)
            } else if c <= 0x7f {
                self.decode_g0(c)
            } else if c <= 0x9f {
                self.decode_c1(c)
            } else {
                self.decode_g1(c)
            };
            if ret & STATUS_OUTPUT != 0 {
                let output = self.build_subtitle();
                self.outputs.push(output);
            }
            if ret & STATUS_STARVING != 0 {
                break;
            }
            let consumed = input.wrapping_sub(self.input.size());
            if consumed != 0 {
                self.clock = self.clock.wrapping_add(tick_from_samples(1, 9600) * i64::from(consumed));
            }
        }
    }

    fn cw(&mut self) -> &mut Window {
        &mut self.windows[self.cw]
    }

    fn decode_c0(&mut self, code: u8) -> u8 {
        let mut ret = STATUS_OK;
        match code {
            0x00 => self.input.pop(1),
            0x03 => {
                // ETX
                self.input.pop(1);
                if self.text_waiting {
                    ret |= STATUS_OUTPUT;
                    self.text_waiting = false;
                }
            }
            0x08 => {
                // BS
                self.input.pop(1);
                if self.cw().defined {
                    self.cw().backward();
                    self.text_waiting = true;
                }
            }
            0x0c => {
                // FF
                self.input.pop(1);
                if self.cw().defined {
                    let w = self.cw();
                    w.clear_text();
                    w.col = 0;
                    w.row = 0;
                    self.text_waiting = true;
                }
            }
            0x0d => {
                // CR
                self.input.pop(1);
                if self.cw().defined && self.cw().style.print_direction <= DIRECTION_RTL {
                    self.cw().carriage_return();
                    if self.cw().visible {
                        ret |= STATUS_OUTPUT;
                    }
                }
            }
            0x0e => {
                // HCR
                self.input.pop(1);
                if self.cw().defined && self.cw().style.print_direction > DIRECTION_RTL {
                    self.cw().carriage_return();
                    if self.cw().visible {
                        ret |= STATUS_OUTPUT;
                    }
                }
            }
            0x10 => {
                // EXT1
                if self.input.size() >= 2 {
                    let v = self.input.peek(1);
                    if v < 0x20 {
                        // C2 extended code set
                        let i = if v > 0x17 {
                            3
                        } else if v > 0x0f {
                            2
                        } else if v > 0x07 {
                            1
                        } else {
                            0
                        };
                        if usize::from(self.input.size()) < 2 + i {
                            return STATUS_STARVING;
                        }
                        self.input.pop(1);
                        self.input.pop(1 + i);
                    } else if v > 0x7f && v < 0xa0 {
                        // C3 extended code set
                        let i = if v > 0x87 { 5 } else { 4 };
                        if usize::from(self.input.size()) < 2 + i {
                            return STATUS_STARVING;
                        }
                        self.input.pop(1);
                        self.input.pop(1 + i);
                    } else {
                        self.input.pop(1);
                        let v = self.input.get();
                        if self.cw().defined {
                            ret |= self.decode_g2g3(v);
                        }
                    }
                } else {
                    return STATUS_STARVING;
                }
            }
            0x18 => {
                // P16
                if self.input.size() < 3 {
                    return STATUS_STARVING;
                }
                self.input.pop(1);
                let mut u16v = u16::from(self.input.get()) << 8;
                u16v |= u16::from(self.input.get());
                ret |= self.decode_p16(u16v);
            }
            _ => self.input.pop(1),
        }
        ret
    }

    fn decode_g0(&mut self, code: u8) -> u8 {
        self.input.pop(1);
        let mut ret = STATUS_OK;
        if !self.cw().defined {
            return ret;
        }
        let mut utf8 = [code, 0, 0, 0];
        if code == 0x7f {
            // Music note
            utf8 = [0xe2, 0x99, 0xaa, 0];
        }
        self.cw().write(utf8);
        // CEA708_Window_BreaksSpace is always true in VLC.
        if code == 0x20 && self.text_waiting {
            ret |= STATUS_OUTPUT;
        }
        self.text_waiting |= self.cw().visible;
        ret
    }

    /// The bit-mapped window commands (CLW, DSW, HDW, TGW, DLW): `apply`
    /// gets each selected window and returns whether to output.
    fn for_windows(&mut self, mut apply: impl FnMut(&mut Window) -> bool) -> u8 {
        let mut ret = 0;
        let mut v = self.input.get();
        let mut i = 0usize;
        while v != 0 {
            if v & 1 != 0 && apply(&mut self.windows[i]) {
                ret |= STATUS_OUTPUT;
            }
            v >>= 1;
            i += 1;
        }
        ret
    }

    fn decode_c1(&mut self, code: u8) -> u8 {
        let mut ret = STATUS_OK;
        if self.text_waiting {
            ret |= STATUS_OUTPUT;
            self.text_waiting = false;
        }
        // REQUIRE_ARGS: the command waits for `n` argument bytes.
        macro_rules! require_and_pop {
            ($n:expr) => {
                if usize::from(self.input.size()) < $n + 1 {
                    return STATUS_STARVING;
                }
                self.input.pop(1);
            };
        }
        match code {
            0x88 => {
                // CLW
                require_and_pop!(1);
                ret |= self.for_windows(|w| {
                    let out = w.defined && w.visible;
                    w.clear_text();
                    out
                });
            }
            0x89 => {
                // DSW
                require_and_pop!(1);
                ret |= self.for_windows(|w| {
                    if w.defined {
                        let out = !w.visible;
                        w.visible = true;
                        out
                    } else {
                        false
                    }
                });
            }
            0x8a => {
                // HDW
                require_and_pop!(1);
                ret |= self.for_windows(|w| {
                    if w.defined {
                        let out = w.visible;
                        w.visible = false;
                        out
                    } else {
                        false
                    }
                });
            }
            0x8b => {
                // TGW
                require_and_pop!(1);
                ret |= self.for_windows(|w| {
                    if w.defined {
                        w.visible = !w.visible;
                        true
                    } else {
                        false
                    }
                });
            }
            0x8c => {
                // DLW
                require_and_pop!(1);
                ret |= self.for_windows(|w| {
                    if w.defined {
                        let out = w.visible;
                        w.reset();
                        out
                    } else {
                        false
                    }
                });
            }
            0x8d => {
                // DLY
                require_and_pop!(1);
                self.suspended_deadline = self.clock.wrapping_add(i64::from(self.input.get()) * 100 * 1000);
            }
            0x8e => {
                // DLC
                self.input.pop(1);
                self.suspended_deadline = TICK_INVALID;
            }
            0x8f => {
                // RST
                self.input.pop(1);
                ret |= STATUS_OUTPUT;
            }
            0x90 => {
                // SPA
                require_and_pop!(2);
                if !self.cw().defined {
                    self.input.pop(2);
                } else {
                    let v = self.input.get();
                    let pen = &mut self.windows[self.cw].pen;
                    pen.text_tag = v >> 4;
                    pen.offset = (v >> 2) & 0x03;
                    pen.size = v & 0x03;
                    let v = self.input.get();
                    let pen = &mut self.windows[self.cw].pen;
                    pen.italics = v & 0x80 != 0;
                    pen.underline = v & 0x40 != 0;
                    pen.edge_type = (v >> 3) & 0x07;
                    pen.font = v & 0x07;
                }
            }
            0x91 => {
                // SPC
                require_and_pop!(3);
                if !self.cw().defined {
                    self.input.pop(3);
                } else {
                    let a = self.input.get();
                    let b = self.input.get();
                    let c = self.input.get();
                    let pen = &mut self.windows[self.cw].pen;
                    pen.fg_opacity = a >> 6;
                    pen.fg_color = a & 0x3f;
                    pen.bg_opacity = b >> 6;
                    pen.bg_color = b & 0x3f;
                    pen.edge_color = c & 0x3f;
                }
            }
            0x92 => {
                // SPL
                require_and_pop!(2);
                if !self.cw().defined {
                    self.input.pop(2);
                } else {
                    let a = self.input.get();
                    let b = self.input.get();
                    let w = self.cw();
                    w.row = (a & 0x0f) % WINDOW_MAX_ROWS;
                    w.col = (b & 0x3f) % WINDOW_MAX_COLS;
                }
            }
            0x97 => {
                // SWA
                require_and_pop!(4);
                if !self.cw().defined {
                    self.input.pop(4);
                } else {
                    let a = self.input.get();
                    let b = self.input.get();
                    let c = self.input.get();
                    let d = self.input.get();
                    let style = &mut self.windows[self.cw].style;
                    style.fill_opacity = a >> 6;
                    style.fill_color = a & 0x3f;
                    style.border_color = b & 0x3f;
                    style.border_type = b >> 6;
                    style.border_type |= (c & 0x80) >> 5;
                    style.word_wrap = c & 0x40 != 0;
                    style.print_direction = (c >> 4) & 0x03;
                    style.scroll_direction = (c >> 2) & 0x03;
                    style.justify = c & 0x03;
                    style.effect_speed = d >> 4;
                    style.effect_direction = (d >> 2) & 0x03;
                    style.display_effect = d & 0x03;
                }
            }
            0x80..=0x87 => {
                // CWx
                self.input.pop(1);
                let w = usize::from(code - 0x80);
                if self.windows[w].defined {
                    self.cw = w;
                }
            }
            0x98..=0x9f => {
                // DFx: defines the window and makes it current.
                require_and_pop!(6);
                self.cw = usize::from(code - 0x98);
                let v = self.input.get();
                let w = &mut self.windows[self.cw];
                if w.defined && w.visible != (v & 0x20 != 0) {
                    ret |= STATUS_OUTPUT;
                }
                w.visible = v & 0x20 != 0;
                w.row_lock = v & 0x10 != 0;
                w.column_lock = v & 0x08 != 0;
                w.priority = v & 0x07;
                let v = self.input.get();
                let w = &mut self.windows[self.cw];
                w.relative = v & 0x80 != 0;
                w.anchor_offset_v = v & 0x7f;
                let v = self.input.get();
                self.windows[self.cw].anchor_offset_h = v;
                let v = self.input.get();
                let w = &mut self.windows[self.cw];
                w.anchor_point = v >> 4;
                w.row_count = (v & 0x0f) + 1;
                let v = self.input.get();
                self.windows[self.cw].col_count = v & 0x3f;
                let v = self.input.get();
                let w = &mut self.windows[self.cw];
                let window_style = usize::from((v >> 3) & 0x07);
                if window_style > 0 {
                    w.style = DEFAULT_WINDOW_STYLES[window_style - 1];
                } else if !w.defined {
                    w.style = DEFAULT_WINDOW_STYLES[0];
                }
                let pen_style = usize::from(v & 0x07);
                if pen_style > 0 {
                    w.pen = DEFAULT_PEN_STYLES[pen_style - 1];
                } else if !w.defined {
                    w.pen = DEFAULT_PEN_STYLES[0];
                }
                w.defined = true;
            }
            _ => self.input.pop(1),
        }
        ret
    }

    fn decode_g1(&mut self, code: u8) -> u8 {
        self.input.pop(1);
        if !self.cw().defined {
            return STATUS_OK;
        }
        let utf8 = [0xc0 | (code & 0xc0) >> 6, 0x80 | (code & 0x3f), 0, 0];
        self.cw().write(utf8);
        self.text_waiting |= self.cw().visible;
        STATUS_OK
    }

    fn decode_g2g3(&mut self, code: u8) -> u8 {
        if !self.cw().defined {
            return STATUS_OK;
        }
        const CODE2UTF8: [(u8, [u8; 4]); 27] = [
            // G2
            (0x20, [0x20, 0, 0, 0]),
            (0x21, [0x20, 0, 0, 0]),
            (0x25, [0xe2, 0x80, 0xa6, 0]),
            (0x2a, [0xc5, 0xa0, 0, 0]),
            (0x2c, [0xc5, 0x92, 0, 0]),
            (0x30, [0xe2, 0x96, 0x88, 0]),
            (0x31, [0xe2, 0x80, 0x98, 0]),
            (0x32, [0xe2, 0x80, 0x99, 0]),
            (0x33, [0xe2, 0x80, 0x9c, 0]),
            (0x34, [0xe2, 0x80, 0x9d, 0]),
            (0x35, [0xe2, 0x80, 0xa2, 0]),
            (0x39, [0xe2, 0x84, 0xa2, 0]),
            (0x3a, [0xc5, 0xa1, 0, 0]),
            (0x3c, [0xc5, 0x93, 0, 0]),
            (0x3d, [0xe2, 0x84, 0xa0, 0]),
            (0x3f, [0xc5, 0xb8, 0, 0]),
            (0x76, [0xe2, 0x85, 0x9b, 0]),
            (0x77, [0xe2, 0x85, 0x9c, 0]),
            (0x78, [0xe2, 0x85, 0x9d, 0]),
            (0x79, [0xe2, 0x85, 0x9e, 0]),
            (0x7a, [0xe2, 0x94, 0x82, 0]),
            (0x7b, [0xe2, 0x94, 0x90, 0]),
            (0x7c, [0xe2, 0x94, 0x94, 0]),
            (0x7d, [0xe2, 0x94, 0x80, 0]),
            (0x7e, [0xe2, 0x94, 0x98, 0]),
            (0x7f, [0xe2, 0x94, 0x8c, 0]),
            // G3: CC
            (0xa0, [0xf0, 0x9f, 0x85, 0xb2]),
        ];
        let mut out = [b'?', 0, 0, 0];
        if let Some((_, utf8)) = CODE2UTF8.iter().find(|(c, _)| *c == code) {
            out = *utf8;
            if out[0] < 0xf0 {
                if out[0] < 0x80 {
                    out[1] = 0;
                } else if out[0] < 0xe0 {
                    out[2] = 0;
                } else {
                    out[3] = 0;
                }
            }
        }
        self.cw().write(out);
        self.text_waiting |= self.cw().visible;
        STATUS_OK
    }

    fn decode_p16(&mut self, ucs2: u16) -> u8 {
        if !self.cw().defined {
            return STATUS_OK;
        }
        let mut out = [b'?', 0, 0, 0];
        if ucs2 <= 0x7f {
            out[0] = ucs2 as u8;
        } else if ucs2 <= 0x7ff {
            out[0] = 0xc0 | (ucs2 >> 6) as u8;
            out[1] = 0x80 | (ucs2 & 0x3f) as u8;
        } else {
            out[0] = 0xe0 | (ucs2 >> 12) as u8;
            out[1] = 0x80 | ((ucs2 >> 6) & 0x3f) as u8;
            out[2] = 0x80 | (ucs2 & 0x3f) as u8;
        }
        self.cw().write(out);
        self.text_waiting |= self.cw().visible;
        STATUS_OK
    }

    /// CEA708_BuildSubtitle
    fn build_subtitle(&self) -> Output {
        let mut regions = Vec::new();
        for w in &self.windows {
            if w.defined && w.visible && w.row_count_used() != 0 {
                let mut region = Region::new();
                spu_convert(w, &mut region);
                regions.push(region);
            }
        }
        if regions.is_empty() {
            // The subpicture's own first region, untouched.
            regions.push(Region::new());
        }
        Output { start: self.clock, stop: self.clock.wrapping_add(10 * CLOCK_FREQ), regions }
    }
}

fn color_convert(c: u8) -> u32 {
    const VALUE: [u32; 4] = [0x00, 0x3f, 0xf0, 0xff];
    let c = c & 0x3f;
    (VALUE[usize::from((c >> 4) & 0x03)] << 16) | (VALUE[usize::from((c >> 2) & 0x03)] << 8) | VALUE[usize::from(c & 0x03)]
}

fn alpha_convert(c: u8) -> u8 {
    match c {
        OPACITY_TRANSLUCENT => STYLE_ALPHA_OPAQUE / 2,
        OPACITY_TRANSPARENT => STYLE_ALPHA_TRANSPARENT,
        _ => STYLE_ALPHA_OPAQUE,
    }
}

/// CEA708PenStyleToSegment, on a zeroed style (STYLE_NO_DEFAULTS).
fn pen_style_to_segment(ps: &PenStyle) -> TextStyle {
    let mut s = TextStyle::default();
    if ps.bg_opacity != OPACITY_TRANSPARENT {
        s.background_alpha = alpha_convert(ps.bg_opacity);
        s.style_flags |= STYLE_BACKGROUND;
        s.background_color = color_convert(ps.bg_color);
        s.features |= STYLE_HAS_BACKGROUND_COLOR | STYLE_HAS_BACKGROUND_ALPHA;
        if ps.bg_opacity == OPACITY_FLASH {
            s.style_flags |= STYLE_BLINK_BACKGROUND;
        }
    }
    s.font_color = color_convert(ps.fg_color);
    s.font_alpha = alpha_convert(ps.fg_opacity);
    s.features |= STYLE_HAS_FONT_ALPHA | STYLE_HAS_FONT_COLOR;
    if ps.fg_opacity == OPACITY_FLASH {
        s.style_flags |= STYLE_BLINK_FOREGROUND;
    }
    if ps.italics {
        s.style_flags |= STYLE_ITALIC;
    }
    if ps.underline {
        s.style_flags |= STYLE_UNDERLINE;
    }
    match ps.font {
        2 | 4 | 5 | 6 | 7 => {}
        _ => s.style_flags |= STYLE_MONOSPACED,
    }
    s.font_relsize = match ps.size {
        0 => FONTRELSIZE_SMALL as f32,
        2 => FONTRELSIZE_LARGE as f32,
        _ => FONTRELSIZE_STANDARD as f32,
    };
    s
}

/// CEA708CharsToSegment
fn chars_to_segment(row: &TextRow, start: u8, end: u8, newline: bool) -> StyledText {
    let style = pen_style_to_segment(&row.styles[usize::from(start)]);
    let mut text = Vec::new();
    let mut i = start;
    loop {
        for j in 0..4 {
            let byte = row.characters[usize::from(i) * 4 + j];
            if byte != 0 {
                text.push(byte);
            } else if j == 0 {
                text.push(b' ');
            } else {
                break;
            }
        }
        if i >= end {
            break;
        }
        i += 1;
    }
    if newline {
        text.push(b'\n');
    }
    StyledText { text, style }
}

/// CEA708RowToSegments
fn row_to_segments(row: &TextRow, add_newline: bool, out: &mut Vec<StyledText>) {
    if row.firstcol > row.lastcol {
        return;
    }
    let mut start = row.firstcol;
    let mut i = start;
    loop {
        let last = i == row.lastcol;
        if last || row.styles[usize::from(i)] != row.styles[usize::from(i) + 1] {
            out.push(chars_to_segment(row, start, i, add_newline && last));
            start = i + 1;
        }
        if last {
            break;
        }
        i += 1;
    }
}

/// CEA708SpuConvert (the decoder's video format has no visible size, so
/// windows with absolute anchors use the 16:9 grid).
fn spu_convert(w: &Window, region: &mut Region) {
    if !w.visible || w.row_count_used() == 0 {
        return;
    }
    let (first, last): (i32, i32) = if w.style.scroll_direction == DIRECTION_BT {
        // BT keeps the last rows between first and last.
        let last = i32::from(w.lastrow);
        let first = if i32::from(w.lastrow) - i32::from(w.row_count) < i32::from(w.firstrow) {
            i32::from(w.firstrow)
        } else {
            i32::from(w.lastrow) - i32::from(w.row_count) + 1
        };
        (first, last)
    } else {
        let first = i32::from(w.firstrow);
        let last = if i32::from(w.firstrow) + i32::from(w.row_count) > i32::from(w.lastrow) {
            i32::from(w.lastrow)
        } else {
            i32::from(w.firstrow) + i32::from(w.row_count) - 1
        };
        (first, last)
    };
    // The C loop runs a uint8_t from `first`: wrap like it does.
    if first >= 0 {
        let mut i = first as u8;
        while i32::from(i) <= last {
            if let Some(row) = w.row_at(usize::from(i)) {
                row_to_segments(row, i < w.lastrow, &mut region.segments);
            }
            match i.checked_add(1) {
                Some(next) => i = next,
                None => break,
            }
        }
    }

    if w.relative {
        region.origin_x = f32::from(w.anchor_offset_h) / 100.0;
        region.origin_y = match w.anchor_point {
            0..=2 => f32::from(w.anchor_offset_v) / 100.0,
            6..=8 => 1.0 - f32::from(w.anchor_offset_v) / 100.0,
            // One C expression a * b + c: clang fuses it (-ffp-contract=on).
            _ => (f32::from(w.anchor_offset_v) / 100.0).mul_add(CENTER_ANCHOR_RANGE, CENTER_ANCHOR_START),
        };
    } else {
        region.flags |= REGION_USES_16_9_GRID;
        region.origin_x = f32::from(w.anchor_offset_h) / SCREEN_COLS_169;
        region.origin_y = f32::from(w.anchor_offset_v) / SCREEN_ROWS;
    }
    region.flags |= REGION_ORIGIN_X_IS_RATIO | REGION_ORIGIN_Y_IS_RATIO | REGION_USES_GRID_COORDINATES;
    if w.firstrow <= w.lastrow {
        // `origin.y += firstrow * ROW_HEIGHT` in double, fused by clang.
        region.origin_y = f64::from(w.firstrow).mul_add(ROW_HEIGHT_STANDARD, f64::from(region.origin_y)) as f32;
    }
    if w.anchor_point <= 8 {
        const ALIGNS: [i32; 9] = [
            ALIGN_TOP | ALIGN_LEFT,
            ALIGN_TOP,
            ALIGN_TOP | ALIGN_RIGHT,
            ALIGN_LEFT,
            0,
            ALIGN_RIGHT,
            ALIGN_BOTTOM | ALIGN_LEFT,
            ALIGN_BOTTOM,
            ALIGN_BOTTOM | ALIGN_RIGHT,
        ];
        region.align = ALIGNS[usize::from(w.anchor_point)];
    }
    region.inner_align = ALIGN_BOTTOM | ALIGN_LEFT;
}

impl Output {
    /// True when no window shows text.
    pub fn is_empty(&self) -> bool {
        self.regions.iter().all(|r| r.segments.is_empty())
    }

    /// The output as a display state (style [`crate::STATE_STYLE`]), as VLC
    /// shows its subpicture: from `start` until the next output replaces it,
    /// `stop` at the latest. The windows' text one after the other, rows as
    /// line breaks; italics, underline and colors other than the default
    /// white kept; placed at the first window's origin. No text clears the
    /// screen.
    pub fn to_state_cue(&self) -> SubtitleCue {
        let mut segments = Vec::new();
        for region in self.regions.iter().filter(|r| !r.segments.is_empty()) {
            if !segments.is_empty() {
                segments.push(Segment::LineBreak);
            }
            for run in &region.segments {
                let text = String::from_utf8_lossy(&run.text);
                let mut lines = text.split('\n').peekable();
                while let Some(line) = lines.next() {
                    if !line.is_empty() {
                        segments.push(styled(line, &run.style));
                    }
                    if lines.peek().is_some() {
                        segments.push(Segment::LineBreak);
                    }
                }
            }
        }
        let positioning = self.regions.iter().find(|r| !r.segments.is_empty()).map(|r| CuePosition {
            x: Some(r.origin_x * 100.0),
            y: Some(r.origin_y * 100.0),
            align: if r.align & ALIGN_LEFT != 0 {
                TextAlign::Left
            } else if r.align & ALIGN_RIGHT != 0 {
                TextAlign::Right
            } else {
                TextAlign::Center
            },
            size: None,
        });
        SubtitleCue {
            start_us: self.start,
            end_us: self.stop,
            style_ref: Some(crate::STATE_STYLE.to_string()),
            positioning,
            segments,
        }
    }
}

fn styled(text: &str, style: &TextStyle) -> Segment {
    let mut segment = Segment::Text(text.to_string());
    let rgb = style.font_color;
    if rgb != 0xf0f0f0 && rgb != 0xffffff {
        segment = Segment::Color { rgb: ((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8), children: vec![segment] };
    }
    if style.style_flags & STYLE_UNDERLINE != 0 {
        segment = Segment::Underline(vec![segment]);
    }
    if style.style_flags & STYLE_ITALIC != 0 {
        segment = Segment::Italic(vec![segment]);
    }
    segment
}

/// The `cea_708` decoder: service 1, each output a display state from the
/// moment VLC queues it (see [`Output::to_state_cue`]).
pub struct Cea708Decoder {
    codec_id: CodecId,
    state: Cea708,
    out: VecDeque<SubtitleCue>,
    last_time: i64,
    eof: bool,
}

/// Decoder factory for the registry.
pub fn make_decoder(_params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(Cea708Decoder {
        codec_id: CodecId::new(CODEC_ID),
        state: Cea708::new(1),
        out: VecDeque::new(),
        last_time: 0,
        eof: false,
    }))
}

impl Decoder for Cea708Decoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.len() % 3 != 0 {
            return Err(Error::invalid("cea_708: packet is not whole cc_data triplets"));
        }
        let tb = packet.time_base.0;
        let pts_us = packet.pts.or(packet.dts).and_then(|t| ticks_to_us(t, tb.num, tb.den)).unwrap_or(self.last_time);
        self.last_time = pts_us;
        for output in self.state.decode(&packet.data, pts_us) {
            self.out.push_back(output.to_state_cue());
        }
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        match self.out.pop_front() {
            Some(cue) => Ok(Frame::Subtitle(cue)),
            None if self.eof => Err(Error::Eof),
            None => Err(Error::NeedMore),
        }
    }

    fn flush(&mut self) -> Result<()> {
        self.eof = true;
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        self.state.flush();
        self.out.clear();
        self.eof = false;
        Ok(())
    }
}
