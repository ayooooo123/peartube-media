//! MIDI macros (IT `SFx`/`Zxx`), ported from libopenmpt 0.8.9
//! `soundlib/MIDIMacros.h/.cpp`, `MIDIMacroParser.cpp` and `MIDIEvents.cpp`.
//! Only the internal device (filter cutoff, resonance and mode) has an
//! effect here: there are no plugins to send other messages to.
//!
//! Copyright (c) 2004-2026, OpenMPT Project Developers and Contributors;
//! Copyright (c) 1997-2003, Olivier Lapicque. BSD-3-Clause (see LICENSE).

#![allow(dead_code)]

pub const MACRO_LENGTH: usize = 32;
pub type Macro = [u8; MACRO_LENGTH];

#[derive(Clone, Debug)]
pub struct MidiMacroConfig {
    pub global: [Macro; 9],
    pub sfx: [Macro; 16],
    pub zxx: [Macro; 128],
}

fn make(s: &str) -> Macro {
    let mut m = [0u8; MACRO_LENGTH];
    let b = s.as_bytes();
    let n = b.len().min(MACRO_LENGTH - 1);
    m[..n].copy_from_slice(&b[..n]);
    m
}

impl Default for MidiMacroConfig {
    /// `MIDIMacroConfig::Reset`.
    fn default() -> Self {
        let mut c = MidiMacroConfig { global: [[0; MACRO_LENGTH]; 9], sfx: [[0; MACRO_LENGTH]; 16], zxx: [[0; MACRO_LENGTH]; 128] };
        c.global[0] = make("FF");
        c.global[1] = make("FC");
        c.global[2] = make("9c n v");
        c.global[3] = make("9c n 0");
        c.global[4] = make("Cc p");
        c.sfx[0] = make("F0F000z");
        for i in 0..16 {
            c.zxx[i] = make(&format!("F0F001{:02X}", i * 8));
        }
        c
    }
}

pub fn macro_length(m: &Macro) -> usize {
    m.iter().position(|&c| c == 0).unwrap_or(MACRO_LENGTH)
}

/// `Macro::Sanitize`.
pub fn sanitize_macro(m: &mut Macro) {
    m[MACRO_LENGTH - 1] = 0;
    let len = macro_length(m);
    for c in m[len..].iter_mut() {
        *c = 0;
    }
    for c in m[..len].iter_mut() {
        if *c < 32 || *c >= 127 {
            *c = b' ';
        }
    }
}

/// `Macro::UpgradeLegacyMacro`.
pub fn upgrade_legacy_macro(m: &mut Macro) {
    for c in m.iter_mut() {
        if (b'a'..=b'f').contains(c) {
            *c = *c - b'a' + b'A';
        } else if *c == b'K' || *c == b'k' {
            *c = b'c';
        } else if matches!(*c, b'X' | b'x' | b'Y' | b'y') {
            *c = b'z';
        }
    }
}

impl MidiMacroConfig {
    pub fn clear_zxx(&mut self) {
        self.sfx = [[0; MACRO_LENGTH]; 16];
        self.zxx = [[0; MACRO_LENGTH]; 128];
    }
    pub fn sanitize(&mut self) {
        for m in self.global.iter_mut().chain(self.sfx.iter_mut()).chain(self.zxx.iter_mut()) {
            sanitize_macro(m);
        }
    }
    pub fn upgrade_macros(&mut self) {
        for m in self.sfx.iter_mut().chain(self.zxx.iter_mut()) {
            upgrade_legacy_macro(m);
        }
    }
}

/// Values the macro variables stand for on one channel.
pub struct MacroVars {
    pub midi_channel: u8,
    pub last_note: u8,
    pub velocity: u8,
    pub calc_volume: u8,
    pub pan: u8,
    pub real_pan: u8,
    pub offset: u8,
    pub host_channel: u8,
    pub loop_dir: u8,
    pub bank_hi: u8,
    pub bank_lo: u8,
    pub program: u8,
}

/// The constructor of `MIDIMacroParser`: turns a macro string into bytes.
/// `smooth_z` interpolates `z` for external messages; it returns the value
/// and whether `lastZxxParam` must be set to it.
pub fn parse_macro(
    m: &Macro,
    vars: &MacroVars,
    param: u8,
    mut smooth_z: Option<&mut dyn FnMut(u8) -> u8>,
    last_zxx: &mut u8,
) -> Vec<u8> {
    let len = macro_length(m);
    let mut out = vec![0u8; len + 1];
    let mut update_zxx: u8 = 0xFF;
    let mut first_nibble = true;
    let mut out_pos = 0usize;
    let mut pos = 0;
    while pos < len && out_pos < out.len() {
        let ch = m[pos];
        pos += 1;
        let mut is_nibble = false;
        let mut data: u8 = 0;
        match ch {
            b'0'..=b'9' => {
                is_nibble = true;
                data = ch - b'0';
            }
            b'A'..=b'F' => {
                is_nibble = true;
                data = ch - b'A' + 0x0A;
            }
            b'c' => {
                is_nibble = true;
                data = vars.midi_channel;
            }
            b'n' => data = vars.last_note,
            b'v' => data = vars.velocity,
            b'u' => data = vars.calc_volume,
            b'x' => data = vars.pan,
            b'y' => data = vars.real_pan,
            b'a' => data = vars.bank_hi,
            b'b' => data = vars.bank_lo,
            b'o' => data = vars.offset,
            b'h' => data = vars.host_channel,
            b'm' => data = vars.loop_dir,
            b'p' => data = vars.program,
            b'z' => {
                data = param;
                let internal = out_pos >= 3 && out[out_pos - 3] == 0xF0 && out[out_pos - 2] >= 0xF0;
                match smooth_z.as_mut() {
                    Some(f) if *last_zxx < 0x80 && !internal => {
                        data = f(data);
                        *last_zxx = data;
                        update_zxx = 0x80;
                    }
                    _ => {
                        if update_zxx == 0xFF {
                            update_zxx = data;
                        }
                    }
                }
            }
            b's' => {
                if !first_nibble {
                    out_pos += 1;
                    first_nibble = true;
                }
                let mut start = out_pos;
                loop {
                    if start == 0 {
                        break;
                    }
                    start -= 1;
                    if out[start] == 0xF0 {
                        break;
                    }
                }
                if out_pos - start < 3 || out[start] != 0xF0 {
                    continue;
                }
                let checksum_start = if out[start + 3] != 0 { 5 } else { 6 };
                if out_pos - start < checksum_start {
                    continue;
                }
                for p in start + checksum_start..out_pos {
                    data = data.wrapping_add(out[p]);
                }
                data = (!data).wrapping_add(1) & 0x7F;
            }
            _ => continue,
        }
        if is_nibble {
            if first_nibble {
                out[out_pos] = data;
            } else {
                out[out_pos] = (out[out_pos] << 4) | data;
                out_pos += 1;
            }
            first_nibble = !first_nibble;
        } else {
            if !first_nibble {
                out_pos += 1;
            }
            if out_pos < out.len() {
                out[out_pos] = data;
                out_pos += 1;
            }
            first_nibble = true;
        }
    }
    if !first_nibble {
        out_pos += 1;
    }
    out_pos = out_pos.min(out.len());
    if update_zxx < 0x80 {
        *last_zxx = update_zxx;
    }
    // Add end of SysEx byte if necessary.
    let mut i = 0;
    while i < out_pos {
        if out[i] != 0xF0 {
            i += 1;
            continue;
        }
        if out_pos - i >= 4 && (out[i + 1] == 0xF0 || out[i + 1] == 0xF1) {
            i += 4;
            continue;
        }
        while i < out_pos && out[i] != 0xF7 {
            i += 1;
        }
        if i == out_pos && out_pos < out.len() {
            out[out_pos] = 0xF7;
            out_pos += 1;
        }
        i += 1;
    }
    out.truncate(out_pos);
    out
}

/// `MIDIEvents::GetEventLength`.
fn event_length(first: u8) -> usize {
    match first & 0xF0 {
        0xC0 | 0xD0 => 2,
        0xF0 => match first {
            0xF1 | 0xF3 => 2,
            0xF2 => 3,
            _ => 1,
        },
        _ => 3,
    }
}

/// `MIDIMacroParser::NextMessage` over the whole buffer (with running
/// status inserted): the messages in order.
pub fn split_messages(data: &[u8]) -> Vec<Vec<u8>> {
    let mut msgs = Vec::new();
    let mut send_pos = 0usize;
    let mut running_status: u8 = 0;
    let n = data.len();
    while send_pos < n {
        let send_len;
        if data[send_pos] == 0xF0 {
            if n - send_pos >= 4 && (data[send_pos + 1] == 0xF0 || data[send_pos + 1] == 0xF1) {
                send_len = 4;
            } else {
                let mut l = n - send_pos;
                for i in send_pos + 1..n {
                    if data[i] == 0xF7 {
                        l = i - send_pos + 1;
                        break;
                    }
                }
                send_len = l;
            }
        } else if data[send_pos] & 0x80 == 0 {
            if running_status != 0 {
                // The status byte is reinserted in front of the data bytes.
                let len = event_length(running_status).saturating_sub(1).min(n - send_pos);
                let mut msg = Vec::with_capacity(len + 1);
                msg.push(running_status);
                msg.extend_from_slice(&data[send_pos..send_pos + len]);
                send_pos += len.max(1);
                msgs.push(msg);
                continue;
            } else {
                send_pos += 1;
                continue;
            }
        } else {
            send_len = event_length(data[send_pos]).min(n - send_pos);
        }
        if send_len == 0 {
            break;
        }
        if data[send_pos] < 0xF0 {
            running_status = data[send_pos];
        }
        msgs.push(data[send_pos..send_pos + send_len].to_vec());
        send_pos += send_len;
    }
    msgs
}
