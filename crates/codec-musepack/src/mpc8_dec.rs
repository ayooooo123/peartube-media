// Ported from FFmpeg libavcodec/mpc8.c (commit 2da55bf), LGPL-2.1-or-later.
// Copyright (c) 2007 Konstantin Shishkov.

use std::collections::VecDeque;
use std::sync::LazyLock;

use oxideav_core::{
    AudioFormat, AudioFrame, CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result, SampleFormat,
};

use crate::bits::{sign_extend, BitReader};
use crate::mpc::{dequantize_and_synth, Band, Lfg, BANDS, MPC_FRAME_SIZE, SAMPLES_PER_BAND};
use crate::mpc8_data::MPC8_THRES;
use crate::mpc8_data::{MPC8_HUFFQ2, MPC8_IDX50, MPC8_IDX51, MPC8_IDX52};
use crate::mpc8_huff::{
    MPC8_BANDS_LEN_COUNTS, MPC8_BANDS_SYMS, MPC8_DSCF_LEN_COUNTS, MPC8_DSCF_SYMS, MPC8_Q1_LEN_COUNTS,
    MPC8_Q2_LEN_COUNTS, MPC8_Q34_LEN_COUNTS, MPC8_Q5_8_LEN_COUNTS, MPC8_Q9UP_LEN_COUNTS, MPC8_Q_SYMS,
    MPC8_RES_LEN_COUNTS, MPC8_RES_SYMS, MPC8_SCFI_LEN_COUNTS, MPC8_SCFI_SYMS,
};
use crate::synth::MpaSynth;
use crate::vlc::Vlc;

struct Mpc8Tables {
    band_vlc: Vlc,
    scfi_vlc: [Vlc; 2],
    dscf_vlc: [Vlc; 2],
    res_vlc: [Vlc; 2],
    q1_vlc: Vlc,
    q9up_vlc: Vlc,
    q2_vlc: [Vlc; 2],
    q3_vlc: [Vlc; 2],
    quant_vlc: [[Vlc; 2]; 4],
}

static TABLES: LazyLock<Mpc8Tables> = LazyLock::new(|| {
    let mut q_syms = &MPC8_Q_SYMS[..];
    let mut scfi_syms = &MPC8_SCFI_SYMS[..];
    let mut dscf_syms = &MPC8_DSCF_SYMS[..];
    let mut res_syms = &MPC8_RES_SYMS[..];

    let band_vlc = Vlc::build_vlc_counts(&MPC8_BANDS_LEN_COUNTS, &MPC8_BANDS_SYMS, 0).expect("band_vlc");

    let q1_vlc = Vlc::build_vlc_counts(&MPC8_Q1_LEN_COUNTS, q_syms, 0).expect("q1_vlc");
    q_syms = &q_syms[19..];

    let q9up_vlc = Vlc::build_vlc_counts(&MPC8_Q9UP_LEN_COUNTS, q_syms, 0).expect("q9up_vlc");
    q_syms = &q_syms[256..];

    let mut scfi_vlc = [Vlc::default(), Vlc::default()];
    let mut dscf_vlc = [Vlc::default(), Vlc::default()];
    let mut res_vlc = [Vlc::default(), Vlc::default()];
    let mut q2_vlc = [Vlc::default(), Vlc::default()];
    let mut q3_vlc = [Vlc::default(), Vlc::default()];
    let mut quant_vlc = [
        [Vlc::default(), Vlc::default()],
        [Vlc::default(), Vlc::default()],
        [Vlc::default(), Vlc::default()],
        [Vlc::default(), Vlc::default()],
    ];

    for i in 0..2 {
        let scfi_cnt: usize = MPC8_SCFI_LEN_COUNTS[i].iter().map(|&c| c as usize).sum();
        scfi_vlc[i] = Vlc::build_vlc_counts(&MPC8_SCFI_LEN_COUNTS[i], scfi_syms, 0).expect("scfi");
        scfi_syms = &scfi_syms[scfi_cnt..];

        let dscf_cnt: usize = MPC8_DSCF_LEN_COUNTS[i].iter().map(|&c| c as usize).sum();
        dscf_vlc[i] = Vlc::build_vlc_counts(&MPC8_DSCF_LEN_COUNTS[i], dscf_syms, 0).expect("dscf");
        dscf_syms = &dscf_syms[dscf_cnt..];

        let res_cnt: usize = MPC8_RES_LEN_COUNTS[i].iter().map(|&c| c as usize).sum();
        res_vlc[i] = Vlc::build_vlc_counts(&MPC8_RES_LEN_COUNTS[i], res_syms, 0).expect("res");
        res_syms = &res_syms[res_cnt..];

        let q2_cnt: usize = MPC8_Q2_LEN_COUNTS[i].iter().map(|&c| c as usize).sum();
        q2_vlc[i] = Vlc::build_vlc_counts(&MPC8_Q2_LEN_COUNTS[i], q_syms, 0).expect("q2");
        q_syms = &q_syms[q2_cnt..];

        let q3_cnt: usize = MPC8_Q34_LEN_COUNTS[i].iter().map(|&c| c as usize).sum();
        q3_vlc[i] = Vlc::build_vlc_counts(&MPC8_Q34_LEN_COUNTS[i], q_syms, -48 - 16 * (i as i16)).expect("q3");
        q_syms = &q_syms[q3_cnt..];

        for j in 0..4 {
            let q_cnt: usize = MPC8_Q5_8_LEN_COUNTS[i][j].iter().map(|&c| c as usize).sum();
            quant_vlc[j][i] = Vlc::build_vlc_counts(&MPC8_Q5_8_LEN_COUNTS[i][j], q_syms, -((8i16 << j) - 1)).expect("quant");
            q_syms = &q_syms[q_cnt..];
        }
    }

    Mpc8Tables {
        band_vlc,
        scfi_vlc,
        dscf_vlc,
        res_vlc,
        q1_vlc,
        q9up_vlc,
        q2_vlc,
        q3_vlc,
        quant_vlc,
    }
});

pub struct Mpc8Decoder {
    synth: MpaSynth,
    rnd: Lfg,
    old_dscf: [[i32; BANDS]; 2],
    maxbands: usize,
    channels: u16,
    sample_rate: u32,
    mss: bool,
    frames: usize,
    cur_frame: usize,
    last_bits_used: usize,
    last_max_band: usize,
    q: [[i32; MPC_FRAME_SIZE]; 2],
    queue: VecDeque<Frame>,
    bands: [Band; BANDS],
    codec_id: CodecId,
}

impl Mpc8Decoder {
    pub fn new(params: &CodecParameters) -> Result<Self> {
        if params.extradata.len() < 2 {
            return Err(Error::invalid("mpc8: extradata too short"));
        }

        let mut gb = BitReader::new(&params.extradata, params.extradata.len());
        let sample_rate_idx = gb.get_bits(3) as usize;
        let sample_rates = [44100, 48000, 37800, 32000];
        if sample_rate_idx >= sample_rates.len() {
            return Err(Error::invalid("mpc8: invalid sample rate index"));
        }
        let sample_rate = sample_rates[sample_rate_idx];

        let maxbands = (gb.get_bits(5) as usize) + 1;
        if maxbands >= BANDS {
            return Err(Error::invalid("mpc8: maxbands too high"));
        }
        let channels = (gb.get_bits(4) as u16) + 1;
        if channels > 2 {
            return Err(Error::unsupported("mpc8: multichannel unsupported"));
        }
        let mss = gb.get_bits1() != 0;
        let frames = 1usize << ((gb.get_bits(3) & 3) * 2);

        Ok(Self {
            synth: MpaSynth::new(),
            rnd: Lfg::new(),
            old_dscf: [[0; BANDS]; 2],
            maxbands,
            channels,
            sample_rate,
            mss,
            frames,
            cur_frame: 0,
            last_bits_used: 0,
            last_max_band: 0,
            q: [[0; MPC_FRAME_SIZE]; 2],
            queue: VecDeque::new(),
            bands: [Band::default(); BANDS],
            codec_id: CodecId::new("musepack8"),
        })
    }

    fn decode_one_frame(&mut self, gb: &mut BitReader, pts: Option<i64>) -> Result<bool> {
        let keyframe = self.cur_frame == 0;
        if keyframe {
            self.q = [[0; MPC_FRAME_SIZE]; 2];
            self.last_bits_used = 0;
        }

        let tables = &*TABLES;
        let mut maxband: i32;
        if keyframe {
            maxband = gb.mpc8_get_mod_golomb(self.maxbands + 1) as i32;
        } else {
            let v = gb.get_vlc2(&tables.band_vlc.table, 9, 2);
            maxband = (self.last_max_band as i32).wrapping_add(v);
            if maxband > 32 {
                maxband -= 33;
            }
        }

        if gb.bits_left() < 0 {
            return Ok(false);
        }

        if maxband < 0 || maxband > (self.maxbands as i32 + 1) {
            return Err(Error::invalid("mpc8: maxband out of range"));
        }
        let maxband = maxband as usize;
        self.last_max_band = maxband;

        let bands = &mut self.bands;
        for i in 0..BANDS {
            bands[i].res = [0; 2];
            bands[i].msf = 0;
        }
        // read subband indexes
        if maxband > 0 {
            let mut last = [0i32; 2];
            for i in (0..maxband).rev() {
                for ch in 0..2 {
                    let vlc_idx = (last[ch] > 2) as usize;
                    let v = gb.get_vlc2(&tables.res_vlc[vlc_idx].table, 9, 2);
                    last[ch] = last[ch].wrapping_add(v);
                    if last[ch] > 15 {
                        last[ch] -= 17;
                    }
                    bands[i].res[ch] = last[ch];
                }
            }
            if self.mss {
                let mut cnt = 0usize;
                for i in 0..maxband {
                    if bands[i].res[0] != 0 || bands[i].res[1] != 0 {
                        cnt += 1;
                    }
                }
                let t = gb.mpc8_get_mod_golomb(cnt) as usize;
                let mut mask = gb.mpc8_get_mask(cnt, t);
                for i in (0..maxband).rev() {
                    if bands[i].res[0] != 0 || bands[i].res[1] != 0 {
                        bands[i].msf = (mask & 1) as i32;
                        mask >>= 1;
                    }
                }
            }
        }
        for i in maxband..self.maxbands {
            bands[i].res[0] = 0;
            bands[i].res[1] = 0;
        }

        if keyframe {
            for i in 0..32 {
                self.old_dscf[0][i] = 1;
                self.old_dscf[1][i] = 1;
            }
        }

        for i in 0..maxband {
            if bands[i].res[0] != 0 || bands[i].res[1] != 0 {
                let cnt = (if bands[i].res[0] != 0 { 1 } else { 0 })
                    + (if bands[i].res[1] != 0 { 1 } else { 0 })
                    - 1;
                if cnt >= 0 {
                    let t = gb.get_vlc2(&tables.scfi_vlc[cnt as usize].table, tables.scfi_vlc[cnt as usize].bits, 1);
                    if bands[i].res[0] != 0 {
                        bands[i].scfi[0] = t >> (2 * cnt);
                    }
                    if bands[i].res[1] != 0 {
                        bands[i].scfi[1] = t & 3;
                    }
                }
            }
        }

        for i in 0..maxband {
            for ch in 0..2 {
                if bands[i].res[ch] == 0 {
                    continue;
                }

                if self.old_dscf[ch][i] != 0 {
                    bands[i].scf_idx[ch][0] = (gb.get_bits(7) as i32) - 6;
                    self.old_dscf[ch][i] = 0;
                } else {
                    let mut t = gb.get_vlc2(&tables.dscf_vlc[1].table, 9, 2);
                    if t == 64 {
                        t += gb.get_bits(6) as i32;
                    }
                    bands[i].scf_idx[ch][0] = ((bands[i].scf_idx[ch][2].wrapping_add(t).wrapping_sub(25)) & 0x7F) - 6;
                }
                for j in 0..2 {
                    if ((bands[i].scfi[ch] << j) & 2) != 0 {
                        bands[i].scf_idx[ch][j + 1] = bands[i].scf_idx[ch][j];
                    } else {
                        let mut t = gb.get_vlc2(&tables.dscf_vlc[0].table, 9, 2);
                        if t == 31 {
                            t = 64 + (gb.get_bits(6) as i32);
                        }
                        bands[i].scf_idx[ch][j + 1] =
                            ((bands[i].scf_idx[ch][j].wrapping_add(t).wrapping_sub(25)) & 0x7F) - 6;
                    }
                }
            }
        }

        for i in 0..maxband {
            let off = i * SAMPLES_PER_BAND;
            for ch in 0..2 {
                let res = bands[i].res[ch];
                match res {
                    -1 => {
                        for j in 0..SAMPLES_PER_BAND {
                            self.q[ch][off + j] = ((self.rnd.get() & 0x3FC) as i32) - 510;
                        }
                    }
                    0 => {}
                    1 => {
                        for j in (0..SAMPLES_PER_BAND).step_by(SAMPLES_PER_BAND / 2) {
                            let cnt = gb.get_vlc2(&tables.q1_vlc.table, 9, 2) as usize;
                            let t = gb.mpc8_get_mask(18, cnt);
                            for k in 0..(SAMPLES_PER_BAND / 2) {
                                let bit = (t & (1 << (SAMPLES_PER_BAND / 2 - k - 1))) != 0;
                                self.q[ch][off + j + k] = if bit {
                                    ((gb.get_bits1() as i32) << 1) - 1
                                } else {
                                    0
                                };
                            }
                        }
                    }
                    2 => {
                        let mut cnt = 6usize;
                        for j in (0..SAMPLES_PER_BAND).step_by(3) {
                            let vlc_idx = (cnt > 3) as usize;
                            let t = gb.get_vlc2(&tables.q2_vlc[vlc_idx].table, 9, 2);
                            let t_idx = if t >= 0 && (t as usize) < MPC8_IDX50.len() {
                                t as usize
                            } else {
                                0
                            };
                            self.q[ch][off + j] = MPC8_IDX50[t_idx] as i32;
                            self.q[ch][off + j + 1] = MPC8_IDX51[t_idx] as i32;
                            self.q[ch][off + j + 2] = MPC8_IDX52[t_idx] as i32;
                            cnt = (cnt >> 1) + (MPC8_HUFFQ2[t_idx] as usize);
                        }
                    }
                    3 | 4 => {
                        let vlc_idx = (res - 3) as usize;
                        for j in (0..SAMPLES_PER_BAND).step_by(2) {
                            let t = gb.get_vlc2(&tables.q3_vlc[vlc_idx].table, 9, 2);
                            self.q[ch][off + j + 1] = t >> 4;
                            self.q[ch][off + j] = sign_extend(t, 4);
                        }
                    }
                    5..=8 => {
                        let r_idx = res as usize;
                        let thres = MPC8_THRES.get(r_idx).copied().unwrap_or(0) as usize;
                        let mut cnt = 2 * thres;
                        for j in 0..SAMPLES_PER_BAND {
                            let vlc_idx = (cnt > thres) as usize;
                            let q_table = &tables.quant_vlc[r_idx - 5][vlc_idx];
                            let val = gb.get_vlc2(&q_table.table, q_table.bits, 2);
                            self.q[ch][off + j] = val;
                            cnt = (cnt >> 1) + (val.abs() as usize);
                        }
                    }
                    _ => {
                        for j in 0..SAMPLES_PER_BAND {
                            let mut val = gb.get_vlc2(&tables.q9up_vlc.table, 9, 2);
                            if res != 9 {
                                val <<= res - 9;
                                val |= gb.get_bits((res - 9) as u32) as i32;
                            }
                            val -= (1 << (res - 2)) - 1;
                            self.q[ch][off + j] = val;
                        }
                    }
                }
            }
        }

        let mut ch0_pcm = vec![0i16; MPC_FRAME_SIZE];
        let mut ch1_pcm = vec![0i16; MPC_FRAME_SIZE];
        {
            let mut out_slices = [&mut ch0_pcm[..], &mut ch1_pcm[..]];
            let dequant_mb = if maxband > 0 { maxband - 1 } else { 0 };
            dequantize_and_synth(
                &mut self.synth,
                &self.bands,
                dequant_mb,
                &self.q,
                &mut out_slices,
                self.channels as usize,
            );
        }

        let mut planes = Vec::with_capacity(self.channels as usize);
        let mut p0 = Vec::with_capacity(MPC_FRAME_SIZE * 2);
        for s in &ch0_pcm {
            p0.extend_from_slice(&s.to_le_bytes());
        }
        planes.push(p0);
        if self.channels > 1 {
            let mut p1 = Vec::with_capacity(MPC_FRAME_SIZE * 2);
            for s in &ch1_pcm {
                p1.extend_from_slice(&s.to_le_bytes());
            }
            planes.push(p1);
        }

        self.queue.push_back(Frame::Audio(AudioFrame {
            samples: MPC_FRAME_SIZE as u32,
            pts,
            data: planes,
        }));

        self.cur_frame += 1;
        self.last_bits_used = gb.bits_count() as usize;
        if self.cur_frame >= self.frames {
            self.cur_frame = 0;
        }

        if gb.bits_left() < 0 {
            self.last_bits_used = gb.size_in_bits() as usize;
            return Ok(false);
        } else if self.cur_frame == 0 && gb.bits_left() < 8 {
            self.last_bits_used = gb.size_in_bits() as usize;
            return Ok(false);
        }

        Ok(true)
    }
}

impl Decoder for Mpc8Decoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn output_audio_format(&self) -> Option<AudioFormat> {
        Some(AudioFormat {
            sample_format: SampleFormat::S16P,
            sample_rate: self.sample_rate,
            channels: self.channels,
        })
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.is_empty() {
            return Ok(());
        }

        let mut gb = BitReader::new(&packet.data, packet.data.len());
        self.cur_frame = 0;

        let mut pts = packet.pts;
        for _ in 0..self.frames.min(64) {
            if !self.decode_one_frame(&mut gb, pts.take())? {
                break;
            }
            if gb.bits_left() < 8 {
                break;
            }
        }

        self.cur_frame = 0;
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        self.queue.pop_front().ok_or(Error::NeedMore)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        self.synth.reset();
        self.rnd = Lfg::new();
        self.old_dscf = [[0; BANDS]; 2];
        self.cur_frame = 0;
        self.last_bits_used = 0;
        self.last_max_band = 0;
        self.q = [[0; MPC_FRAME_SIZE]; 2];
        self.queue.clear();
        self.bands = [Band::default(); BANDS];
        Ok(())
    }
}

pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(Mpc8Decoder::new(params)?))
}
