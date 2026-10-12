// EIA-608 closed caption decoder.
//
// Ported from FFmpeg libavcodec/ccaption_dec.c (commit 2da55bf),
// LGPL-2.1-or-later: the buffered (default) and real_time modes with
// automatic data field selection.

//! EIA-608 (line 21) captions from A/53 `cc_data` triplets: pop-on,
//! roll-up, paint-on and text modes, colors, italics, underline and the
//! special and extended character sets.
//!
//! [`Cc608::new`] decodes as FFmpeg's `cc_dec` does by default: a screen
//! state is emitted once it is replaced (carriage return, erase, end of
//! caption), timed from the change that made it until the change that
//! ended it. [`Cc608::real_time`] is its `real_time` mode, which the
//! registered decoder runs: each screen is emitted as it changes, without
//! an end. Each caption carries the text FFmpeg renders as ASS
//! ([`Caption::rects`]) and the same text as styled [`Segment`]s.

use std::collections::VecDeque;

use oxideav_core::{
    CodecId, CodecParameters, CuePosition, Decoder, Error, Frame, Packet, Result, Segment, SubtitleCue, TextAlign,
};

/// The codec id: FFmpeg's `AV_CODEC_ID_EIA_608`.
pub const CODEC_ID: &str = "eia_608";

const SCREEN_ROWS: usize = 15;
const SCREEN_COLUMNS: usize = 32;
const ASS_DEFAULT_PLAYRESX: f64 = 384.0;
const ASS_DEFAULT_PLAYRESY: f64 = 288.0;
/// AV_NOPTS_VALUE
const NOPTS: i64 = i64::MIN;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    PopOn,
    PaintOn,
    RollUp,
    Text,
}

const COL_WHITE: u8 = 0;
const COL_GREEN: u8 = 1;
const COL_BLUE: u8 = 2;
const COL_CYAN: u8 = 3;
const COL_RED: u8 = 4;
const COL_YELLOW: u8 = 5;
const COL_MAGENTA: u8 = 6;
const COL_BLACK: u8 = 8;

const FONT_REGULAR: u8 = 0;
const FONT_ITALICS: u8 = 1;
const FONT_UNDERLINED: u8 = 2;
const FONT_UNDERLINED_ITALICS: u8 = 3;

const SET_BASIC_AMERICAN: u8 = 0;
const SET_SPECIAL_AMERICAN: u8 = 1;
const SET_EXTENDED_SPANISH_FRENCH_MISC: u8 = 2;
const SET_EXTENDED_PORTUGUESE_GERMAN_DANISH: u8 = 3;

/// ccaption_dec.c charset_overrides: the glyph a code stands for in a
/// character set, where it differs from ASCII.
fn charset_override(set: u8, code: u8) -> Option<&'static str> {
    Some(match (set, code) {
        (SET_BASIC_AMERICAN, 0x27) => "\u{2019}",
        (SET_BASIC_AMERICAN, 0x2a) => "\u{00e1}",
        (SET_BASIC_AMERICAN, 0x5c) => "\u{00e9}",
        (SET_BASIC_AMERICAN, 0x5e) => "\u{00ed}",
        (SET_BASIC_AMERICAN, 0x5f) => "\u{00f3}",
        (SET_BASIC_AMERICAN, 0x60) => "\u{00fa}",
        (SET_BASIC_AMERICAN, 0x7b) => "\u{00e7}",
        (SET_BASIC_AMERICAN, 0x7c) => "\u{00f7}",
        (SET_BASIC_AMERICAN, 0x7d) => "\u{00d1}",
        (SET_BASIC_AMERICAN, 0x7e) => "\u{00f1}",
        (SET_BASIC_AMERICAN, 0x7f) => "\u{2588}",
        (SET_SPECIAL_AMERICAN, 0x30) => "\u{00ae}",
        (SET_SPECIAL_AMERICAN, 0x31) => "\u{00b0}",
        (SET_SPECIAL_AMERICAN, 0x32) => "\u{00bd}",
        (SET_SPECIAL_AMERICAN, 0x33) => "\u{00bf}",
        (SET_SPECIAL_AMERICAN, 0x34) => "\u{2122}",
        (SET_SPECIAL_AMERICAN, 0x35) => "\u{00a2}",
        (SET_SPECIAL_AMERICAN, 0x36) => "\u{00a3}",
        (SET_SPECIAL_AMERICAN, 0x37) => "\u{266a}",
        (SET_SPECIAL_AMERICAN, 0x38) => "\u{00e0}",
        (SET_SPECIAL_AMERICAN, 0x39) => "\u{00a0}",
        (SET_SPECIAL_AMERICAN, 0x3a) => "\u{00e8}",
        (SET_SPECIAL_AMERICAN, 0x3b) => "\u{00e2}",
        (SET_SPECIAL_AMERICAN, 0x3c) => "\u{00ea}",
        (SET_SPECIAL_AMERICAN, 0x3d) => "\u{00ee}",
        (SET_SPECIAL_AMERICAN, 0x3e) => "\u{00f4}",
        (SET_SPECIAL_AMERICAN, 0x3f) => "\u{00fb}",
        (SET_EXTENDED_SPANISH_FRENCH_MISC, c @ 0x20..=0x3f) => {
            const SET: [&str; 32] = [
                "\u{00c1}", "\u{00c9}", "\u{00d3}", "\u{00da}", "\u{00dc}", "\u{00fc}", "\u{00b4}", "\u{00a1}",
                "*", "\u{2018}", "-", "\u{00a9}", "\u{2120}", "\u{00b7}", "\u{201c}", "\u{201d}",
                "\u{00c0}", "\u{00c2}", "\u{00c7}", "\u{00c8}", "\u{00ca}", "\u{00cb}", "\u{00eb}", "\u{00ce}",
                "\u{00cf}", "\u{00ef}", "\u{00d4}", "\u{00d9}", "\u{00f9}", "\u{00db}", "\u{00ab}", "\u{00bb}",
            ];
            SET[usize::from(c - 0x20)]
        }
        (SET_EXTENDED_PORTUGUESE_GERMAN_DANISH, c @ 0x20..=0x3f) => {
            const SET: [&str; 32] = [
                "\u{00c3}", "\u{00e3}", "\u{00cd}", "\u{00cc}", "\u{00ec}", "\u{00d2}", "\u{00f2}", "\u{00d5}",
                "\u{00f5}", "{", "}", "\\", "^", "_", "|", "~",
                "\u{00c4}", "\u{00e4}", "\u{00d6}", "\u{00f6}", "\u{00df}", "\u{00a5}", "\u{00a4}", "\u{00a6}",
                "\u{00c5}", "\u{00e5}", "\u{00d8}", "\u{00f8}", "\u{250c}", "\u{2510}", "\u{2514}", "\u{2518}",
            ];
            SET[usize::from(c - 0x20)]
        }
        _ => return None,
    })
}

const BG_ATTRIBS: [u8; 8] = [COL_WHITE, COL_GREEN, COL_BLUE, COL_CYAN, COL_RED, COL_YELLOW, COL_MAGENTA, COL_BLACK];

/// ccaption_dec.c pac2_attribs: color, font and indent of preamble address
/// and mid-row codes 0x40..0x5f (0x60..0x7f).
const PAC2_ATTRIBS: [(u8, u8, u8); 32] = [
    (COL_WHITE, FONT_REGULAR, 0),
    (COL_WHITE, FONT_UNDERLINED, 0),
    (COL_GREEN, FONT_REGULAR, 0),
    (COL_GREEN, FONT_UNDERLINED, 0),
    (COL_BLUE, FONT_REGULAR, 0),
    (COL_BLUE, FONT_UNDERLINED, 0),
    (COL_CYAN, FONT_REGULAR, 0),
    (COL_CYAN, FONT_UNDERLINED, 0),
    (COL_RED, FONT_REGULAR, 0),
    (COL_RED, FONT_UNDERLINED, 0),
    (COL_YELLOW, FONT_REGULAR, 0),
    (COL_YELLOW, FONT_UNDERLINED, 0),
    (COL_MAGENTA, FONT_REGULAR, 0),
    (COL_MAGENTA, FONT_UNDERLINED, 0),
    (COL_WHITE, FONT_ITALICS, 0),
    (COL_WHITE, FONT_UNDERLINED_ITALICS, 0),
    (COL_WHITE, FONT_REGULAR, 0),
    (COL_WHITE, FONT_UNDERLINED, 0),
    (COL_WHITE, FONT_REGULAR, 4),
    (COL_WHITE, FONT_UNDERLINED, 4),
    (COL_WHITE, FONT_REGULAR, 8),
    (COL_WHITE, FONT_UNDERLINED, 8),
    (COL_WHITE, FONT_REGULAR, 12),
    (COL_WHITE, FONT_UNDERLINED, 12),
    (COL_WHITE, FONT_REGULAR, 16),
    (COL_WHITE, FONT_UNDERLINED, 16),
    (COL_WHITE, FONT_REGULAR, 20),
    (COL_WHITE, FONT_UNDERLINED, 20),
    (COL_WHITE, FONT_REGULAR, 24),
    (COL_WHITE, FONT_UNDERLINED, 24),
    (COL_WHITE, FONT_REGULAR, 28),
    (COL_WHITE, FONT_UNDERLINED, 28),
];

/// One caption screen (`struct Screen`); the extra row and column hold
/// what the C arrays' `+1` holds.
#[derive(Clone)]
struct Screen {
    characters: [[u8; SCREEN_COLUMNS + 1]; SCREEN_ROWS + 1],
    charsets: [[u8; SCREEN_COLUMNS + 1]; SCREEN_ROWS + 1],
    colors: [[u8; SCREEN_COLUMNS + 1]; SCREEN_ROWS + 1],
    bgs: [[u8; SCREEN_COLUMNS + 1]; SCREEN_ROWS + 1],
    fonts: [[u8; SCREEN_COLUMNS + 1]; SCREEN_ROWS + 1],
    row_used: u16,
}

impl Default for Screen {
    fn default() -> Self {
        let zero = [[0u8; SCREEN_COLUMNS + 1]; SCREEN_ROWS + 1];
        Self { characters: zero, charsets: zero, colors: zero, bgs: zero, fonts: zero, row_used: 0 }
    }
}

fn check_flag(var: u16, bit: usize) -> bool {
    var & (1 << bit) != 0
}

/// One rendered screen: FFmpeg's ASS event text and the same as cue
/// segments, with the position of its first row.
#[derive(Clone, Debug, Default)]
pub struct Rect {
    /// The ASS dialogue text `cc_dec` emits for this screen.
    pub ass: String,
    /// The screen as styled segments, rows separated by line breaks.
    pub segments: Vec<Segment>,
    /// Row and column (0-based) of the first visible character.
    pub origin: Option<(u8, u8)>,
}

/// One subtitle the decoder emits (FFmpeg's `AVSubtitle`).
#[derive(Clone, Debug)]
pub struct Caption {
    /// Start, microseconds.
    pub start_us: i64,
    /// Display time in milliseconds (`end_display_time`); `None` in real
    /// time mode, where a caption stays until the next replaces it.
    pub duration_ms: Option<i64>,
    /// The screens, normally one.
    pub rects: Vec<Rect>,
}

impl Caption {
    /// The caption as a display state (style [`crate::STATE_STYLE`]): the
    /// screen as it stands after the packet (the last rect), from its start
    /// until the next state, or its end in buffered mode.
    pub fn to_state_cue(&self) -> SubtitleCue {
        let rect = self.rects.last();
        let positioning = rect.and_then(|r| r.origin).map(|(row, column)| CuePosition {
            x: Some((10.0 + 2.5 * f64::from(column)) as f32),
            y: Some((10.0 + 5.33 * f64::from(row)) as f32),
            align: TextAlign::Left,
            size: None,
        });
        let end_us = match self.duration_ms {
            Some(ms) => self.start_us.saturating_add(ms.saturating_mul(1000)),
            None => i64::MAX,
        };
        SubtitleCue {
            start_us: self.start_us,
            end_us,
            style_ref: Some(crate::STATE_STYLE.to_string()),
            positioning,
            segments: rect.map(|r| r.segments.clone()).unwrap_or_default(),
        }
    }
}

/// The decoder state (`CCaptionSubContext`).
pub struct Cc608 {
    data_field: i8,
    screen: [Screen; 2],
    active_screen: usize,
    cursor_row: u8,
    cursor_column: u8,
    cursor_color: u8,
    bg_color: u8,
    cursor_font: u8,
    cursor_charset: u8,
    buffer: [Rect; 2],
    buffer_index: usize,
    buffer_changed: bool,
    rollup: u8,
    mode: Mode,
    buffer_time: [i64; 2],
    prev_cmd: [u8; 2],
    /// FFmpeg's `real_time` option: emit each screen as it changes.
    real_time: bool,
    /// `real_time_latency_msec`, in microseconds.
    real_time_latency_us: i64,
    screen_touched: bool,
    last_real_time: i64,
}

impl Default for Cc608 {
    fn default() -> Self {
        Self::new()
    }
}

impl Cc608 {
    /// init_decoder in FFmpeg's default (buffered) mode: roll-up 2, cursor
    /// on row 11, field picked from the first triplet.
    pub fn new() -> Self {
        Self {
            data_field: -1,
            screen: [Screen::default(), Screen::default()],
            active_screen: 0,
            cursor_row: 10,
            cursor_column: 0,
            cursor_color: COL_WHITE,
            bg_color: COL_BLACK,
            cursor_font: FONT_REGULAR,
            cursor_charset: SET_BASIC_AMERICAN,
            buffer: [Rect::default(), Rect::default()],
            buffer_index: 0,
            buffer_changed: false,
            rollup: 2,
            mode: Mode::RollUp,
            buffer_time: [0, 0],
            prev_cmd: [0, 0],
            real_time: false,
            real_time_latency_us: 200_000,
            screen_touched: false,
            last_real_time: 0,
        }
    }

    /// FFmpeg's `real_time` mode (`-real_time 1`, latency 200 ms): each
    /// screen comes out as it changes, without an end, until the next
    /// replaces it.
    pub fn real_time() -> Self {
        Self { real_time: true, ..Self::new() }
    }

    /// flush_decoder: what a seek forgets. The chosen data field stays.
    pub fn flush(&mut self) {
        self.screen[0].row_used = 0;
        self.screen[1].row_used = 0;
        self.prev_cmd = [0, 0];
        self.mode = Mode::RollUp;
        self.rollup = 2;
        self.cursor_row = 10;
        self.cursor_column = 0;
        self.cursor_font = FONT_REGULAR;
        self.cursor_color = COL_WHITE;
        self.bg_color = COL_BLACK;
        self.cursor_charset = SET_BASIC_AMERICAN;
        self.active_screen = 0;
        self.last_real_time = 0;
        self.screen_touched = false;
        self.buffer_changed = false;
        self.buffer = [Rect::default(), Rect::default()];
    }

    fn writing_screen(&mut self) -> &mut Screen {
        match self.mode {
            Mode::PopOn => &mut self.screen[1 - self.active_screen],
            Mode::PaintOn | Mode::RollUp | Mode::Text => &mut self.screen[self.active_screen],
        }
    }

    fn writing_index(&self) -> usize {
        match self.mode {
            Mode::PopOn => 1 - self.active_screen,
            _ => self.active_screen,
        }
    }

    fn write_char(&mut self, screen: usize, ch: u8) {
        let col = usize::from(self.cursor_column);
        let row = usize::from(self.cursor_row);
        let s = &mut self.screen[screen];
        if col < SCREEN_COLUMNS {
            s.characters[row][col] = ch;
            s.fonts[row][col] = self.cursor_font;
            s.colors[row][col] = self.cursor_color;
            s.bgs[row][col] = self.bg_color;
            s.charsets[row][col] = self.cursor_charset;
            self.cursor_charset = SET_BASIC_AMERICAN;
            if ch != 0 {
                self.cursor_column += 1;
            }
        } else if col == SCREEN_COLUMNS && ch == 0 {
            s.characters[row][col] = ch;
        }
        // Past the last column: ignored.
    }

    fn roll_up(&mut self) {
        if self.mode == Mode::Text {
            return;
        }
        let cursor_row = i32::from(self.cursor_row);
        let keep_lines = (cursor_row + 1).min(i32::from(self.rollup));
        let s = self.writing_screen();
        for i in 0..SCREEN_ROWS as i32 {
            if i > cursor_row - keep_lines && i <= cursor_row {
                continue;
            }
            s.row_used &= !(1 << i);
        }
        let mut i = 0;
        while i < keep_lines && s.row_used != 0 {
            let i_row = (cursor_row - keep_lines + i + 1) as usize;
            for plane in [&mut s.characters, &mut s.colors, &mut s.bgs, &mut s.fonts, &mut s.charsets] {
                let next = plane[i_row + 1];
                plane[i_row][..SCREEN_COLUMNS].copy_from_slice(&next[..SCREEN_COLUMNS]);
            }
            if check_flag(s.row_used, i_row + 1) {
                s.row_used |= 1 << i_row;
            }
            i += 1;
        }
        s.row_used &= !(1 << cursor_row);
    }

    /// capture_screen: the active screen into `buffer[buffer_index]`.
    fn capture_screen(&mut self) {
        let screen = &self.screen[self.active_screen];
        let mut rect = Rect::default();
        let mut tab = 0usize;
        for i in 0..SCREEN_ROWS {
            if screen.row_used == 0 {
                break;
            }
            if check_flag(screen.row_used, i) {
                let row = &screen.characters[i];
                let charset = &screen.charsets[i];
                let mut j = 0usize;
                while row[j] == b' ' && charset[j] == SET_BASIC_AMERICAN {
                    j += 1;
                }
                if tab == 0 || j < tab {
                    tab = j;
                }
            }
        }

        let mut prev_font = FONT_REGULAR;
        let mut prev_color = COL_WHITE;
        let mut prev_bg_color = COL_BLACK;
        let mut lines: Vec<Vec<Run>> = Vec::new();
        for i in 0..SCREEN_ROWS {
            if screen.row_used == 0 {
                break;
            }
            if !check_flag(screen.row_used, i) {
                continue;
            }
            let row = &screen.characters[i];
            let font = &screen.fonts[i];
            let bg = &screen.bgs[i];
            let color = &screen.colors[i];
            let charset = &screen.charsets[i];
            let mut seen_char = false;
            let mut j = 0usize;
            while row[j] == b' ' && charset[j] == SET_BASIC_AMERICAN && j < tab {
                j += 1;
            }
            // `PLAYRES * (0.1 + 0.0250 * j)`: clang builds FFmpeg with
            // -ffp-contract=on, which fuses the inner a * b + c.
            let x = (ASS_DEFAULT_PLAYRESX * 0.0250f64.mul_add(j as f64, 0.1)) as i32;
            let y = (ASS_DEFAULT_PLAYRESY * 0.0533f64.mul_add(i as f64, 0.1)) as i32;
            rect.ass.push_str(&format!("{{\\an7}}{{\\pos({x},{y})}}"));
            if rect.origin.is_none() {
                rect.origin = Some((i as u8, j as u8));
            }
            let mut runs: Vec<Run> = Vec::new();
            while j < SCREEN_COLUMNS {
                if row[j] == 0 {
                    break;
                }
                let (mut e_tag, mut s_tag, mut c_tag, mut b_tag) = ("", "", "", "");
                if prev_font != font[j] {
                    e_tag = match prev_font {
                        FONT_ITALICS => "{\\i0}",
                        FONT_UNDERLINED => "{\\u0}",
                        FONT_UNDERLINED_ITALICS => "{\\u0}{\\i0}",
                        _ => "",
                    };
                    s_tag = match font[j] {
                        FONT_ITALICS => "{\\i1}",
                        FONT_UNDERLINED => "{\\u1}",
                        FONT_UNDERLINED_ITALICS => "{\\u1}{\\i1}",
                        _ => "",
                    };
                }
                if prev_color != color[j] {
                    c_tag = match color[j] {
                        COL_WHITE => "{\\c&HFFFFFF&}",
                        COL_GREEN => "{\\c&H00FF00&}",
                        COL_BLUE => "{\\c&HFF0000&}",
                        COL_CYAN => "{\\c&HFFFF00&}",
                        COL_RED => "{\\c&H0000FF&}",
                        COL_YELLOW => "{\\c&H00FFFF&}",
                        COL_MAGENTA => "{\\c&HFF00FF&}",
                        _ => "",
                    };
                }
                if prev_bg_color != bg[j] {
                    b_tag = match bg[j] {
                        COL_WHITE => "{\\3c&HFFFFFF&}",
                        COL_GREEN => "{\\3c&H00FF00&}",
                        COL_BLUE => "{\\3c&HFF0000&}",
                        COL_CYAN => "{\\3c&HFFFF00&}",
                        COL_RED => "{\\3c&H0000FF&}",
                        COL_YELLOW => "{\\3c&H00FFFF&}",
                        COL_MAGENTA => "{\\3c&HFF00FF&}",
                        COL_BLACK => "{\\3c&H000000&}",
                        _ => "",
                    };
                }
                prev_font = font[j];
                prev_color = color[j];
                prev_bg_color = bg[j];
                rect.ass.push_str(e_tag);
                rect.ass.push_str(s_tag);
                rect.ass.push_str(c_tag);
                rect.ass.push_str(b_tag);
                let text: String = if let Some(glyph) = charset_override(charset[j], row[j]) {
                    rect.ass.push_str(glyph);
                    seen_char = true;
                    glyph.to_string()
                } else if row[j] == b' ' && !seen_char {
                    rect.ass.push_str("\\h");
                    "\u{a0}".to_string()
                } else {
                    let c = char::from(row[j]);
                    rect.ass.push(c);
                    seen_char = true;
                    c.to_string()
                };
                match runs.last_mut() {
                    Some(run) if run.font == font[j] && run.color == color[j] => run.text.push_str(&text),
                    _ => runs.push(Run { text, font: font[j], color: color[j] }),
                }
                j += 1;
            }
            rect.ass.push_str("\\N");
            lines.push(runs);
        }
        if screen.row_used != 0 && rect.ass.len() >= 2 {
            rect.ass.truncate(rect.ass.len() - 2);
        }
        for (n, runs) in lines.into_iter().enumerate() {
            if n > 0 {
                rect.segments.push(Segment::LineBreak);
            }
            rect.segments.extend(runs.into_iter().map(Run::into_segment));
        }
        self.buffer[self.buffer_index] = rect;
        self.buffer_changed = true;
    }

    fn update_time(&mut self, pts: i64) {
        self.buffer_time[0] = self.buffer_time[1];
        self.buffer_time[1] = pts;
    }

    fn handle_bgattr(&mut self, lo: u8) {
        self.bg_color = BG_ATTRIBS[usize::from((lo & 0xf) >> 1)];
    }

    fn handle_textattr(&mut self, lo: u8) {
        let i = usize::from(lo - 0x20);
        if i >= 32 {
            return;
        }
        let screen = self.writing_index();
        self.cursor_color = PAC2_ATTRIBS[i].0;
        self.cursor_font = PAC2_ATTRIBS[i].1;
        self.screen[screen].row_used |= 1 << self.cursor_row;
        self.write_char(screen, b' ');
    }

    fn handle_pac(&mut self, hi: u8, lo: u8) {
        const ROW_MAP: [i8; 16] = [11, -1, 1, 2, 3, 4, 12, 13, 14, 15, 5, 6, 7, 8, 9, 10];
        let index = usize::from(((hi << 1) & 0x0e) | ((lo >> 5) & 0x01));
        if ROW_MAP[index] <= 0 {
            return;
        }
        let screen = self.writing_index();
        let lo = usize::from(lo & 0x1f);
        self.cursor_row = (ROW_MAP[index] - 1) as u8;
        self.cursor_color = PAC2_ATTRIBS[lo].0;
        self.cursor_font = PAC2_ATTRIBS[lo].1;
        self.cursor_charset = SET_BASIC_AMERICAN;
        self.cursor_column = 0;
        for _ in 0..PAC2_ATTRIBS[lo].2 {
            self.write_char(screen, b' ');
        }
    }

    fn handle_edm(&mut self) {
        // Buffered mode keeps writing to the screen until it is wiped and
        // captures it then; real time mode captures the wiped screen, so the
        // last one does not stay up.
        if !self.real_time {
            self.capture_screen();
        }
        self.screen[self.active_screen].row_used = 0;
        self.bg_color = COL_BLACK;
        if self.real_time {
            self.capture_screen();
        }
    }

    fn handle_eoc(&mut self) {
        self.active_screen = 1 - self.active_screen;
        // Buffered mode captures, at the next EOC, what was on screen since
        // the last one; real time mode shows the flipped screen at once.
        if !self.real_time {
            self.handle_edm();
        }
        self.cursor_column = 0;
        if self.real_time {
            self.capture_screen();
        }
    }

    fn handle_delete_end_of_row(&mut self) {
        let screen = self.writing_index();
        self.write_char(screen, 0);
    }

    fn handle_char(&mut self, hi: u8, lo: u8) {
        let screen = self.writing_index();
        self.screen[screen].row_used |= 1 << self.cursor_row;
        match hi {
            0x11 => self.cursor_charset = SET_SPECIAL_AMERICAN,
            0x12 => {
                self.cursor_column = self.cursor_column.saturating_sub(1);
                self.cursor_charset = SET_EXTENDED_SPANISH_FRENCH_MISC;
            }
            0x13 => {
                self.cursor_column = self.cursor_column.saturating_sub(1);
                self.cursor_charset = SET_EXTENDED_PORTUGUESE_GERMAN_DANISH;
            }
            _ => {
                self.cursor_charset = SET_BASIC_AMERICAN;
                self.write_char(screen, hi);
            }
        }
        if lo != 0 {
            self.write_char(screen, lo);
        }
        self.write_char(screen, 0);
        if self.mode != Mode::PopOn {
            self.screen_touched = true;
        }
    }

    fn process_cc608(&mut self, hi: u8, lo: u8) {
        if hi == self.prev_cmd[0] && lo == self.prev_cmd[1] {
            return;
        }
        self.prev_cmd = [hi, lo];
        if (hi == 0x10 && (0x40..=0x5f).contains(&lo)) || ((0x11..=0x17).contains(&hi) && (0x40..=0x7f).contains(&lo)) {
            self.handle_pac(hi, lo);
        } else if (hi == 0x11 && (0x20..=0x2f).contains(&lo)) || (hi == 0x17 && (0x2e..=0x2f).contains(&lo)) {
            self.handle_textattr(lo);
        } else if hi == 0x10 && (0x20..=0x2f).contains(&lo) {
            self.handle_bgattr(lo);
        } else if hi == 0x14 || hi == 0x15 || hi == 0x1c {
            match lo {
                0x20 => self.mode = Mode::PopOn,
                0x24 => self.handle_delete_end_of_row(),
                0x25..=0x27 => {
                    self.rollup = lo - 0x23;
                    self.mode = Mode::RollUp;
                }
                0x29 => self.mode = Mode::PaintOn,
                0x2b => self.mode = Mode::Text,
                0x2c => self.handle_edm(),
                0x2d => {
                    // Carriage return: buffered mode captures the screen.
                    if !self.real_time {
                        self.capture_screen();
                    }
                    self.roll_up();
                    self.cursor_column = 0;
                }
                0x2e => {
                    // Erase non-displayed memory, in real time mode only:
                    // buffered mode keeps its own use of the inactive screen.
                    if self.real_time {
                        self.screen[1 - self.active_screen].row_used = 0;
                    }
                }
                0x2f => self.handle_eoc(),
                _ => {}
            }
        } else if (0x11..=0x13).contains(&hi) {
            self.handle_char(hi, lo);
        } else if hi >= 0x20 {
            self.handle_char(hi, lo);
            self.prev_cmd = [0, 0];
        } else if hi == 0x17 && (0x21..=0x23).contains(&lo) {
            for _ in 0..lo - 0x20 {
                self.handle_char(b' ', 0);
            }
        }
    }

    /// One packet of triplets at `pts_us` (`decode`): the subtitle it
    /// completes, if any. A packet without a time is `AV_NOPTS_VALUE` to
    /// the decoder, and a subtitle left without one is dropped, as the
    /// ffmpeg tool drops it.
    pub fn decode(&mut self, data: &[u8], pts_us: Option<i64>) -> Option<Caption> {
        let in_time = pts_us.unwrap_or(NOPTS);
        let mut caption: Option<Caption> = None;
        // `sub->pts`: the packet time until a rect retimes it.
        let mut sub_pts = in_time;
        let mut bidx = self.buffer_index;
        for triplet in data.chunks_exact(3) {
            let cc_type = (triplet[0] & 1) as i8;
            if self.data_field < 0 {
                self.data_field = cc_type;
            }
            let Some(hi) = validate_cc_data_pair(triplet) else { continue };
            if cc_type != self.data_field {
                continue;
            }
            self.process_cc608(hi & 0x7f, triplet[2] & 0x7f);
            if !self.buffer_changed {
                continue;
            }
            self.buffer_changed = false;
            if !self.real_time && self.mode == Mode::PopOn {
                self.buffer_index = 1 - self.buffer_index;
                bidx = self.buffer_index;
            }
            self.update_time(in_time);
            if !self.buffer[bidx].ass.is_empty() || self.real_time {
                let start = self.buffer_time[0];
                let end = self.buffer_time[1];
                sub_pts = start;
                let caption = caption.get_or_insert_with(|| Caption { start_us: 0, duration_ms: None, rects: Vec::new() });
                // One AVSubtitle per packet: later rects reset its timing.
                caption.start_us = start;
                caption.duration_ms = (!self.real_time).then(|| rescale_us_to_ms(end.saturating_sub(start)));
                caption.rects.push(self.buffer[bidx].clone());
                self.last_real_time = sub_pts;
                self.screen_touched = false;
            }
        }
        // Real time mode: a screen being written (roll-up, paint-on) comes
        // out once the latency has passed since the last one.
        if self.real_time
            && self.screen_touched
            && sub_pts >= self.last_real_time.saturating_add(self.real_time_latency_us)
        {
            self.last_real_time = sub_pts;
            self.screen_touched = false;
            self.capture_screen();
            self.buffer_changed = false;
            let caption = caption.get_or_insert_with(|| Caption { start_us: sub_pts, duration_ms: None, rects: Vec::new() });
            caption.duration_ms = None;
            caption.rects.push(self.buffer[bidx].clone());
        }
        caption.filter(|c| c.start_us != NOPTS)
    }

    /// The end of the stream (`decode` with no data): in buffered mode the
    /// screen still buffered, timed as FFmpeg times it.
    pub fn finish(&mut self) -> Option<Caption> {
        if self.real_time {
            return None;
        }
        let bidx = 1 - self.buffer_index;
        if self.buffer[bidx].ass.is_empty() {
            return None;
        }
        let rect = std::mem::take(&mut self.buffer[bidx]);
        let mut duration_ms = rescale_us_to_ms(self.buffer_time[1].saturating_sub(self.buffer_time[0]));
        if duration_ms == 0 {
            duration_ms = rect.ass.len() as i64 * 20;
        }
        let caption = Caption { start_us: self.buffer_time[1], duration_ms: Some(duration_ms), rects: vec![rect] };
        Some(caption).filter(|c| c.start_us != NOPTS)
    }
}

/// A run of characters with one style.
struct Run {
    text: String,
    font: u8,
    color: u8,
}

impl Run {
    fn into_segment(self) -> Segment {
        let mut segment = Segment::Text(self.text);
        let rgb = match self.color {
            COL_GREEN => Some((0, 255, 0)),
            COL_BLUE => Some((0, 0, 255)),
            COL_CYAN => Some((0, 255, 255)),
            COL_RED => Some((255, 0, 0)),
            COL_YELLOW => Some((255, 255, 0)),
            COL_MAGENTA => Some((255, 0, 255)),
            _ => None,
        };
        if let Some(rgb) = rgb {
            segment = Segment::Color { rgb, children: vec![segment] };
        }
        if self.font == FONT_UNDERLINED || self.font == FONT_UNDERLINED_ITALICS {
            segment = Segment::Underline(vec![segment]);
        }
        if self.font == FONT_ITALICS || self.font == FONT_UNDERLINED_ITALICS {
            segment = Segment::Italic(vec![segment]);
        }
        segment
    }
}

/// validate_cc_data_pair: the first data byte (a solid block when it fails
/// parity), or `None` for a pair to skip: invalid, failed parity in the
/// second byte, padding, or CEA-708.
fn validate_cc_data_pair(pair: &[u8]) -> Option<u8> {
    let cc_valid = pair[0] & 4 != 0;
    let cc_type = pair[0] & 3;
    let mut hi = pair[1];
    if !cc_valid {
        return None;
    }
    if cc_type == 0 || cc_type == 1 {
        if pair[2].count_ones() % 2 == 0 {
            return None;
        }
        if pair[1].count_ones() % 2 == 0 {
            hi = 0x7f;
        }
    }
    if (pair[0] == 0xfa || pair[0] == 0xfc || pair[0] == 0xfd) && pair[1] & 0x7f == 0 && pair[2] & 0x7f == 0 {
        return None;
    }
    if cc_type == 3 || cc_type == 2 {
        return None;
    }
    Some(hi)
}

/// av_rescale_q(us, AV_TIME_BASE_Q, {1, 1000}): rounded to the nearest,
/// halves away from zero.
fn rescale_us_to_ms(us: i64) -> i64 {
    let us = i128::from(us);
    let ms = if us >= 0 { (us + 500) / 1000 } else { -((-us + 500) / 1000) };
    ms as i64
}

/// av_rescale_q(ticks, time_base, AV_TIME_BASE_Q).
pub fn ticks_to_us(ticks: i64, num: i64, den: i64) -> Option<i64> {
    if den <= 0 || num <= 0 {
        return None;
    }
    let b = i128::from(num) * 1_000_000;
    let c = i128::from(den);
    let a = i128::from(ticks);
    let r = if a >= 0 {
        a.checked_mul(b)?.checked_add(c / 2)? / c
    } else {
        -((-a).checked_mul(b)?.checked_add(c / 2)? / c)
    };
    i64::try_from(r).ok()
}

/// The `eia_608` decoder, as a player shows captions: FFmpeg's real time
/// mode, each screen a display state ([`Caption::to_state_cue`]) shown from
/// the moment it changes until the next replaces it.
pub struct Eia608Decoder {
    codec_id: CodecId,
    state: Cc608,
    out: VecDeque<SubtitleCue>,
    eof: bool,
}

/// Decoder factory for the registry.
pub fn make_decoder(_params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(Eia608Decoder { codec_id: CodecId::new(CODEC_ID), state: Cc608::real_time(), out: VecDeque::new(), eof: false }))
}

impl Decoder for Eia608Decoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.len() % 3 != 0 {
            return Err(Error::invalid("eia_608: packet is not whole cc_data triplets"));
        }
        let time_base = packet.time_base.0;
        let pts_us = packet.pts.or(packet.dts).and_then(|t| ticks_to_us(t, time_base.num, time_base.den));
        if let Some(caption) = self.state.decode(&packet.data, pts_us) {
            self.out.push_back(caption.to_state_cue());
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
