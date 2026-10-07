//! DVB subtitle decoder, ported from FFmpeg `libavcodec/dvbsubdec.c`
//! (commit 2da55bf; header verified LGPL-2.1-or-later).
//!
//! Implements FFmpeg's default options: timeout-based end times and
//! computed palettes when no CLUT is supplied. Region pixels survive
//! composition updates; acquisition/mode-change pages replace the epoch.
//! Both bare Matroska segments and DVB private-PES framing are accepted.
//!
//! Deliberate differences from FFmpeg:
//! - Only the stream's first service is decoded: the composition and
//!   ancillary pages of the first record in the extradata (MPEG-TS
//!   descriptor 0x59, Matroska CodecPrivate), as VLC's dvbsub.c filters a
//!   PID by its service's pages and as FFmpeg does with `dvb_substream` 0.
//!   FFmpeg's default decodes every page, so two services sharing a PID
//!   would draw over each other. Without valid extradata every page is
//!   decoded, as in FFmpeg.
//! - Hostile-input bounds FFmpeg does not have: canvases (display
//!   definitions) of at most 4096x4096, at most 4096x4096 region pixels in
//!   all, at most 1024 object placements, and per packet at most
//!   `PAINT_WORK_PER_PACKET` of object painting; one display end with no
//!   bitmap renders its blank canvas once per packet.
//! - A segment shorter than the fields its parser reads ends the packet
//!   with an error, and pixel strings read zeros past their block; FFmpeg
//!   reads on into the bytes that follow (the next segment, then zero
//!   padding). Both only happen on truncated segments.

use std::borrow::Cow;
use std::collections::HashMap;

use oxideav_core::{CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result};

use crate::colorspace::{Matrix, ycbcr_to_rgb};
use crate::subtitle::{CanvasFrames, MAX_SIDE, Rect, Subtitle};

/// dvbsubdec.c's region limit: `width * height * 2 > 320 * 1024 * 8` fails.
const MAX_REGION_PIXELS: usize = 320 * 1024 * 4;
/// Pixels of all regions together: a canvas' worth.
const MAX_REGION_BYTES: usize = MAX_SIDE * MAX_SIDE;
/// Object placements (an object shown in a region) at any time.
const MAX_OBJECT_DISPLAYS: usize = 1024;
/// Painting one packet's objects may cost at most this much: for each
/// placement, its field data plus every pixel of its region.
const PAINT_WORK_PER_PACKET: usize = 16 << 20;

fn invalid() -> Error {
    Error::invalid("DVB subtitle: invalid or truncated segment")
}

fn be16(bytes: &[u8]) -> usize {
    usize::from(u16::from_be_bytes([bytes[0], bytes[1]]))
}

struct Clut {
    version: Option<u8>,
    two: [[u8; 4]; 4],
    four: [[u8; 4]; 16],
    eight: [[u8; 4]; 256],
}

impl Default for Clut {
    fn default() -> Self {
        let mut clut = Self {
            version: None,
            two: [[0, 0, 0, 0], [255, 255, 255, 255], [0, 0, 0, 255], [127, 127, 127, 255]],
            four: [[0; 4]; 16],
            eight: [[0; 4]; 256],
        };
        for i in 1..16 {
            let level = if i < 8 { 255 } else { 127 };
            clut.four[i] = [if i & 1 != 0 { level } else { 0 }, if i & 2 != 0 { level } else { 0 }, if i & 4 != 0 { level } else { 0 }, 255];
        }
        for i in 1..256 {
            let (base, low, high, alpha) = if i < 8 {
                (0, 255, 0, 63)
            } else {
                match i & 0x88 {
                    0x00 => (0, 85, 170, 255),
                    0x08 => (0, 85, 170, 127),
                    0x80 => (127, 43, 85, 255),
                    _ => (0, 43, 85, 255),
                }
            };
            let component = |mask: usize| {
                base + (if i & mask != 0 { low } else { 0 }) + (if i & (mask << 4) != 0 { high } else { 0 })
            };
            clut.eight[i] = [component(1), component(2), component(4), alpha];
        }
        clut
    }
}

impl Clut {
    fn table(&self, depth: u8) -> &[[u8; 4]] {
        match depth {
            2 => &self.two,
            8 => &self.eight,
            _ => &self.four,
        }
    }
}

#[derive(Clone, Copy)]
struct ObjectDisplay {
    region: u8,
    x: usize,
    y: usize,
}

#[derive(Default)]
struct Region {
    width: usize,
    height: usize,
    depth: u8,
    clut: u8,
    dirty: bool,
    pixels: Vec<u8>,
    objects: Vec<u16>,
    computed: Option<Box<[[u8; 4]; 256]>>,
    computed_valid: bool,
}

struct RegionDisplay {
    region: u8,
    x: i32,
    y: i32,
}

struct DisplayDefinition {
    version: u8,
    x: i32,
    y: i32,
}

struct Context {
    version: Option<u8>,
    timeout: u8,
    regions: HashMap<u8, Region>,
    objects: HashMap<u16, Vec<ObjectDisplay>>,
    cluts: HashMap<u8, Clut>,
    displays: Vec<RegionDisplay>,
    definition: Option<DisplayDefinition>,
    width: i32,
    height: i32,
    pixel_bytes: usize,
    object_displays: usize,
    /// Object painting this packet may still do (`PAINT_WORK_PER_PACKET`).
    paint_left: usize,
    // Allocated only for streams without an explicit CLUT, then reused.
    adjacency: Vec<u32>,
}

impl Context {
    fn new(params: &CodecParameters) -> Self {
        Self {
            version: None,
            timeout: 0,
            regions: HashMap::new(),
            objects: HashMap::new(),
            cluts: HashMap::new(),
            displays: Vec::new(),
            definition: None,
            width: params.width.and_then(|v| i32::try_from(v).ok()).unwrap_or(0),
            height: params.height.and_then(|v| i32::try_from(v).ok()).unwrap_or(0),
            pixel_bytes: 0,
            object_displays: 0,
            paint_left: PAINT_WORK_PER_PACKET,
            adjacency: Vec::new(),
        }
    }

    fn page(&mut self, data: &[u8]) -> Result<()> {
        if data.len() < 2 {
            return Err(invalid());
        }
        let version = data[1] >> 4;
        if self.version == Some(version) {
            return Ok(());
        }
        self.version = Some(version);
        self.timeout = data[0];
        if matches!((data[1] >> 2) & 3, 1 | 2) {
            self.regions.clear();
            self.objects.clear();
            self.cluts.clear();
            self.pixel_bytes = 0;
            self.object_displays = 0;
        }
        self.displays.clear();
        let mut seen = [false; 256];
        for entry in data[2..].chunks_exact(6) {
            if seen[usize::from(entry[0])] {
                break;
            }
            seen[usize::from(entry[0])] = true;
            self.displays.push(RegionDisplay {
                region: entry[0],
                x: be16(&entry[2..]) as i32,
                y: be16(&entry[4..]) as i32,
            });
        }
        // FFmpeg prepends page regions: later wire entries are painted first.
        self.displays.reverse();
        Ok(())
    }

    fn region(&mut self, data: &[u8]) -> Result<()> {
        if data.len() < 10 {
            return Err(invalid());
        }
        let id = data[0];
        let region = self.regions.entry(id).or_default();
        let width = be16(&data[2..]);
        let height = be16(&data[4..]);
        let area = width * height;
        // dvbsubdec.c checks the area (av_image_check_size2 cannot fail
        // within it for 16-bit sides); the total is this port's bound.
        if width == 0 || height == 0 || area > MAX_REGION_PIXELS
            || self.pixel_bytes - region.pixels.len() + area > MAX_REGION_BYTES
        {
            region.width = 0;
            region.height = 0;
            return Err(invalid());
        }
        let resize = area != region.pixels.len();
        region.width = width;
        region.height = height;
        region.depth = match (data[6] >> 2) & 7 {
            1 => 2,
            3 => 8,
            _ => 4,
        };
        region.clut = data[7];
        let background = match region.depth {
            8 => data[8],
            2 => (data[9] >> 2) & 3,
            _ => data[9] >> 4,
        };
        if resize {
            self.pixel_bytes = self.pixel_bytes - region.pixels.len() + area;
            // Do not retain a formerly larger allocation after shrinking:
            // pixel_bytes is also the decoder's allocation bound.
            region.pixels = vec![background; area];
            region.dirty = false;
        } else if data[1] & 8 != 0 {
            region.pixels.fill(background);
        }
        for object_id in region.objects.drain(..) {
            if let Some(displays) = self.objects.get_mut(&object_id) {
                let before = displays.len();
                displays.retain(|display| display.region != id);
                self.object_displays -= before - displays.len();
                if displays.is_empty() {
                    self.objects.remove(&object_id);
                }
            }
        }
        let mut rest = &data[10..];
        while rest.len() >= 6 {
            let object_id = be16(rest) as u16;
            let kind = rest[2] >> 6;
            let x = be16(&rest[2..]) & 0xfff;
            let y = be16(&rest[4..]) & 0xfff;
            rest = &rest[6..];
            if x >= width || y >= height || self.object_displays == MAX_OBJECT_DISPLAYS {
                return Err(invalid());
            }
            if matches!(kind, 1 | 2) && rest.len() >= 2 {
                // Foreground/background colors apply to character objects,
                // which FFmpeg does not implement either.
                rest = &rest[2..];
            }
            self.objects.entry(object_id).or_default().push(ObjectDisplay { region: id, x, y });
            region.objects.push(object_id);
            self.object_displays += 1;
        }
        Ok(())
    }

    fn clut(&mut self, data: &[u8]) -> Result<()> {
        if data.len() < 2 {
            return Err(invalid());
        }
        let clut = self.cluts.entry(data[0]).or_default();
        let version = data[1] >> 4;
        if clut.version == Some(version) {
            return Ok(());
        }
        clut.version = Some(version);
        let mut rest = &data[2..];
        // Match FFmpeg's strict >4 loop, including its reduced-range tail.
        while rest.len() > 4 {
            let entry = usize::from(rest[0]);
            let flags = rest[1];
            let (y, cr, cb, mut transparency, used) = if flags & 1 != 0 {
                if rest.len() < 6 {
                    return Err(invalid());
                }
                (rest[2], rest[3], rest[4], rest[5], 6)
            } else {
                (rest[2] & 0xfc, (((rest[2] & 3) << 2) | (rest[3] >> 6)) << 4,
                 (rest[3] << 2) & 0xf0, (rest[3] << 6) & 0xc0, 4)
            };
            if y == 0 {
                transparency = 255;
            }
            let [r, g, b] = ycbcr_to_rgb(y, cb, cr, Matrix::Bt601);
            let color = [r, g, b, 255 - transparency];
            if flags & 0x80 != 0 && entry < 4 {
                clut.two[entry] = color;
            } else if flags & 0x40 != 0 && entry < 16 {
                clut.four[entry] = color;
            } else if flags & 0x20 != 0 {
                clut.eight[entry] = color;
            }
            rest = &rest[used..];
        }
        Ok(())
    }

    fn object(&mut self, data: &[u8]) -> Result<()> {
        if data.len() < 3 {
            return Err(invalid());
        }
        let Some(displays) = self.objects.get(&(be16(data) as u16)) else { return Err(invalid()) };
        if (data[2] >> 2) & 3 != 0 {
            return Err(Error::invalid("DVB subtitle: character/progressive object coding is not supported by the FFmpeg port"));
        }
        if data.len() < 7 {
            return Err(invalid());
        }
        let top_len = be16(&data[3..]);
        let bottom_len = be16(&data[5..]);
        if top_len + bottom_len > data.len() - 7 {
            return Err(invalid());
        }
        let top = &data[7..7 + top_len];
        let bottom = if bottom_len == 0 { top } else { &data[7 + top_len..7 + top_len + bottom_len] };
        for display in displays.iter().rev() {
            if let Some(region) = self.regions.get_mut(&display.region) {
                // Painting a placement reads its field data and writes at
                // most every pixel of its region.
                let work = top.len() + bottom.len() + region.pixels.len();
                self.paint_left = self.paint_left.checked_sub(work)
                    .ok_or(Error::invalid("DVB subtitle: object painting exceeds the per-packet bound"))?;
                region.paint_block(display, top, 0, data[2] & 2 != 0);
                region.paint_block(display, bottom, 1, data[2] & 2 != 0);
            }
        }
        Ok(())
    }

    fn display_definition(&mut self, data: &[u8]) -> Result<()> {
        if data.len() < 5 {
            return Err(invalid());
        }
        let version = data[0] >> 4;
        if self.definition.as_ref().is_some_and(|d| d.version == version) {
            return Ok(());
        }
        // dvbsubdec.c records the version and a zero offset before it checks
        // the size or the window: a failed definition is not parsed again.
        self.definition = Some(DisplayDefinition { version, x: 0, y: 0 });
        let width = be16(&data[1..]) + 1;
        let height = be16(&data[3..]) + 1;
        if self.width == 0 || self.height == 0 {
            if width > MAX_SIDE || height > MAX_SIDE {
                return Err(invalid());
            }
            self.width = width as i32;
            self.height = height as i32;
        }
        if data[0] & 8 != 0 {
            if data.len() < 13 {
                return Err(invalid());
            }
            self.definition = Some(DisplayDefinition { version, x: be16(&data[5..]) as i32, y: be16(&data[9..]) as i32 });
        }
        Ok(())
    }

    fn subtitle(&mut self, pts: Option<i64>) -> Subtitle<'_> {
        for display in &self.displays {
            if let Some(region) = self.regions.get_mut(&display.region) {
                if region.dirty && !self.cluts.contains_key(&region.clut) && !region.computed_valid {
                    if self.adjacency.is_empty() {
                        self.adjacency.resize(257 * 256, 0);
                    }
                    region.compute_clut(&mut self.adjacency);
                }
            }
        }
        let (offset_x, offset_y) = self.definition.as_ref().map_or((0, 0), |d| (d.x, d.y));
        let mut rects = Vec::with_capacity(self.displays.len());
        for display in &self.displays {
            let Some(region) = self.regions.get(&display.region).filter(|r| r.dirty) else { continue };
            let palette: &[[u8; 4]] = match self.cluts.get(&region.clut) {
                Some(clut) => clut.table(region.depth),
                None => region.computed.as_ref().expect("computed above").as_slice(),
            };
            rects.push(Rect {
                x: display.x + offset_x,
                y: display.y + offset_y,
                w: region.width as i32,
                h: region.height as i32,
                linesize: region.width,
                pixels: Cow::Borrowed(&region.pixels),
                palette: Cow::Borrowed(palette),
            });
        }
        Subtitle { pts, start_display_time: 0, end_display_time: u32::from(self.timeout) * 1000, rects }
    }
}

impl Region {
    fn paint_block(&mut self, display: &ObjectDisplay, data: &[u8], field: usize, non_mod: bool) {
        self.dirty = true;
        let (mut x, mut y) = (display.x, display.y + field);
        let mut map_two_four = [0, 7, 8, 15];
        let mut map_two_eight = [0, 0x77, 0x88, 0xff];
        let mut map_four_eight = std::array::from_fn::<_, 16, _>(|i| i as u8 * 17);
        let mut at = 0;
        while at < data.len() {
            let tag = data[at];
            at += 1;
            if (tag != 0xf0 && x >= self.width) || y >= self.height {
                return;
            }
            match tag {
                0x10..=0x12 => {
                    let depth = 2 << (tag - 0x10);
                    if depth > self.depth {
                        return;
                    }
                    let map: Option<&[u8]> = match (depth, self.depth) {
                        (2, 4) => Some(&map_two_four),
                        (2, 8) => Some(&map_two_eight),
                        (4, 8) => Some(&map_four_eight),
                        _ => None,
                    };
                    let row = &mut self.pixels[y * self.width..(y + 1) * self.width];
                    let (next_x, used) = read_string(row, x, &data[at..], depth, non_mod, map);
                    x = next_x;
                    at = (at + used).min(data.len());
                }
                // A map table cut off by the end of the block ends it.
                // dvbsubdec.c reads the table from the bytes that follow
                // and stops there too, still invalidating the computed CLUT.
                0x20 => {
                    let Some(bytes) = data.get(at..at + 2) else { break };
                    map_two_four = [bytes[0] >> 4, bytes[0] & 15, bytes[1] >> 4, bytes[1] & 15];
                    at += 2;
                }
                0x21 => {
                    let Some(bytes) = data.get(at..at + 4) else { break };
                    map_two_eight.copy_from_slice(bytes);
                    at += 4;
                }
                0x22 => {
                    let Some(bytes) = data.get(at..at + 16) else { break };
                    map_four_eight.copy_from_slice(bytes);
                    at += 16;
                }
                0xf0 => {
                    x = display.x;
                    y += 2;
                }
                _ => {}
            }
        }
        self.computed_valid = false;
    }

    fn compute_clut(&mut self, adjacency: &mut [u32]) {
        adjacency.fill(0);
        let mut edges = [0u32; 256];
        let width = self.width;
        let height = self.height;
        for y in 0..height {
            for x in 0..width {
                let at = y * width + x;
                let value = usize::from(self.pixels[at]);
                let neighbors = [
                    if x > 0 { usize::from(self.pixels[at - 1]) + 1 } else { 0 },
                    if x + 1 < width { usize::from(self.pixels[at + 1]) + 1 } else { 0 },
                    if y > 0 { usize::from(self.pixels[at - width]) + 1 } else { 0 },
                    if y + 1 < height { usize::from(self.pixels[at + width]) + 1 } else { 0 },
                ];
                edges[value] += u32::from(neighbors.iter().any(|&v| v != value + 1));
                for neighbor in neighbors {
                    adjacency[neighbor * 256 + value] += 1;
                }
            }
        }
        let mut selected = [false; 256];
        let mut order = [0usize; 256];
        let mut count = 0;
        let mut scores: [u64; 256] = std::array::from_fn(|i| u64::from(adjacency[i]));
        for i in 0..256 {
            adjacency[(i + 1) * 256 + i] = 0;
        }
        loop {
            let (mut best, mut best_score) = (0, 0);
            for value in 0..256 {
                if !selected[value] && scores[value] != 0 && edges[value] != 0 {
                    let score = 1024 * scores[value] / u64::from(edges[value]);
                    if score > best_score {
                        best = value;
                        best_score = score;
                    }
                }
            }
            if best_score == 0 {
                break;
            }
            selected[best] = true;
            order[count] = best;
            count += 1;
            for value in 0..256 {
                scores[value] += u64::from(adjacency[(best + 1) * 256 + value]);
            }
        }
        let palette = self.computed.get_or_insert_with(|| Box::new([[0; 4]; 256]));
        for (rank, &value) in order[..count].iter().enumerate() {
            let v = (rank * 255 / count.saturating_sub(1).max(1)) as u8;
            palette[value] = [v / 2, v, v / 2, v];
        }
        self.computed_valid = true;
    }
}

struct Bits<'a> {
    data: &'a [u8],
    at: usize,
}

impl Bits<'_> {
    // Bits past the block read as zeros. dvbsubdec.c's reader goes on into
    // the bytes that follow it (the next segment, then zero padding); the
    // two differ only on a truncated pixel string.
    fn get(&mut self, count: usize) -> usize {
        let byte = self.at >> 3;
        let word = (u16::from(self.data.get(byte).copied().unwrap_or(0)) << 8)
            | u16::from(self.data.get(byte + 1).copied().unwrap_or(0));
        let shift = 16 - (self.at & 7) - count;
        self.at += count;
        usize::from((word >> shift) & ((1 << count) - 1))
    }
}

fn read_string(row: &mut [u8], x: usize, data: &[u8], depth: u8, non_mod: bool, map: Option<&[u8]>) -> (usize, usize) {
    let mut bits = Bits { data, at: 0 };
    let (mut logical, mut written) = (x, x);
    let mut ended = false;
    while bits.at < data.len() * 8 && logical < row.len() {
        let color = bits.get(usize::from(depth));
        let (run, color) = if color != 0 {
            (1, color)
        } else if depth == 2 {
            if bits.get(1) != 0 {
                (bits.get(3) + 3, bits.get(2))
            } else if bits.get(1) != 0 {
                (1, 0)
            } else {
                match bits.get(2) {
                    0 => { ended = true; break; }
                    1 => (2, 0),
                    2 => (bits.get(4) + 12, bits.get(2)),
                    _ => (bits.get(8) + 29, bits.get(2)),
                }
            }
        } else if depth == 4 {
            if bits.get(1) == 0 {
                let run = bits.get(3);
                if run == 0 { ended = true; break; }
                (run + 2, 0)
            } else if bits.get(1) == 0 {
                (bits.get(2) + 4, bits.get(4))
            } else {
                match bits.get(2) {
                    0 => (1, 0),
                    1 => (2, 0),
                    2 => (bits.get(4) + 9, bits.get(4)),
                    _ => (bits.get(8) + 25, bits.get(4)),
                }
            }
        } else {
            let flags = bits.get(8);
            let run = flags & 127;
            if flags & 128 == 0 {
                if run == 0 { ended = true; break; }
                (run, 0)
            } else {
                (run, bits.get(8))
            }
        };
        if non_mod && color == 1 {
            // FFmpeg advances the logical column but not the destination
            // pointer for the non-modifying color. Preserve that behavior.
            logical += run;
        } else {
            let count = run.min(row.len() - logical);
            row[written..written + count].fill(map.map_or(color as u8, |m| m[color]));
            written += count;
            logical += count;
        }
    }
    if !ended {
        bits.get(if depth == 2 { 6 } else { 8 });
        if depth == 8 && data.get(bits.at / 8).copied().unwrap_or(0) == 0 {
            bits.get(8);
        }
    }
    let used = bits.at.div_ceil(8);
    (logical, if depth == 8 { used.min(data.len()) } else { used })
}

struct DvbDecoder {
    codec_id: CodecId,
    context: Context,
    canvas: CanvasFrames,
    /// The decoded service's composition and ancillary page ids; `None`
    /// decodes every page.
    pages: Option<[u16; 2]>,
}

pub(crate) fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    // dvbsubdec_init's extradata check: one 4-byte record, or 5-byte
    // records (composition page, ancillary page, subtitling type).
    let extradata = &params.extradata;
    let pages = (extradata.len() >= 4 && (extradata.len() % 5 == 0 || extradata.len() == 4))
        .then(|| [be16(extradata) as u16, be16(&extradata[2..]) as u16]);
    Ok(Box::new(DvbDecoder {
        codec_id: params.codec_id.clone(),
        context: Context::new(params),
        canvas: CanvasFrames::new(params.width, params.height),
        pages,
    }))
}

/// What a display end outputs: its canvas, rendered at once when it has
/// bitmaps (later segments of the packet may still change the regions), or
/// a blank state, rendered once the packet ends: dvbsubdec.c saves a blank
/// set again at every display end and only the packet's last one is output.
enum Output {
    Bitmaps(Frame),
    Blank { pts: Option<i64>, end_display_time: u32, width: i32, height: i32 },
}

impl DvbDecoder {
    fn output(&mut self, packet: &Packet) -> Result<Output> {
        let (width, height) = (self.context.width, self.context.height);
        let subtitle = self.context.subtitle(packet.pts);
        if subtitle.rects.is_empty() {
            let (pts, end_display_time) = (subtitle.pts, subtitle.end_display_time);
            return Ok(Output::Blank { pts, end_display_time, width, height });
        }
        Ok(Output::Bitmaps(self.canvas.render(subtitle, packet, width, height)?))
    }
}

impl Decoder for DvbDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        // MPEG-TS delivers the complete private-PES payload, including the
        // data_identifier and subtitle_stream_id. Matroska stores segments.
        let mut data = packet.data.as_slice();
        if data.first() == Some(&0x20) && data.len() >= 2 {
            data = &data[2..];
        }
        if data.len() <= 6 || data.first() != Some(&0x0f) {
            return Err(invalid());
        }
        self.context.paint_left = PAINT_WORK_PER_PACKET;
        let (mut page, mut region, mut object, mut definition) = (false, false, false, false);
        let mut output: Option<Output> = None;
        while data.len() >= 6 && data[0] == 0x0f {
            let kind = data[1];
            let page_id = be16(&data[2..]) as u16;
            let len = be16(&data[4..]);
            if len > data.len() - 6 {
                return Err(invalid());
            }
            let body = &data[6..6 + len];
            if self.pages.is_none_or(|pages| pages.contains(&page_id)) {
                match kind {
                    0x10 => { self.context.page(body)?; page = true; }
                    0x11 => { self.context.region(body)?; region = true; }
                    0x12 => self.context.clut(body)?,
                    0x13 => { self.context.object(body)?; object = true; }
                    0x14 => { self.context.display_definition(body)?; definition = true; }
                    0x80 => {
                        if matches!(output, Some(Output::Bitmaps(_))) {
                            return Err(Error::invalid("DVB subtitle: repeated display end with bitmap rectangles"));
                        }
                        output = Some(self.output(packet)?);
                    }
                    _ => {}
                }
            }
            data = &data[6 + len..];
        }
        if page && region && object {
            if !definition && self.context.width == 0 && self.context.height == 0 {
                self.context.width = 720;
                self.context.height = 576;
            }
            if output.is_none() {
                output = Some(self.output(packet)?);
            }
        }
        let frame = match output {
            None => return Ok(()),
            Some(Output::Bitmaps(frame)) => frame,
            Some(Output::Blank { pts, end_display_time, width, height }) => {
                let blank = Subtitle { pts, start_display_time: 0, end_display_time, rects: Vec::new() };
                self.canvas.render(blank, packet, width, height)?
            }
        };
        self.canvas.queue(frame);
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        self.canvas.pop().ok_or(Error::NeedMore)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        // Like FFmpeg (no flush callback), retain the acquired page state.
        self.canvas.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_pixel_string_depths_and_non_modifying_color() {
        for (depth, bytes, expected) in [
            (2, &[0x6c, 0x00][..], &[1, 2, 3][..]),
            (4, &[0x12, 0x30, 0x00][..], &[1, 2, 3][..]),
            (8, &[1, 2, 3, 0, 0][..], &[1, 2, 3][..]),
        ] {
            let mut row = [9; 8];
            let (x, used) = read_string(&mut row, 0, bytes, depth, false, None);
            assert_eq!(x, 3);
            assert_eq!(used, bytes.len());
            assert_eq!(&row[..3], expected);
            assert_eq!(&row[3..], &[9; 5]);
            let mut row = [9; 8];
            let (x, _) = read_string(&mut row, 0, bytes, depth, true, None);
            assert_eq!(x, 3);
            assert_eq!(row, [2, 3, 9, 9, 9, 9, 9, 9]);
        }
        let mut row = [0; 8];
        read_string(&mut row, 0, &[0x6c, 0], 2, false, Some(&[0, 7, 8, 15]));
        assert_eq!(&row[..3], &[7, 8, 15]);
    }

    #[test]
    fn region_update_keeps_pixels_and_shrink_releases_capacity() {
        let mut ctx = Context::new(&CodecParameters::subtitle(CodecId::new("dvb_subtitle")));
        ctx.region(&[1, 0, 0, 4, 0, 4, 8, 0, 0, 0x30]).unwrap();
        let region = ctx.regions.get_mut(&1).unwrap();
        assert_eq!(region.pixels, [3; 16]);
        region.pixels[0] = 2;
        ctx.region(&[1, 0, 0, 4, 0, 4, 8, 0, 0, 0x30]).unwrap();
        assert_eq!(ctx.regions[&1].pixels[0], 2);
        ctx.region(&[1, 0, 0, 2, 0, 2, 8, 0, 0, 0x30]).unwrap();
        assert_eq!(ctx.regions[&1].pixels, [3; 4]);
        assert_eq!(ctx.regions[&1].pixels.capacity(), 4);
        assert_eq!(ctx.pixel_bytes, 4);
    }

    #[test]
    fn missing_clut_uses_ffmpeg_edge_order_and_alpha() {
        let mut region = Region {
            width: 4, height: 4,
            pixels: vec![0, 0, 0, 0, 0, 1, 1, 0, 0, 1, 1, 0, 0, 0, 0, 0],
            ..Region::default()
        };
        region.compute_clut(&mut vec![0; 257 * 256]);
        let palette = region.computed.as_ref().unwrap();
        assert_eq!(palette[0], [0, 0, 0, 0]);
        assert_eq!(palette[1], [127, 255, 127, 255]);
        assert!(region.computed_valid);
    }
}
