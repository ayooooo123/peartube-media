// Port of FFmpeg's ATRAC3+ bitstream parser (libavcodec/atrac3plus.c,
// FFmpeg commit 2da55bf).
// Copyright (c) 2010-2013 Maxim Poliakovski; LGPL-2.1-or-later (see LICENSE).

use oxideav_core::{Error, Result};

use super::tables::*;
use super::{ChanParams, ChanUnit, POWER_COMP_OFF, SUBBANDS, VLCS, WaveParam, WavesData};
use crate::bits::BitReader;
use crate::common::GainInfo;
use crate::vlc::Vlc;

fn invalid(what: &str) -> Error {
    Error::invalid(format!("atrac3plus: {what}"))
}

/// `sign_extend(val, bits)`.
fn sign_extend(val: i32, bits: u32) -> i32 {
    ((val as u32) << (32 - bits)) as i32 >> (32 - bits)
}

/// `av_log2` of a positive value (0 for 0).
fn log2(v: i32) -> u32 {
    31 - (v.max(1) as u32).leading_zeros()
}

/// `get_bitsz`: zero bits read nothing.
fn get_bitsz(gb: &mut BitReader, n: u32) -> i32 {
    if n == 0 { 0 } else { gb.geti(n) }
}

/// Both channels of a unit: channel `ch_num` and channel 0, its reference.
/// For channel 0 the reference is a copy of itself (FFmpeg reads it only
/// for channel 1).
fn chan_and_ref(ctx: &mut ChanUnit, ch_num: usize) -> (&mut ChanParams, ChanParams) {
    let reference = ctx.channels[0].clone();
    (&mut ctx.channels[ch_num], reference)
}

/// `num_coded_units`.
fn num_coded_units(gb: &mut BitReader, chan: &mut ChanParams, num_quant_units: i32) -> Result<()> {
    chan.fill_mode = gb.geti(2);
    if chan.fill_mode == 0 {
        chan.num_coded_vals = num_quant_units;
    } else {
        chan.num_coded_vals = gb.geti(5);
        if chan.num_coded_vals > num_quant_units {
            return Err(invalid("invalid number of transmitted units"));
        }
        if chan.fill_mode == 3 {
            chan.split_point = gb.geti(2) + ((chan.ch_num as i32) << 1) + 1;
        }
    }
    Ok(())
}

/// `add_wordlen_weights`.
fn add_wordlen_weights(num_quant_units: i32, chan: &mut ChanParams, wtab_idx: i32) -> Result<()> {
    let weights = &WL_WEIGHTS[chan.ch_num * 3 + wtab_idx as usize - 1];
    for i in 0..num_quant_units as usize {
        chan.qu_wordlen[i] += i32::from(weights[i]);
        if !(0..=7).contains(&chan.qu_wordlen[i]) {
            return Err(invalid("WL index out of range"));
        }
    }
    Ok(())
}

/// `subtract_sf_weights`.
fn subtract_sf_weights(used_quant_units: i32, chan: &mut ChanParams, wtab_idx: i32) -> Result<()> {
    let weights = &SF_WEIGHTS[wtab_idx as usize - 1];
    for i in 0..used_quant_units as usize {
        chan.qu_sf_idx[i] -= i32::from(weights[i]);
        if !(0..=63).contains(&chan.qu_sf_idx[i]) {
            return Err(invalid("SF index out of range"));
        }
    }
    Ok(())
}

/// `unpack_vq_shape`.
fn unpack_vq_shape(start_val: i32, shape_vec: &[i8; 9], dst: &mut [i32; 32], num_values: i32) {
    if num_values > 0 {
        dst[0] = start_val;
        dst[1] = start_val;
        dst[2] = start_val;
        for i in 3..num_values as usize {
            dst[i] = start_val - i32::from(shape_vec[usize::from(QU_NUM_TO_SEG[i]) - 1]);
        }
    }
}

/// `UNPACK_SF_VQ_SHAPE`.
fn unpack_sf_vq_shape(gb: &mut BitReader, dst: &mut [i32; 32], num_vals: i32) {
    let start_val = gb.geti(6);
    let shape = &SF_SHAPES[gb.get(6) as usize];
    unpack_vq_shape(start_val, shape, dst, num_vals);
}

/// `decode_channel_wordlen`.
fn decode_channel_wordlen(gb: &mut BitReader, ctx: &mut ChanUnit, ch_num: usize) -> Result<()> {
    let vlcs = &*VLCS;
    let nqu = ctx.num_quant_units;
    let (chan, ref_chan) = chan_and_ref(ctx, ch_num);
    let mut weight_idx = 0;
    chan.fill_mode = 0;

    match gb.get(2) {
        0 => {
            // constant number of bits
            for i in 0..nqu as usize {
                chan.qu_wordlen[i] = gb.geti(3);
            }
        }
        1 => {
            if ch_num != 0 {
                num_coded_units(gb, chan, nqu)?;
                if chan.num_coded_vals != 0 {
                    let vlc = &vlcs.wl[gb.get(2) as usize];
                    for i in 0..chan.num_coded_vals as usize {
                        let delta = vlc.get(gb);
                        chan.qu_wordlen[i] = (ref_chan.qu_wordlen[i] + delta) & 7;
                    }
                }
            } else {
                weight_idx = gb.geti(2);
                num_coded_units(gb, chan, nqu)?;
                if chan.num_coded_vals != 0 {
                    let pos = gb.geti(5);
                    if pos > chan.num_coded_vals {
                        return Err(invalid("WL mode 1: invalid position"));
                    }
                    let delta_bits = gb.get(2);
                    let min_val = gb.geti(3);
                    for i in 0..pos as usize {
                        chan.qu_wordlen[i] = gb.geti(3);
                    }
                    for i in pos as usize..chan.num_coded_vals as usize {
                        chan.qu_wordlen[i] = (min_val + get_bitsz(gb, delta_bits)) & 7;
                    }
                }
            }
        }
        2 => {
            num_coded_units(gb, chan, nqu)?;
            if ch_num != 0 && chan.num_coded_vals != 0 {
                let vlc = &vlcs.wl[gb.get(2) as usize];
                let delta = vlc.get(gb);
                chan.qu_wordlen[0] = (ref_chan.qu_wordlen[0] + delta) & 7;
                for i in 1..chan.num_coded_vals as usize {
                    let diff = ref_chan.qu_wordlen[i] - ref_chan.qu_wordlen[i - 1];
                    let delta = vlc.get(gb);
                    chan.qu_wordlen[i] = (chan.qu_wordlen[i - 1] + diff + delta) & 7;
                }
            } else if chan.num_coded_vals != 0 {
                let flag = gb.get1();
                let vlc = &vlcs.wl[gb.get1() as usize];
                let start_val = gb.geti(3);
                let shape = &WL_SHAPES[start_val as usize][gb.get(4) as usize];
                let ncv = chan.num_coded_vals;
                unpack_vq_shape(start_val, shape, &mut chan.qu_wordlen, ncv);
                if flag == 0 {
                    for i in 0..ncv as usize {
                        let delta = vlc.get(gb);
                        chan.qu_wordlen[i] = (chan.qu_wordlen[i] + delta) & 7;
                    }
                } else {
                    let mut i = 0usize;
                    while i < (ncv & -2) as usize {
                        if gb.get1() == 0 {
                            chan.qu_wordlen[i] = (chan.qu_wordlen[i] + vlc.get(gb)) & 7;
                            chan.qu_wordlen[i + 1] = (chan.qu_wordlen[i + 1] + vlc.get(gb)) & 7;
                        }
                        i += 2;
                    }
                    if ncv & 1 != 0 {
                        chan.qu_wordlen[i] = (chan.qu_wordlen[i] + vlc.get(gb)) & 7;
                    }
                }
            }
        }
        _ => {
            weight_idx = gb.geti(2);
            num_coded_units(gb, chan, nqu)?;
            if chan.num_coded_vals != 0 {
                let vlc = &vlcs.wl[gb.get(2) as usize];
                // the first coefficient is coded directly
                chan.qu_wordlen[0] = gb.geti(3);
                for i in 1..chan.num_coded_vals as usize {
                    let delta = vlc.get(gb);
                    chan.qu_wordlen[i] = (chan.qu_wordlen[i - 1] + delta) & 7;
                }
            }
        }
    }

    if chan.fill_mode == 2 {
        for i in chan.num_coded_vals.max(0) as usize..nqu as usize {
            chan.qu_wordlen[i] = if ch_num != 0 { gb.geti(1) } else { 1 };
        }
    } else if chan.fill_mode == 3 {
        let pos = if ch_num != 0 {
            chan.num_coded_vals + chan.split_point
        } else {
            nqu - chan.split_point
        };
        let pos = pos.min(32);
        for i in chan.num_coded_vals.max(0)..pos {
            chan.qu_wordlen[i as usize] = 1;
        }
    }

    if weight_idx != 0 {
        return add_wordlen_weights(nqu, chan, weight_idx);
    }
    Ok(())
}

/// `decode_channel_sf_idx`.
fn decode_channel_sf_idx(gb: &mut BitReader, ctx: &mut ChanUnit, ch_num: usize) -> Result<()> {
    let vlcs = &*VLCS;
    let used = ctx.used_quant_units;
    let (chan, ref_chan) = chan_and_ref(ctx, ch_num);
    let mut weight_idx = 0;

    match gb.get(2) {
        0 => {
            for i in 0..used as usize {
                chan.qu_sf_idx[i] = gb.geti(6);
            }
        }
        1 => {
            if ch_num != 0 {
                let vlc = &vlcs.sf[gb.get(2) as usize];
                for i in 0..used as usize {
                    let delta = vlc.get(gb);
                    chan.qu_sf_idx[i] = (ref_chan.qu_sf_idx[i] + delta) & 0x3F;
                }
            } else {
                weight_idx = gb.geti(2);
                if weight_idx == 3 {
                    unpack_sf_vq_shape(gb, &mut chan.qu_sf_idx, used);
                    let num_long_vals = gb.geti(5);
                    let delta_bits = gb.get(2);
                    let min_val = gb.geti(4) - 7;
                    for i in 0..num_long_vals as usize {
                        chan.qu_sf_idx[i] = (chan.qu_sf_idx[i] + gb.geti(4) - 7) & 0x3F;
                    }
                    // all others are min_val + delta
                    for i in num_long_vals as usize..used.max(0) as usize {
                        chan.qu_sf_idx[i] =
                            (chan.qu_sf_idx[i] + min_val + get_bitsz(gb, delta_bits)) & 0x3F;
                    }
                } else {
                    let num_long_vals = gb.geti(5);
                    let delta_bits = gb.get(3);
                    let min_val = gb.geti(6);
                    if num_long_vals > used || delta_bits == 7 {
                        return Err(invalid("SF mode 1: invalid parameters"));
                    }
                    // full-precision SF indexes
                    for i in 0..num_long_vals as usize {
                        chan.qu_sf_idx[i] = gb.geti(6);
                    }
                    for i in num_long_vals as usize..used as usize {
                        chan.qu_sf_idx[i] = (min_val + get_bitsz(gb, delta_bits)) & 0x3F;
                    }
                }
            }
        }
        2 => {
            if ch_num != 0 {
                let vlc = &vlcs.sf[gb.get(2) as usize];
                let delta = vlc.get(gb);
                chan.qu_sf_idx[0] = (ref_chan.qu_sf_idx[0] + delta) & 0x3F;
                for i in 1..used as usize {
                    let diff = ref_chan.qu_sf_idx[i] - ref_chan.qu_sf_idx[i - 1];
                    let delta = vlc.get(gb);
                    chan.qu_sf_idx[i] = (chan.qu_sf_idx[i - 1] + diff + delta) & 0x3F;
                }
            } else {
                let vlc = &vlcs.sf[gb.get(2) as usize + 4];
                unpack_sf_vq_shape(gb, &mut chan.qu_sf_idx, used);
                for i in 0..used as usize {
                    let delta = vlc.get(gb);
                    chan.qu_sf_idx[i] = (chan.qu_sf_idx[i] + sign_extend(delta, 4)) & 0x3F;
                }
            }
        }
        _ => {
            if ch_num != 0 {
                // copy the reference channel's coefficients
                chan.qu_sf_idx[..used as usize]
                    .copy_from_slice(&ref_chan.qu_sf_idx[..used as usize]);
            } else {
                weight_idx = gb.geti(2);
                let vlc_sel = gb.get(2) as usize;
                if weight_idx == 3 {
                    let vlc = &vlcs.sf[vlc_sel + 4];
                    unpack_sf_vq_shape(gb, &mut chan.qu_sf_idx, used);
                    let mut diff = (gb.geti(4) + 56) & 0x3F;
                    chan.qu_sf_idx[0] = (chan.qu_sf_idx[0] + diff) & 0x3F;
                    for i in 1..used as usize {
                        let delta = vlc.get(gb);
                        diff = (diff + sign_extend(delta, 4)) & 0x3F;
                        chan.qu_sf_idx[i] = (diff + chan.qu_sf_idx[i]) & 0x3F;
                    }
                } else {
                    let vlc = &vlcs.sf[vlc_sel];
                    // the first coefficient is coded directly
                    chan.qu_sf_idx[0] = gb.geti(6);
                    for i in 1..used as usize {
                        let delta = vlc.get(gb);
                        chan.qu_sf_idx[i] = (chan.qu_sf_idx[i - 1] + delta) & 0x3F;
                    }
                }
            }
        }
    }

    if weight_idx != 0 && weight_idx < 3 {
        return subtract_sf_weights(used, chan, weight_idx);
    }
    Ok(())
}

/// `decode_quant_wordlen`.
fn decode_quant_wordlen(gb: &mut BitReader, ctx: &mut ChanUnit, num_channels: usize) -> Result<()> {
    for ch_num in 0..num_channels {
        ctx.channels[ch_num].qu_wordlen = [0; 32];
        decode_channel_wordlen(gb, ctx, ch_num)?;
    }
    // the number of quant units with coded spectrum
    let mut i = ctx.num_quant_units - 1;
    while i >= 0 {
        let k = i as usize;
        if ctx.channels[0].qu_wordlen[k] != 0
            || (num_channels == 2 && ctx.channels[1].qu_wordlen[k] != 0)
        {
            break;
        }
        i -= 1;
    }
    ctx.used_quant_units = i + 1;
    Ok(())
}

/// `decode_scale_factors`.
fn decode_scale_factors(gb: &mut BitReader, ctx: &mut ChanUnit, num_channels: usize) -> Result<()> {
    if ctx.used_quant_units == 0 {
        return Ok(());
    }
    for ch_num in 0..num_channels {
        ctx.channels[ch_num].qu_sf_idx = [0; 32];
        decode_channel_sf_idx(gb, ctx, ch_num)?;
    }
    Ok(())
}

/// `get_num_ct_values`.
fn get_num_ct_values(gb: &mut BitReader, used: i32) -> Result<i32> {
    if gb.get1() != 0 {
        let n = gb.geti(5);
        if n > used {
            return Err(invalid("invalid number of code table indexes"));
        }
        Ok(n)
    } else {
        Ok(used)
    }
}

/// `decode_channel_code_tab` (the `DEC_CT_IDX_COMMON` cases inlined).
fn decode_channel_code_tab(gb: &mut BitReader, ctx: &mut ChanUnit, ch_num: usize) -> Result<()> {
    let vlcs = &*VLCS;
    let full = ctx.use_full_table;
    let used = ctx.used_quant_units;
    let mask = if full { 7 } else { 3 };
    let (chan, ref_chan) = chan_and_ref(ctx, ch_num);

    chan.table_type = gb.geti(1);
    let mode = gb.get(2);
    if mode == 3 && ch_num == 0 {
        return Ok(());
    }
    let num_vals = get_num_ct_values(gb, used)?;
    let (vlc, delta_vlc): (&Vlc, &Vlc) = match (mode, full) {
        (1, true) => (&vlcs.ct[1], &vlcs.ct[1]),
        (2, true) => (&vlcs.ct[1], &vlcs.ct[2]),
        (3, true) => (&vlcs.ct[3], &vlcs.ct[3]),
        _ => (&vlcs.ct[0], &vlcs.ct[0]),
    };
    let num_bits = u32::from(full) + 2;
    let mut pred = 0;
    for i in 0..num_vals as usize {
        if chan.qu_wordlen[i] != 0 {
            chan.qu_tab_idx[i] = match mode {
                0 => gb.geti(num_bits),
                1 => vlc.get(gb),
                2 => {
                    let v = if i == 0 {
                        vlc.get(gb)
                    } else {
                        (pred + delta_vlc.get(gb)) & mask
                    };
                    pred = v;
                    v
                }
                _ => (ref_chan.qu_tab_idx[i] + vlc.get(gb)) & mask,
            };
        } else if ch_num != 0 && ref_chan.qu_wordlen[i] != 0 {
            // clone master flag
            chan.qu_tab_idx[i] = gb.geti(1);
        }
    }
    Ok(())
}

/// `decode_code_table_indexes`.
fn decode_code_table_indexes(
    gb: &mut BitReader,
    ctx: &mut ChanUnit,
    num_channels: usize,
) -> Result<()> {
    if ctx.used_quant_units == 0 {
        return Ok(());
    }
    ctx.use_full_table = gb.get1() != 0;
    for ch_num in 0..num_channels {
        ctx.channels[ch_num].qu_tab_idx = [0; 32];
        decode_channel_code_tab(gb, ctx, ch_num)?;
    }
    Ok(())
}

/// `decode_qu_spectra`.
fn decode_qu_spectra(
    gb: &mut BitReader,
    tab: &[u8; 4],
    vlc: &Vlc,
    out: &mut [i16],
    num_specs: usize,
) {
    let (group_size, num_coeffs, bits, is_signed) = (
        usize::from(tab[0]),
        usize::from(tab[1]),
        u32::from(tab[2]),
        tab[3] != 0,
    );
    let mut pos = 0usize;
    while pos < num_specs {
        if group_size == 1 || gb.get1() != 0 {
            for _ in 0..group_size {
                let mut val = vlc.get(gb) as u32;
                for _ in 0..num_coeffs {
                    let mut cf = (val & ((1u32 << bits) - 1)) as i32;
                    if is_signed {
                        cf = sign_extend(cf, bits);
                    } else if cf != 0 && gb.get1() != 0 {
                        cf = -cf;
                    }
                    if let Some(o) = out.get_mut(pos) {
                        *o = cf as i16;
                    }
                    pos += 1;
                    val >>= bits;
                }
            }
        } else {
            // group skipped
            pos += group_size * num_coeffs;
        }
    }
}

/// `decode_spectrum`.
fn decode_spectrum(gb: &mut BitReader, ctx: &mut ChanUnit, num_channels: usize) -> Result<()> {
    let vlcs = &*VLCS;
    let used = ctx.used_quant_units as usize;
    for ch_num in 0..num_channels {
        let (first, rest) = ctx.channels.split_at_mut(1);
        // channel 1 copies spectra from channel 0, the master
        let (chan, master) = if ch_num == 0 {
            (&mut first[0], None)
        } else {
            (&mut rest[0], Some(&first[0]))
        };
        chan.spectrum = [0; 2048];
        // power compensation off
        chan.power_levs = [POWER_COMP_OFF; 5];

        for qu in 0..used {
            let start = usize::from(QU_TO_SPEC_POS[qu]);
            let num_specs = usize::from(QU_TO_SPEC_POS[qu + 1]) - start;
            let wordlen = chan.qu_wordlen[qu];
            let mut codetab = chan.qu_tab_idx[qu];
            if wordlen != 0 {
                if !ctx.use_full_table {
                    codetab = i32::from(
                        *CT_RESTRICTED_TO_FULL
                            .get(chan.table_type as usize)
                            .and_then(|t| t.get(wordlen as usize - 1))
                            .and_then(|t| t.get(usize::try_from(codetab).ok()?))
                            .ok_or_else(|| invalid("invalid code table index"))?,
                    );
                }
                let tab_index = usize::try_from((chan.table_type * 8 + codetab) * 7 + wordlen - 1)
                    .ok()
                    .filter(|&t| t < SPECTRA_TABS.len())
                    .ok_or_else(|| invalid("invalid code table index"))?;
                decode_qu_spectra(
                    gb,
                    &SPECTRA_TABS[tab_index],
                    vlcs.spec(tab_index),
                    &mut chan.spectrum[start..],
                    num_specs,
                );
            } else if let Some(master) = master.filter(|m| m.qu_wordlen[qu] != 0 && codetab == 0) {
                // copy the coefficients from the master
                chan.spectrum[start..start + num_specs]
                    .copy_from_slice(&master.spectrum[start..start + num_specs]);
                chan.qu_wordlen[qu] = master.qu_wordlen[qu];
            }
        }

        // power compensation levels are coded for more than 2 quant units
        if used > 2 {
            let num = SUBBAND_TO_NUM_POWGRPS[ctx.num_coded_subbands as usize - 1] as usize;
            for i in 0..num {
                chan.power_levs[i] = gb.get(4) as u8;
            }
        }
    }
    Ok(())
}

/// `get_subband_flags`.
fn get_subband_flags(gb: &mut BitReader, out: &mut [u8; SUBBANDS], num_flags: usize) -> bool {
    let num_flags = num_flags.min(SUBBANDS);
    out[..num_flags].fill(0);
    let result = gb.get1() != 0;
    if result {
        if gb.get1() != 0 {
            for o in &mut out[..num_flags] {
                *o = gb.get1() as u8;
            }
        } else {
            out[..num_flags].fill(1);
        }
    }
    result
}

/// `decode_window_shape`.
fn decode_window_shape(gb: &mut BitReader, ctx: &mut ChanUnit, num_channels: usize) {
    let num_subbands = ctx.num_subbands as usize;
    for chan in ctx.channels.iter_mut().take(num_channels) {
        let cur = chan.cur;
        get_subband_flags(gb, &mut chan.wnd_shape_hist[cur], num_subbands);
    }
}

/// `decode_gainc_npoints`.
fn decode_gainc_npoints(
    gb: &mut BitReader,
    chan: &mut ChanParams,
    reference: &[GainInfo; SUBBANDS],
    ch_num: usize,
    coded_subbands: usize,
) -> Result<()> {
    let vlcs = &*VLCS;
    let gain = chan.gain_data_mut();
    match gb.get(2) {
        0 => {
            for g in gain.iter_mut().take(coded_subbands) {
                g.num_points = gb.geti(3);
            }
        }
        1 => {
            for g in gain.iter_mut().take(coded_subbands) {
                g.num_points = vlcs.gain[0].get(gb);
            }
        }
        2 => {
            if ch_num != 0 {
                // VLC modulo delta to the master channel
                for i in 0..coded_subbands {
                    let delta = vlcs.gain[1].get(gb);
                    gain[i].num_points = (reference[i].num_points + delta) & 7;
                }
            } else {
                // VLC modulo delta to the previous subband
                gain[0].num_points = vlcs.gain[0].get(gb);
                for i in 1..coded_subbands {
                    let delta = vlcs.gain[1].get(gb);
                    gain[i].num_points = (gain[i - 1].num_points + delta) & 7;
                }
            }
        }
        _ => {
            if ch_num != 0 {
                // copy the master channel's
                for i in 0..coded_subbands {
                    gain[i].num_points = reference[i].num_points;
                }
            } else {
                // shorter delta to min
                let delta_bits = gb.get(2);
                let min_val = gb.geti(3);
                for g in gain.iter_mut().take(coded_subbands) {
                    g.num_points = min_val + get_bitsz(gb, delta_bits);
                    if g.num_points > 7 {
                        return Err(invalid("invalid number of gain points"));
                    }
                }
            }
        }
    }
    Ok(())
}

/// The number of a block's points as an index bound (0 for none or -1).
fn points(g: &GainInfo) -> usize {
    (g.num_points.max(0) as usize).min(7)
}

/// `gainc_level_mode3s`.
fn gainc_level_mode3s(dst: &mut GainInfo, reference: &GainInfo) {
    for i in 0..points(dst) {
        dst.lev_code[i] = if i as i32 >= reference.num_points {
            7
        } else {
            reference.lev_code[i]
        };
    }
}

/// `gainc_level_mode1m`.
fn gainc_level_mode1m(gb: &mut BitReader, dst: &mut GainInfo) {
    let vlcs = &*VLCS;
    if dst.num_points > 0 {
        dst.lev_code[0] = vlcs.gain[2].get(gb);
    }
    for i in 1..points(dst) {
        let delta = vlcs.gain[3].get(gb);
        dst.lev_code[i] = (dst.lev_code[i - 1] + delta) & 0xF;
    }
}

/// `decode_gainc_levels`.
fn decode_gainc_levels(
    gb: &mut BitReader,
    chan: &mut ChanParams,
    reference: &[GainInfo; SUBBANDS],
    ch_num: usize,
    coded_subbands: usize,
) -> Result<()> {
    let vlcs = &*VLCS;
    let gain = chan.gain_data_mut();
    match gb.get(2) {
        0 => {
            for g in gain.iter_mut().take(coded_subbands) {
                for i in 0..points(g) {
                    g.lev_code[i] = gb.geti(4);
                }
            }
        }
        1 => {
            if ch_num != 0 {
                // VLC modulo delta to the master channel
                for sb in 0..coded_subbands {
                    for i in 0..points(&gain[sb]) {
                        let delta = vlcs.gain[5].get(gb);
                        let pred = if i as i32 >= reference[sb].num_points {
                            7
                        } else {
                            reference[sb].lev_code[i]
                        };
                        gain[sb].lev_code[i] = (pred + delta) & 0xF;
                    }
                }
            } else {
                // VLC modulo delta to the previous point
                for g in gain.iter_mut().take(coded_subbands) {
                    gainc_level_mode1m(gb, g);
                }
            }
        }
        2 => {
            if ch_num != 0 {
                // VLC modulo delta to the previous point, or a master clone
                for sb in 0..coded_subbands {
                    if gain[sb].num_points > 0 {
                        if gb.get1() != 0 {
                            gainc_level_mode1m(gb, &mut gain[sb]);
                        } else {
                            gainc_level_mode3s(&mut gain[sb], &reference[sb]);
                        }
                    }
                }
            } else {
                // VLC modulo delta to the previous subband's levels
                if gain[0].num_points > 0 {
                    gainc_level_mode1m(gb, &mut gain[0]);
                }
                for sb in 1..coded_subbands {
                    for i in 0..points(&gain[sb]) {
                        let delta = vlcs.gain[4].get(gb);
                        let pred = if i as i32 >= gain[sb - 1].num_points {
                            7
                        } else {
                            gain[sb - 1].lev_code[i]
                        };
                        gain[sb].lev_code[i] = (pred + delta) & 0xF;
                    }
                }
            }
        }
        _ => {
            if ch_num != 0 {
                // clone the master
                for sb in 0..coded_subbands {
                    gainc_level_mode3s(&mut gain[sb], &reference[sb]);
                }
            } else {
                // shorter delta to min
                let delta_bits = gb.get(2);
                let min_val = gb.geti(4);
                for g in gain.iter_mut().take(coded_subbands) {
                    for i in 0..points(g) {
                        g.lev_code[i] = min_val + get_bitsz(gb, delta_bits);
                        if g.lev_code[i] > 15 {
                            return Err(invalid("invalid gain level"));
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// `gainc_loc_mode0`.
fn gainc_loc_mode0(gb: &mut BitReader, dst: &mut GainInfo, pos: usize) {
    if pos == 0 || dst.loc_code[pos - 1] < 15 {
        dst.loc_code[pos] = gb.geti(5);
    } else if dst.loc_code[pos - 1] >= 30 {
        dst.loc_code[pos] = 31;
    } else {
        let delta_bits = log2(30 - dst.loc_code[pos - 1]) + 1;
        dst.loc_code[pos] = dst.loc_code[pos - 1] + gb.geti(delta_bits) + 1;
    }
}

/// `gainc_loc_mode1`.
fn gainc_loc_mode1(gb: &mut BitReader, dst: &mut GainInfo) {
    let vlcs = &*VLCS;
    if dst.num_points > 0 {
        // the first location is coded directly
        dst.loc_code[0] = gb.geti(5);
        for i in 1..points(dst) {
            // the table follows the curve's direction
            let tab = if dst.lev_code[i] <= dst.lev_code[i - 1] {
                &vlcs.gain[7]
            } else {
                &vlcs.gain[9]
            };
            dst.loc_code[i] = dst.loc_code[i - 1] + tab.get(gb);
        }
    }
}

/// `decode_gainc_loc_codes`.
fn decode_gainc_loc_codes(
    gb: &mut BitReader,
    chan: &mut ChanParams,
    reference: &[GainInfo; SUBBANDS],
    ch_num: usize,
    coded_subbands: usize,
) -> Result<()> {
    let vlcs = &*VLCS;
    let gain = chan.gain_data_mut();
    match gb.get(2) {
        0 => {
            // a sequence of numbers in ascending order
            for g in gain.iter_mut().take(coded_subbands) {
                for i in 0..points(g) {
                    gainc_loc_mode0(gb, g, i);
                }
            }
        }
        1 => {
            if ch_num != 0 {
                for sb in 0..coded_subbands {
                    if gain[sb].num_points <= 0 {
                        continue;
                    }
                    let dst = &mut gain[sb];
                    let r = &reference[sb];
                    // the first value is a VLC modulo delta to the master's
                    let delta = vlcs.gain[10].get(gb);
                    let pred = if r.num_points > 0 { r.loc_code[0] } else { 0 };
                    dst.loc_code[0] = (pred + delta) & 0x1F;
                    for i in 1..points(dst) {
                        let more_than_ref = i as i32 >= r.num_points;
                        if dst.lev_code[i] > dst.lev_code[i - 1] {
                            // ascending curve
                            if more_than_ref {
                                let delta = vlcs.gain[9].get(gb);
                                dst.loc_code[i] = dst.loc_code[i - 1] + delta;
                            } else if gb.get1() != 0 {
                                gainc_loc_mode0(gb, dst, i); // direct coding
                            } else {
                                dst.loc_code[i] = r.loc_code[i]; // clone master
                            }
                        } else {
                            // descending curve
                            let tab = if more_than_ref {
                                &vlcs.gain[7]
                            } else {
                                &vlcs.gain[10]
                            };
                            let delta = tab.get(gb);
                            if more_than_ref {
                                dst.loc_code[i] = dst.loc_code[i - 1] + delta;
                            } else {
                                dst.loc_code[i] = (r.loc_code[i] + delta) & 0x1F;
                            }
                        }
                    }
                }
            } else {
                // VLC delta to the previous location
                for g in gain.iter_mut().take(coded_subbands) {
                    gainc_loc_mode1(gb, g);
                }
            }
        }
        2 => {
            if ch_num != 0 {
                for sb in 0..coded_subbands {
                    if gain[sb].num_points <= 0 {
                        continue;
                    }
                    let dst = &mut gain[sb];
                    let r = &reference[sb];
                    if dst.num_points > r.num_points || gb.get1() != 0 {
                        gainc_loc_mode1(gb, dst);
                    } else {
                        // clone the master for the whole subband
                        for i in 0..points(dst) {
                            dst.loc_code[i] = r.loc_code[i];
                        }
                    }
                }
            } else {
                // the first subband's data is coded directly
                for i in 0..points(&gain[0]) {
                    gainc_loc_mode0(gb, &mut gain[0], i);
                }
                for sb in 1..coded_subbands {
                    if gain[sb].num_points <= 0 {
                        continue;
                    }
                    let (before, after) = gain.split_at_mut(sb);
                    let prev = &before[sb - 1];
                    let dst = &mut after[0];
                    // the first value is a VLC modulo delta to the previous
                    // subband's first, if any, or zero
                    let delta = vlcs.gain[6].get(gb);
                    let pred = if prev.num_points > 0 {
                        prev.loc_code[0]
                    } else {
                        0
                    };
                    dst.loc_code[0] = (pred + delta) & 0x1F;
                    for i in 1..points(dst) {
                        let more_than_ref = i as i32 >= prev.num_points;
                        // the table follows the curve's direction and the
                        // presence of a prediction
                        let tab = &vlcs.gain[usize::from(dst.lev_code[i] > dst.lev_code[i - 1])
                            * 2
                            + usize::from(more_than_ref)
                            + 6];
                        let delta = tab.get(gb);
                        if more_than_ref {
                            dst.loc_code[i] = dst.loc_code[i - 1] + delta;
                        } else {
                            dst.loc_code[i] = (prev.loc_code[i] + delta) & 0x1F;
                        }
                    }
                }
            }
        }
        _ => {
            if ch_num != 0 {
                // clone the master, or code directly
                for sb in 0..coded_subbands {
                    for i in 0..points(&gain[sb]) {
                        if i as i32 >= reference[sb].num_points {
                            gainc_loc_mode0(gb, &mut gain[sb], i);
                        } else {
                            gain[sb].loc_code[i] = reference[sb].loc_code[i];
                        }
                    }
                }
            } else {
                // shorter delta to min
                let delta_bits = gb.get(2) + 1;
                let min_val = gb.geti(5);
                for g in gain.iter_mut().take(coded_subbands) {
                    for i in 0..points(g) {
                        g.loc_code[i] = min_val + i as i32 + gb.geti(delta_bits);
                    }
                }
            }
        }
    }

    // validate the decoded information
    for g in gain.iter().take(coded_subbands) {
        for i in 0..points(g) {
            if g.loc_code[i] < 0
                || g.loc_code[i] > 31
                || (i > 0 && g.loc_code[i] <= g.loc_code[i - 1])
            {
                return Err(invalid("invalid gain location"));
            }
        }
    }
    Ok(())
}

/// `decode_gainc_data`.
fn decode_gainc_data(gb: &mut BitReader, ctx: &mut ChanUnit, num_channels: usize) -> Result<()> {
    for ch_num in 0..num_channels {
        let reference = *ctx.channels[0].gain_data();
        let chan = &mut ctx.channels[ch_num];
        *chan.gain_data_mut() = [GainInfo::default(); SUBBANDS];
        if gb.get1() != 0 {
            // gain control data present
            let coded_subbands = gb.get(4) as usize + 1;
            chan.num_gain_subbands = if gb.get1() != 0 {
                gb.geti(4) + 1
            } else {
                coded_subbands as i32
            };
            // channel 0's own data, decoded so far, is its reference
            let reference = if ch_num == 0 {
                [GainInfo::default(); SUBBANDS]
            } else {
                reference
            };
            decode_gainc_npoints(gb, chan, &reference, ch_num, coded_subbands)?;
            decode_gainc_levels(gb, chan, &reference, ch_num, coded_subbands)?;
            decode_gainc_loc_codes(gb, chan, &reference, ch_num, coded_subbands)?;
            // propagate the gain data if requested
            let num_gain_subbands = chan.num_gain_subbands as usize;
            let gain = chan.gain_data_mut();
            for sb in coded_subbands..num_gain_subbands {
                gain[sb] = gain[sb - 1];
            }
        } else {
            chan.num_gain_subbands = 0;
        }
    }
    Ok(())
}

/// Number of tone bands as an index bound.
fn tone_bands(ctx: &ChanUnit) -> usize {
    (ctx.waves_info().num_tone_bands.max(0) as usize).min(SUBBANDS)
}

/// `decode_tones_envelope`.
fn decode_tones_envelope(
    gb: &mut BitReader,
    ctx: &mut ChanUnit,
    ch_num: usize,
    band_has_tones: &[bool; SUBBANDS],
) {
    let bands = tone_bands(ctx);
    let reference = *ctx.channels[0].tones_info();
    let dst = ctx.channels[ch_num].tones_info_mut();
    if ch_num == 0 || gb.get1() == 0 {
        // mode 0: fixed-length coding
        for sb in 0..bands {
            if !band_has_tones[sb] {
                continue;
            }
            let env = &mut dst[sb].pend_env;
            env.has_start_point = gb.get1() != 0;
            env.start_pos = if env.has_start_point { gb.geti(5) } else { -1 };
            env.has_stop_point = gb.get1() != 0;
            env.stop_pos = if env.has_stop_point { gb.geti(5) } else { 32 };
        }
    } else {
        // mode 1 (slave only): copy the master's
        for sb in 0..bands {
            if band_has_tones[sb] {
                dst[sb].pend_env = reference[sb].pend_env;
            }
        }
    }
}

/// `decode_band_numwavs`.
fn decode_band_numwavs(
    gb: &mut BitReader,
    ctx: &mut ChanUnit,
    ch_num: usize,
    band_has_tones: &[bool; SUBBANDS],
) -> Result<()> {
    let vlcs = &*VLCS;
    let bands = tone_bands(ctx);
    let reference = *ctx.channels[0].tones_info();
    let mut tones_index = ctx.waves_info().tones_index;
    let dst = ctx.channels[ch_num].tones_info_mut();
    match gb.get(ch_num as u32 + 1) {
        0 => {
            // fixed-length coding
            for sb in 0..bands {
                if band_has_tones[sb] {
                    dst[sb].num_wavs = gb.geti(4);
                }
            }
        }
        1 => {
            // variable-length coding
            for sb in 0..bands {
                if band_has_tones[sb] {
                    dst[sb].num_wavs = vlcs.tone[1].get(gb);
                }
            }
        }
        2 => {
            // VLC modulo delta to the master (slave only)
            for sb in 0..bands {
                if band_has_tones[sb] {
                    let delta = sign_extend(vlcs.tone[2].get(gb), 3);
                    dst[sb].num_wavs = (reference[sb].num_wavs + delta) & 0xF;
                }
            }
        }
        _ => {
            // copy the master (slave only)
            for sb in 0..bands {
                if band_has_tones[sb] {
                    dst[sb].num_wavs = reference[sb].num_wavs;
                }
            }
        }
    }
    // the start tone index of each subband
    for sb in 0..bands {
        if band_has_tones[sb] {
            if dst[sb].num_wavs < 0 || tones_index + dst[sb].num_wavs > 48 {
                return Err(invalid("too many tones"));
            }
            dst[sb].start_index = tones_index;
            tones_index += dst[sb].num_wavs;
        }
    }
    ctx.waves_info_mut().tones_index = tones_index;
    Ok(())
}

/// The waves of a band: `num_wavs` entries from `start_index` (both
/// checked against the 48-entry table when decoded).
fn band_waves<'a>(waves: &'a mut [WaveParam; 48], data: &WavesData) -> &'a mut [WaveParam] {
    let start = data.start_index.clamp(0, 48) as usize;
    let end = (start + data.num_wavs.max(0) as usize).min(48);
    &mut waves[start..end]
}

/// `decode_tones_frequency`.
fn decode_tones_frequency(
    gb: &mut BitReader,
    ctx: &mut ChanUnit,
    ch_num: usize,
    band_has_tones: &[bool; SUBBANDS],
) {
    let vlcs = &*VLCS;
    let bands = tone_bands(ctx);
    let reference = *ctx.channels[0].tones_info();
    let dst = *ctx.channels[ch_num].tones_info();
    let waves = &mut ctx.waves_info_mut().waves;
    if ch_num == 0 || gb.get1() == 0 {
        // mode 0: fixed-length coding
        for sb in 0..bands {
            if !band_has_tones[sb] || dst[sb].num_wavs == 0 {
                continue;
            }
            let iwav = band_waves(waves, &dst[sb]);
            let n = iwav.len();
            let direction = if dst[sb].num_wavs > 1 { gb.get1() } else { 0 };
            if direction != 0 {
                // packed numbers in descending order
                if n > 0 {
                    iwav[n - 1].freq_index = gb.geti(10);
                }
                for i in (0..n.saturating_sub(1)).rev() {
                    let nbits = log2(iwav[i + 1].freq_index) + 1;
                    iwav[i].freq_index = gb.geti(nbits);
                }
            } else {
                // packed numbers in ascending order
                for i in 0..n {
                    if i == 0 || iwav[i - 1].freq_index < 512 {
                        iwav[i].freq_index = gb.geti(10);
                    } else {
                        let nbits = log2(1023 - iwav[i - 1].freq_index) + 1;
                        iwav[i].freq_index = gb.geti(nbits) + 1024 - (1 << nbits);
                    }
                }
            }
        }
    } else {
        // mode 1: VLC modulo delta to the master (slave only)
        for sb in 0..bands {
            if !band_has_tones[sb] || dst[sb].num_wavs == 0 {
                continue;
            }
            let iwav: Vec<i32> = band_waves(waves, &reference[sb])
                .iter()
                .map(|w| w.freq_index)
                .collect();
            let owav = band_waves(waves, &dst[sb]);
            for (i, w) in owav.iter_mut().enumerate() {
                let delta = sign_extend(vlcs.tone[6].get(gb), 8);
                let pred = match iwav.get(i).or(iwav.last()) {
                    Some(&f) => f,
                    None => 0,
                };
                w.freq_index = (pred + delta) & 0x3FF;
            }
        }
    }
}

/// `decode_tones_amplitude`.
fn decode_tones_amplitude(
    gb: &mut BitReader,
    ctx: &mut ChanUnit,
    ch_num: usize,
    band_has_tones: &[bool; SUBBANDS],
) {
    let vlcs = &*VLCS;
    let bands = tone_bands(ctx);
    let reference = *ctx.channels[0].tones_info();
    let dst = *ctx.channels[ch_num].tones_info();
    let info = ctx.waves_info_mut();
    let mut refwaves = [0i32; 48];

    if ch_num != 0 {
        for sb in 0..bands {
            if !band_has_tones[sb] || dst[sb].num_wavs == 0 {
                continue;
            }
            let wsrc: Vec<i32> = band_waves(&mut info.waves, &dst[sb])
                .iter()
                .map(|w| w.freq_index)
                .collect();
            let wref: Vec<i32> = band_waves(&mut info.waves, &reference[sb])
                .iter()
                .map(|w| w.freq_index)
                .collect();
            for (j, &f) in wsrc.iter().enumerate() {
                let (mut fi, mut maxdiff) = (0usize, 1024);
                for (i, &r) in wref.iter().enumerate() {
                    let diff = (f - r).abs();
                    if diff < maxdiff {
                        maxdiff = diff;
                        fi = i;
                    }
                }
                let slot = dst[sb].start_index as usize + j;
                refwaves[slot] = if maxdiff < 8 {
                    fi as i32 + reference[sb].start_index
                } else if j < wref.len() {
                    j as i32 + reference[sb].start_index
                } else {
                    -1
                };
            }
        }
    }

    let amp_of = |waves: &[WaveParam; 48], r: i32, default: i32| -> i32 {
        usize::try_from(r)
            .ok()
            .and_then(|r| waves.get(r))
            .map_or(default, |w| w.amp_sf)
    };
    match gb.get(ch_num as u32 + 1) {
        0 => {
            // fixed-length coding
            for sb in 0..bands {
                if !band_has_tones[sb] || dst[sb].num_wavs == 0 {
                    continue;
                }
                let amplitude_mode = info.amplitude_mode;
                let w = band_waves(&mut info.waves, &dst[sb]);
                if amplitude_mode != 0 {
                    for p in w.iter_mut() {
                        p.amp_sf = gb.geti(6);
                    }
                } else if let Some(p) = w.first_mut() {
                    p.amp_sf = gb.geti(6);
                }
            }
        }
        1 => {
            // min + VLC delta
            for sb in 0..bands {
                if !band_has_tones[sb] || dst[sb].num_wavs == 0 {
                    continue;
                }
                let amplitude_mode = info.amplitude_mode;
                let w = band_waves(&mut info.waves, &dst[sb]);
                if amplitude_mode != 0 {
                    for p in w.iter_mut() {
                        p.amp_sf = vlcs.tone[3].get(gb) + 20;
                    }
                } else if let Some(p) = w.first_mut() {
                    p.amp_sf = vlcs.tone[4].get(gb) + 24;
                }
            }
        }
        2 => {
            // VLC modulo delta to the master (slave only)
            for sb in 0..bands {
                if !band_has_tones[sb] || dst[sb].num_wavs == 0 {
                    continue;
                }
                let start = dst[sb].start_index as usize;
                for i in 0..band_waves(&mut info.waves, &dst[sb]).len() {
                    let delta = sign_extend(vlcs.tone[5].get(gb), 5);
                    let pred = amp_of(&info.waves, refwaves[start + i], 34);
                    info.waves[start + i].amp_sf = (pred + delta) & 0x3F;
                }
            }
        }
        _ => {
            // clone the master (slave only)
            for sb in 0..bands {
                if !band_has_tones[sb] {
                    continue;
                }
                let start = dst[sb].start_index.max(0) as usize;
                for i in 0..band_waves(&mut info.waves, &dst[sb]).len() {
                    info.waves[start + i].amp_sf = amp_of(&info.waves, refwaves[start + i], 32);
                }
            }
        }
    }
}

/// `decode_tones_phase`.
fn decode_tones_phase(
    gb: &mut BitReader,
    ctx: &mut ChanUnit,
    ch_num: usize,
    band_has_tones: &[bool; SUBBANDS],
) {
    let bands = tone_bands(ctx);
    let dst = *ctx.channels[ch_num].tones_info();
    let waves = &mut ctx.waves_info_mut().waves;
    for sb in 0..bands {
        if band_has_tones[sb] {
            for w in band_waves(waves, &dst[sb]) {
                w.phase_index = gb.geti(5);
            }
        }
    }
}

/// `decode_tones_info`.
fn decode_tones_info(gb: &mut BitReader, ctx: &mut ChanUnit, num_channels: usize) -> Result<()> {
    for chan in ctx.channels.iter_mut().take(num_channels) {
        *chan.tones_info_mut() = [WavesData::default(); SUBBANDS];
    }
    let present = gb.get1() != 0;
    ctx.waves_info_mut().tones_present = present;
    if !present {
        return Ok(());
    }
    let info = ctx.waves_info_mut();
    info.waves = [WaveParam::default(); 48];
    info.amplitude_mode = gb.geti(1);
    if info.amplitude_mode == 0 {
        return Err(Error::unsupported("atrac3plus: GHA amplitude mode 0"));
    }
    info.num_tone_bands = VLCS.tone[0].get(gb) + 1;
    let bands = (info.num_tone_bands.max(0) as usize).min(SUBBANDS);
    if num_channels == 2 {
        get_subband_flags(gb, &mut info.tone_sharing, bands);
        get_subband_flags(gb, &mut info.tone_master, bands);
        get_subband_flags(gb, &mut info.invert_phase, bands);
    }
    info.tones_index = 0;

    for ch_num in 0..num_channels {
        let mut band_has_tones = [false; SUBBANDS];
        for (i, b) in band_has_tones.iter_mut().enumerate().take(bands) {
            *b = ch_num == 0 || ctx.waves_info().tone_sharing[i] == 0;
        }
        decode_tones_envelope(gb, ctx, ch_num, &band_has_tones);
        decode_band_numwavs(gb, ctx, ch_num, &band_has_tones)?;
        decode_tones_frequency(gb, ctx, ch_num, &band_has_tones);
        decode_tones_amplitude(gb, ctx, ch_num, &band_has_tones);
        decode_tones_phase(gb, ctx, ch_num, &band_has_tones);
    }

    if num_channels == 2 {
        let (sharing, master) = (ctx.waves_info().tone_sharing, ctx.waves_info().tone_master);
        let (c0, c1) = ctx.channels.split_at_mut(1);
        let (t0, t1) = (c0[0].tones_info_mut(), c1[0].tones_info_mut());
        for i in 0..bands {
            if sharing[i] != 0 {
                t1[i] = t0[i];
            }
            if master[i] != 0 {
                std::mem::swap(&mut t0[i], &mut t1[i]);
            }
        }
    }
    Ok(())
}

/// `ff_atrac3p_decode_channel_unit`.
pub(super) fn decode_channel_unit(
    gb: &mut BitReader,
    ctx: &mut ChanUnit,
    num_channels: usize,
) -> Result<()> {
    // sound header
    ctx.num_quant_units = gb.geti(5) + 1;
    if ctx.num_quant_units > 28 && ctx.num_quant_units < 32 {
        return Err(invalid("invalid number of quantization units"));
    }
    ctx.mute_flag = gb.get1() != 0;

    decode_quant_wordlen(gb, ctx, num_channels)?;
    ctx.num_subbands = i32::from(QU_TO_SUBBAND[ctx.num_quant_units as usize - 1]) + 1;
    ctx.num_coded_subbands = if ctx.used_quant_units != 0 {
        i32::from(QU_TO_SUBBAND[ctx.used_quant_units as usize - 1]) + 1
    } else {
        0
    };

    decode_scale_factors(gb, ctx, num_channels)?;
    decode_code_table_indexes(gb, ctx, num_channels)?;
    decode_spectrum(gb, ctx, num_channels)?;

    if num_channels == 2 {
        let n = ctx.num_coded_subbands as usize;
        get_subband_flags(gb, &mut ctx.swap_channels, n);
        get_subband_flags(gb, &mut ctx.negate_coeffs, n);
    }

    decode_window_shape(gb, ctx, num_channels);
    decode_gainc_data(gb, ctx, num_channels)?;
    decode_tones_info(gb, ctx, num_channels)?;

    // global noise info
    ctx.noise_present = gb.get1() != 0;
    if ctx.noise_present {
        ctx.noise_level_index = gb.geti(4);
        ctx.noise_table_index = gb.geti(4);
    }
    Ok(())
}
