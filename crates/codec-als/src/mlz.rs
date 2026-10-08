// Masked LZ decompression, for ALS floating-point streams.
//
// Ported from FFmpeg (commit 2da55bf) libavcodec/mlz.c and mlz.h.
// Copyright (c) 2016 Umair Khan <omerjerk@gmail.com>; LGPL-2.1-or-later
// (see LICENSE).

use crate::bits::Bits;

const CODE_UNSET: i32 = -1;
const CODE_BIT_INIT: u32 = 9;
const DIC_INDEX_INIT: u32 = 512;
const DIC_INDEX_MAX: i32 = 32768;
const FLUSH_CODE: i32 = 256;
const FREEZE_CODE: i32 = 257;
const FIRST_CODE: i32 = 258;
const MAX_CODE: i32 = 32767;
const TABLE_SIZE: usize = 35023;
/// The widest code a stream can ask for before the dictionary bound makes
/// it meaningless (15 bits in a valid stream).
const MAX_CODE_BITS: u32 = 24;

#[derive(Clone, Copy)]
struct Entry {
    string_code: i32,
    parent_code: i32,
    char_code: i32,
    match_len: i32,
}

/// `MLZ`
pub(crate) struct Mlz {
    dic_code_bit: u32,
    current_dic_index_max: u32,
    bump_code: u32,
    next_code: i32,
    freeze_flag: bool,
    dict: Vec<Entry>,
}

impl Mlz {
    /// `ff_mlz_init_dict`, then `ff_mlz_flush_dict`.
    pub(crate) fn new() -> Self {
        let mut m = Self {
            dic_code_bit: CODE_BIT_INIT,
            current_dic_index_max: DIC_INDEX_INIT,
            bump_code: DIC_INDEX_INIT - 1,
            next_code: FIRST_CODE,
            freeze_flag: false,
            dict: vec![Entry { string_code: 0, parent_code: 0, char_code: 0, match_len: 0 }; TABLE_SIZE],
        };
        m.flush();
        m
    }

    /// `ff_mlz_flush_dict`
    pub(crate) fn flush(&mut self) {
        for e in &mut self.dict {
            e.string_code = CODE_UNSET;
            e.parent_code = CODE_UNSET;
            e.match_len = 0;
        }
        self.current_dic_index_max = DIC_INDEX_INIT;
        self.dic_code_bit = CODE_BIT_INIT;
        self.bump_code = self.current_dic_index_max - 1;
        self.next_code = FIRST_CODE;
        self.freeze_flag = false;
    }

    /// `set_new_entry_dict`. A damaged stream can name a parent past the
    /// table (FFmpeg reads out of bounds there); it counts as length 0.
    fn set_entry(&mut self, string_code: i32, parent_code: i32, char_code: i32) {
        let match_len = if parent_code < FIRST_CODE {
            2
        } else {
            self.dict.get(parent_code as usize).map_or(0, |p| p.match_len).wrapping_add(1)
        };
        let e = &mut self.dict[string_code as usize];
        e.parent_code = parent_code;
        e.string_code = string_code;
        e.char_code = char_code;
        e.match_len = match_len;
    }

    /// `decode_string`: the string of `string_code` into `buff`, its first
    /// character code in `first`; the bytes written.
    fn decode_string(&self, buff: &mut [u8], string_code: i32, first: &mut i32) -> usize {
        let bufsize = buff.len();
        let mut count = 0;
        let mut current = string_code;
        *first = CODE_UNSET;
        while count < bufsize {
            if current == CODE_UNSET {
                return count;
            }
            if current < FIRST_CODE {
                *first = current;
                buff[0] = current as u8;
                return count + 1;
            }
            // `current` is below the table's end: a code the stream sent
            // (below next_code) or a checked parent.
            let Some(e) = self.dict.get(current as usize) else { return count };
            let offset = e.match_len.wrapping_sub(1) as u32 as usize;
            if offset >= bufsize {
                return count;
            }
            buff[offset] = e.char_code as u8;
            count += 1;
            current = e.parent_code;
            if !(0..DIC_INDEX_MAX).contains(&current) {
                return count;
            }
            if current > FIRST_CODE {
                let p = self.dict[current as usize];
                if !(0..DIC_INDEX_MAX).contains(&p.parent_code) {
                    return count;
                }
                if p.match_len.wrapping_sub(1) as u32 > (DIC_INDEX_MAX - 1) as u32 {
                    return count;
                }
            }
        }
        count
    }

    /// `ff_mlz_decompression`: up to `buff.len()` bytes; the count decoded.
    pub(crate) fn decompress(&mut self, gb: &mut Bits, buff: &mut [u8]) -> usize {
        let size = buff.len();
        let mut char_code: i32 = -1;
        let mut last_string_code: i32 = -1;
        let mut output = 0usize;
        while output < size {
            // input_code: the bits least significant first.
            let mut string_code: i32 = 0;
            for i in 0..self.dic_code_bit {
                string_code |= (gb.bit() as i32) << i;
            }
            match string_code {
                FLUSH_CODE | MAX_CODE => {
                    self.flush();
                    char_code = -1;
                    last_string_code = -1;
                }
                FREEZE_CODE => self.freeze_flag = true,
                _ => {
                    if string_code as u32 > self.current_dic_index_max {
                        return output;
                    }
                    if string_code as u32 == self.bump_code {
                        if self.dic_code_bit >= MAX_CODE_BITS {
                            return output;
                        }
                        self.dic_code_bit += 1;
                        self.current_dic_index_max *= 2;
                        self.bump_code = self.current_dic_index_max - 1;
                    } else {
                        if string_code >= self.next_code {
                            let mut first = char_code;
                            let n = self.decode_string(&mut buff[output..], last_string_code, &mut first);
                            output += n;
                            let n = self.decode_string(&mut buff[output..], first, &mut first);
                            char_code = first;
                            output += n;
                            self.set_entry(self.next_code, last_string_code, char_code);
                            if self.next_code >= TABLE_SIZE as i32 - 1 {
                                return output;
                            }
                            self.next_code += 1;
                        } else {
                            let mut first = char_code;
                            let n = self.decode_string(&mut buff[output..], string_code, &mut first);
                            char_code = first;
                            output += n;
                            if output <= size && !self.freeze_flag {
                                if last_string_code != -1 {
                                    self.set_entry(self.next_code, last_string_code, char_code);
                                    if self.next_code >= TABLE_SIZE as i32 - 1 {
                                        return output;
                                    }
                                    self.next_code += 1;
                                }
                            } else {
                                break;
                            }
                        }
                        last_string_code = string_code;
                    }
                }
            }
        }
        output
    }
}
