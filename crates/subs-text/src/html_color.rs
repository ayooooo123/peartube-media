// Copyright (c) 2010 Aurelien Jacobs <aurel@gnuage.org>
// Copyright (c) 2017 Clément Bœsch <u@pkh.me>
//
// Derived from FFmpeg at commit 2da55bf: libavcodec/htmlsubtitles.c and
// libavutil/parseutils.c (part of FFmpeg; its header names no copyright
// holder).
// Changed for PearTube on 2026-10-07: ported to safe Rust and modified.
//
// This file is free software; you can redistribute it and/or
// modify it under the terms of the GNU Lesser General Public
// License as published by the Free Software Foundation; either
// version 2.1 of the License, or (at your option) any later version.
//
// This file is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
// Lesser General Public License for more details.
//
// You should have received a copy of the GNU Lesser General Public
// License along with this file (crates/subs-text/LICENSE); if not,
// write to the Free Software Foundation, Inc., 51 Franklin Street,
// Fifth Floor, Boston, MA 02110-1301 USA

//! HTML colour names and hex forms as SubRip `<font color=...>` uses them.
//!
//! Ported to safe Rust from FFmpeg's `libavutil/parseutils.c`
//! (`av_parse_color` and its colour table) and `libavcodec/htmlsubtitles.c`
//! (`html_color_parse`) at commit 2da55bf (LGPL-2.1-or-later — headers
//! verified).

const COLOR_TABLE: [(&str, [u8; 3]); 140] = [
    ("AliceBlue", [0xF0, 0xF8, 0xFF]),
    ("AntiqueWhite", [0xFA, 0xEB, 0xD7]),
    ("Aqua", [0x00, 0xFF, 0xFF]),
    ("Aquamarine", [0x7F, 0xFF, 0xD4]),
    ("Azure", [0xF0, 0xFF, 0xFF]),
    ("Beige", [0xF5, 0xF5, 0xDC]),
    ("Bisque", [0xFF, 0xE4, 0xC4]),
    ("Black", [0x00, 0x00, 0x00]),
    ("BlanchedAlmond", [0xFF, 0xEB, 0xCD]),
    ("Blue", [0x00, 0x00, 0xFF]),
    ("BlueViolet", [0x8A, 0x2B, 0xE2]),
    ("Brown", [0xA5, 0x2A, 0x2A]),
    ("BurlyWood", [0xDE, 0xB8, 0x87]),
    ("CadetBlue", [0x5F, 0x9E, 0xA0]),
    ("Chartreuse", [0x7F, 0xFF, 0x00]),
    ("Chocolate", [0xD2, 0x69, 0x1E]),
    ("Coral", [0xFF, 0x7F, 0x50]),
    ("CornflowerBlue", [0x64, 0x95, 0xED]),
    ("Cornsilk", [0xFF, 0xF8, 0xDC]),
    ("Crimson", [0xDC, 0x14, 0x3C]),
    ("Cyan", [0x00, 0xFF, 0xFF]),
    ("DarkBlue", [0x00, 0x00, 0x8B]),
    ("DarkCyan", [0x00, 0x8B, 0x8B]),
    ("DarkGoldenRod", [0xB8, 0x86, 0x0B]),
    ("DarkGray", [0xA9, 0xA9, 0xA9]),
    ("DarkGreen", [0x00, 0x64, 0x00]),
    ("DarkKhaki", [0xBD, 0xB7, 0x6B]),
    ("DarkMagenta", [0x8B, 0x00, 0x8B]),
    ("DarkOliveGreen", [0x55, 0x6B, 0x2F]),
    ("Darkorange", [0xFF, 0x8C, 0x00]),
    ("DarkOrchid", [0x99, 0x32, 0xCC]),
    ("DarkRed", [0x8B, 0x00, 0x00]),
    ("DarkSalmon", [0xE9, 0x96, 0x7A]),
    ("DarkSeaGreen", [0x8F, 0xBC, 0x8F]),
    ("DarkSlateBlue", [0x48, 0x3D, 0x8B]),
    ("DarkSlateGray", [0x2F, 0x4F, 0x4F]),
    ("DarkTurquoise", [0x00, 0xCE, 0xD1]),
    ("DarkViolet", [0x94, 0x00, 0xD3]),
    ("DeepPink", [0xFF, 0x14, 0x93]),
    ("DeepSkyBlue", [0x00, 0xBF, 0xFF]),
    ("DimGray", [0x69, 0x69, 0x69]),
    ("DodgerBlue", [0x1E, 0x90, 0xFF]),
    ("FireBrick", [0xB2, 0x22, 0x22]),
    ("FloralWhite", [0xFF, 0xFA, 0xF0]),
    ("ForestGreen", [0x22, 0x8B, 0x22]),
    ("Fuchsia", [0xFF, 0x00, 0xFF]),
    ("Gainsboro", [0xDC, 0xDC, 0xDC]),
    ("GhostWhite", [0xF8, 0xF8, 0xFF]),
    ("Gold", [0xFF, 0xD7, 0x00]),
    ("GoldenRod", [0xDA, 0xA5, 0x20]),
    ("Gray", [0x80, 0x80, 0x80]),
    ("Green", [0x00, 0x80, 0x00]),
    ("GreenYellow", [0xAD, 0xFF, 0x2F]),
    ("HoneyDew", [0xF0, 0xFF, 0xF0]),
    ("HotPink", [0xFF, 0x69, 0xB4]),
    ("IndianRed", [0xCD, 0x5C, 0x5C]),
    ("Indigo", [0x4B, 0x00, 0x82]),
    ("Ivory", [0xFF, 0xFF, 0xF0]),
    ("Khaki", [0xF0, 0xE6, 0x8C]),
    ("Lavender", [0xE6, 0xE6, 0xFA]),
    ("LavenderBlush", [0xFF, 0xF0, 0xF5]),
    ("LawnGreen", [0x7C, 0xFC, 0x00]),
    ("LemonChiffon", [0xFF, 0xFA, 0xCD]),
    ("LightBlue", [0xAD, 0xD8, 0xE6]),
    ("LightCoral", [0xF0, 0x80, 0x80]),
    ("LightCyan", [0xE0, 0xFF, 0xFF]),
    ("LightGoldenRodYellow", [0xFA, 0xFA, 0xD2]),
    ("LightGreen", [0x90, 0xEE, 0x90]),
    ("LightGrey", [0xD3, 0xD3, 0xD3]),
    ("LightPink", [0xFF, 0xB6, 0xC1]),
    ("LightSalmon", [0xFF, 0xA0, 0x7A]),
    ("LightSeaGreen", [0x20, 0xB2, 0xAA]),
    ("LightSkyBlue", [0x87, 0xCE, 0xFA]),
    ("LightSlateGray", [0x77, 0x88, 0x99]),
    ("LightSteelBlue", [0xB0, 0xC4, 0xDE]),
    ("LightYellow", [0xFF, 0xFF, 0xE0]),
    ("Lime", [0x00, 0xFF, 0x00]),
    ("LimeGreen", [0x32, 0xCD, 0x32]),
    ("Linen", [0xFA, 0xF0, 0xE6]),
    ("Magenta", [0xFF, 0x00, 0xFF]),
    ("Maroon", [0x80, 0x00, 0x00]),
    ("MediumAquaMarine", [0x66, 0xCD, 0xAA]),
    ("MediumBlue", [0x00, 0x00, 0xCD]),
    ("MediumOrchid", [0xBA, 0x55, 0xD3]),
    ("MediumPurple", [0x93, 0x70, 0xD8]),
    ("MediumSeaGreen", [0x3C, 0xB3, 0x71]),
    ("MediumSlateBlue", [0x7B, 0x68, 0xEE]),
    ("MediumSpringGreen", [0x00, 0xFA, 0x9A]),
    ("MediumTurquoise", [0x48, 0xD1, 0xCC]),
    ("MediumVioletRed", [0xC7, 0x15, 0x85]),
    ("MidnightBlue", [0x19, 0x19, 0x70]),
    ("MintCream", [0xF5, 0xFF, 0xFA]),
    ("MistyRose", [0xFF, 0xE4, 0xE1]),
    ("Moccasin", [0xFF, 0xE4, 0xB5]),
    ("NavajoWhite", [0xFF, 0xDE, 0xAD]),
    ("Navy", [0x00, 0x00, 0x80]),
    ("OldLace", [0xFD, 0xF5, 0xE6]),
    ("Olive", [0x80, 0x80, 0x00]),
    ("OliveDrab", [0x6B, 0x8E, 0x23]),
    ("Orange", [0xFF, 0xA5, 0x00]),
    ("OrangeRed", [0xFF, 0x45, 0x00]),
    ("Orchid", [0xDA, 0x70, 0xD6]),
    ("PaleGoldenRod", [0xEE, 0xE8, 0xAA]),
    ("PaleGreen", [0x98, 0xFB, 0x98]),
    ("PaleTurquoise", [0xAF, 0xEE, 0xEE]),
    ("PaleVioletRed", [0xD8, 0x70, 0x93]),
    ("PapayaWhip", [0xFF, 0xEF, 0xD5]),
    ("PeachPuff", [0xFF, 0xDA, 0xB9]),
    ("Peru", [0xCD, 0x85, 0x3F]),
    ("Pink", [0xFF, 0xC0, 0xCB]),
    ("Plum", [0xDD, 0xA0, 0xDD]),
    ("PowderBlue", [0xB0, 0xE0, 0xE6]),
    ("Purple", [0x80, 0x00, 0x80]),
    ("Red", [0xFF, 0x00, 0x00]),
    ("RosyBrown", [0xBC, 0x8F, 0x8F]),
    ("RoyalBlue", [0x41, 0x69, 0xE1]),
    ("SaddleBrown", [0x8B, 0x45, 0x13]),
    ("Salmon", [0xFA, 0x80, 0x72]),
    ("SandyBrown", [0xF4, 0xA4, 0x60]),
    ("SeaGreen", [0x2E, 0x8B, 0x57]),
    ("SeaShell", [0xFF, 0xF5, 0xEE]),
    ("Sienna", [0xA0, 0x52, 0x2D]),
    ("Silver", [0xC0, 0xC0, 0xC0]),
    ("SkyBlue", [0x87, 0xCE, 0xEB]),
    ("SlateBlue", [0x6A, 0x5A, 0xCD]),
    ("SlateGray", [0x70, 0x80, 0x90]),
    ("Snow", [0xFF, 0xFA, 0xFA]),
    ("SpringGreen", [0x00, 0xFF, 0x7F]),
    ("SteelBlue", [0x46, 0x82, 0xB4]),
    ("Tan", [0xD2, 0xB4, 0x8C]),
    ("Teal", [0x00, 0x80, 0x80]),
    ("Thistle", [0xD8, 0xBF, 0xD8]),
    ("Tomato", [0xFF, 0x63, 0x47]),
    ("Turquoise", [0x40, 0xE0, 0xD0]),
    ("Violet", [0xEE, 0x82, 0xEE]),
    ("Wheat", [0xF5, 0xDE, 0xB3]),
    ("White", [0xFF, 0xFF, 0xFF]),
    ("WhiteSmoke", [0xF5, 0xF5, 0xF5]),
    ("Yellow", [0xFF, 0xFF, 0x00]),
    ("YellowGreen", [0x9A, 0xCD, 0x32]),
];

/// `av_parse_color` over the first `len` bytes of `s`: `[r, g, b]`, or
/// `None` where FFmpeg returns an error. `random`/`bikeshed` (a random
/// colour in FFmpeg) is white here.
fn parse_color(s: &[u8]) -> Option<[u8; 3]> {
    let hex_offset = if s.first() == Some(&b'#') {
        1
    } else if s.starts_with(b"0x") {
        2
    } else {
        0
    };
    let mut name = &s[hex_offset.min(s.len())..];
    name = &name[..name.len().min(127)];
    let alpha = name.iter().position(|&b| b == b'@').map(|at| {
        let tail = &name[at + 1..];
        name = &name[..at];
        tail
    });
    let rgb = if name.eq_ignore_ascii_case(b"random") || name.eq_ignore_ascii_case(b"bikeshed") {
        [0xFF, 0xFF, 0xFF]
    } else if hex_offset > 0 || name.iter().all(u8::is_ascii_hexdigit) {
        // strtoul(name, &tail, 16): the whole string must be hex, of 6 or 8.
        let (value, used) = crate::scan::strtol_hex(name);
        if used != name.len() || (name.len() != 6 && name.len() != 8) {
            return None;
        }
        let value = if name.len() == 8 { (value as u64 >> 8) as u32 } else { value as u32 };
        [(value >> 16) as u8, (value >> 8) as u8, value as u8]
    } else {
        COLOR_TABLE.iter().find(|(n, _)| n.as_bytes().eq_ignore_ascii_case(name))?.1
    };
    if let Some(alpha) = alpha {
        let valid = if let Some(hex) = alpha.strip_prefix(b"0x") {
            let (value, used) = crate::scan::strtol_hex(hex);
            used == hex.len() && !hex.is_empty() && (0..=255).contains(&value)
        } else {
            std::str::from_utf8(alpha).ok().and_then(|a| a.parse::<f64>().ok()).is_some_and(|a| (0.0..=1.0).contains(&a))
        };
        if !valid {
            return None;
        }
    }
    Some(rgb)
}

/// `html_color_parse`: the colour of a `color=` attribute value (up to `"`,
/// space or `>`), as `0xBBGGRR`; `None` when FFmpeg rejects it.
pub fn html_color(value: &[u8]) -> Option<u32> {
    let sharps = value.iter().take_while(|&&b| b == b'#').count();
    let value = &value[sharps.saturating_sub(1)..];
    let len = crate::scan::strcspn(value, b"\" >");
    let [r, g, b] = parse_color(&value[..len])?;
    Some(u32::from(r) | u32::from(g) << 8 | u32::from(b) << 16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_parse_like_ffmpeg() {
        assert_eq!(html_color(b"red\">"), Some(0x0000ff));
        assert_eq!(html_color(b"##FFFF00\">"), Some(0x00ffff));
        assert_eq!(html_color(b"00ff00\""), Some(0x00ff00));
        assert_eq!(html_color(b"brown size=24>"), Some(0x2a2aa5));
        assert_eq!(html_color(b"#12345\""), None);
        assert_eq!(html_color(b"nocolor"), None);
    }
}
