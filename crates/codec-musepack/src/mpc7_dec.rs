// Ported from FFmpeg libavcodec/mpc7.c (commit 2da55bf), LGPL-2.1-or-later.
// Copyright (c) 2006 Konstantin Shishkov.

use std::collections::VecDeque;
use std::sync::LazyLock;

use oxideav_core::{
    AudioFormat, AudioFrame, CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result, SampleFormat,
};

use crate::bits::{bswap_buf, BitReader};
use crate::mpc::{dequantize_and_synth, Band, Lfg, BANDS, MPC_FRAME_SIZE, SAMPLES_PER_BAND};
use crate::mpc7_data::{
    MPC7_DSCF, MPC7_HDR, MPC7_IDX30, MPC7_IDX31, MPC7_IDX32, MPC7_IDX50, MPC7_IDX51, MPC7_QUANT_VLCS,
    MPC7_QUANT_VLC_OFF, MPC7_QUANT_VLC_SIZES, MPC7_SCFI,
};
use mpegaudiodsp::MpaSynth;
use crate::vlc::Vlc;

struct Mpc7Tables {
    scfi_vlc: Vlc,
    dscf_vlc: Vlc,
    hdr_vlc: Vlc,
    quant_vlc: [[Vlc; 2]; 7],
}

static TABLES: LazyLock<Mpc7Tables> = LazyLock::new(|| {
        // scfi_vlc
        let mut scfi_lens = [0i8; 4];
        let mut scfi_syms = [0i16; 4];
        for i in 0..4 {
            scfi_lens[i] = MPC7_SCFI[2 * i + 1] as i8;
            scfi_syms[i] = MPC7_SCFI[2 * i] as i16;
        }
        let scfi_vlc = Vlc::init_from_lengths(3, &scfi_lens, &scfi_syms, 0).expect("scfi_vlc");

        // dscf_vlc
        let mut dscf_lens = [0i8; 16];
        let mut dscf_syms = [0i16; 16];
        for i in 0..16 {
            dscf_lens[i] = MPC7_DSCF[2 * i + 1] as i8;
            dscf_syms[i] = MPC7_DSCF[2 * i] as i16;
        }
        let dscf_vlc = Vlc::init_from_lengths(6, &dscf_lens, &dscf_syms, -7).expect("dscf_vlc");

        // hdr_vlc
        let mut hdr_lens = [0i8; 10];
        let mut hdr_syms = [0i16; 10];
        for i in 0..10 {
            hdr_lens[i] = MPC7_HDR[2 * i + 1] as i8;
            hdr_syms[i] = MPC7_HDR[2 * i] as i16;
        }
        let hdr_vlc = Vlc::init_from_lengths(9, &hdr_lens, &hdr_syms, -5).expect("hdr_vlc");

        // quant_vlc
        let mut quant_vlc: [[Vlc; 2]; 7] = Default::default();
        let mut raw_offset = 0;
        for i in 0..7 {
            let size = MPC7_QUANT_VLC_SIZES[i];
            let off = MPC7_QUANT_VLC_OFF[i];
            for j in 0..2 {
                let mut lens = Vec::with_capacity(size);
                let mut syms = Vec::with_capacity(size);
                for k in 0..size {
                    lens.push(MPC7_QUANT_VLCS[raw_offset + 2 * k + 1] as i8);
                    syms.push(MPC7_QUANT_VLCS[raw_offset + 2 * k] as i16);
                }
                quant_vlc[i][j] = Vlc::init_from_lengths(9, &lens, &syms, off).expect("quant_vlc");
                raw_offset += 2 * size;
            }
        }

        Mpc7Tables { scfi_vlc, dscf_vlc, hdr_vlc, quant_vlc }
});

fn get_tables() -> &'static Mpc7Tables {
    &TABLES
}

#[inline]
fn get_scale_idx(gb: &mut BitReader, reference: i32, dscf_vlc: &Vlc) -> i32 {
    let t = gb.get_vlc2(&dscf_vlc.table, 6, 1);
    if t == 8 {
        gb.get_bits(6) as i32
    } else {
        reference.wrapping_add(t)
    }
}

fn idx_to_quant(
    rnd: &mut Lfg,
    gb: &mut BitReader,
    idx: i32,
    dst: &mut [i32; SAMPLES_PER_BAND],
    tables: &Mpc7Tables,
) {
    match idx {
        -1 => {
            for s in dst.iter_mut() {
                *s = ((rnd.get() & 0x3FC) as i32) - 510;
            }
        }
        1 => {
            let i1 = gb.get_bits1() as usize;
            for i in 0..(SAMPLES_PER_BAND / 3) {
                let t = gb.get_vlc2(&tables.quant_vlc[0][i1].table, 9, 2);
                let t_idx = if t >= 0 && (t as usize) < MPC7_IDX30.len() { t as usize } else { 0 };
                dst[3 * i] = MPC7_IDX30[t_idx] as i32;
                dst[3 * i + 1] = MPC7_IDX31[t_idx] as i32;
                dst[3 * i + 2] = MPC7_IDX32[t_idx] as i32;
            }
        }
        2 => {
            let i1 = gb.get_bits1() as usize;
            for i in 0..(SAMPLES_PER_BAND / 2) {
                let t = gb.get_vlc2(&tables.quant_vlc[1][i1].table, 9, 2);
                let t_idx = if t >= 0 && (t as usize) < MPC7_IDX50.len() { t as usize } else { 0 };
                dst[2 * i] = MPC7_IDX50[t_idx] as i32;
                dst[2 * i + 1] = MPC7_IDX51[t_idx] as i32;
            }
        }
        3..=7 => {
            let i1 = gb.get_bits1() as usize;
            let q_table = &tables.quant_vlc[(idx - 1) as usize][i1].table;
            for s in dst.iter_mut() {
                *s = gb.get_vlc2(q_table, 9, 2);
            }
        }
        8..=17 => {
            let t = (1i32 << (idx - 2)) - 1;
            let nbits = (idx - 1) as u32;
            for s in dst.iter_mut() {
                *s = (gb.get_bits(nbits) as i32).wrapping_sub(t);
            }
        }
        _ => {}
    }
}

pub struct Mpc7Decoder {
    synth: MpaSynth,
    rnd: Lfg,
    old_dscf: [[i32; BANDS]; 2],
    maxbands: usize,
    _is: bool,
    mss: bool,
    _gapless: bool,
    lastframelen: usize,
    frames_to_skip: usize,
    queue: VecDeque<Frame>,
    sample_rate: u32,
    channels: u16,
    codec_id: CodecId,
}

impl Mpc7Decoder {
    pub fn new(params: &CodecParameters) -> Result<Self> {
        if params.channels.is_some_and(|c| c != 2) {
            return Err(Error::unsupported("mpc7: only stereo supported"));
        }
        if params.extradata.len() < 16 {
            return Err(Error::invalid("mpc7: extradata too short"));
        }

        let mut swapped_extra = [0u8; 16];
        bswap_buf(&mut swapped_extra, &params.extradata[..16]);
        let mut gb = BitReader::new(&swapped_extra, 16);

        let is = gb.get_bits1() != 0;
        let mss = gb.get_bits1() != 0;
        let maxbands = gb.get_bits(6) as usize;
        if maxbands >= BANDS {
            return Err(Error::invalid("mpc7: too many bands"));
        }
        gb.skip_bits(88);
        let gapless = gb.get_bits1() != 0;
        let lastframelen = gb.get_bits(11) as usize;

        let sample_rate = params.sample_rate.unwrap_or(44100);

        Ok(Self {
            synth: MpaSynth::new(),
            rnd: Lfg::new(),
            old_dscf: [[0; BANDS]; 2],
            maxbands,
            _is: is,
            mss,
            _gapless: gapless,
            lastframelen,
            frames_to_skip: 0,
            queue: VecDeque::new(),
            sample_rate,
            channels: 2,
            codec_id: CodecId::new("musepack7"),
        })
    }
}

impl Decoder for Mpc7Decoder {
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
        if packet.data.len() < 4 {
            return Err(Error::invalid("mpc7: packet too small"));
        }
        let buf_size = packet.data.len() & !3;
        if buf_size < 4 {
            return Err(Error::invalid("mpc7: packet too small"));
        }

        let skip = packet.data[0];
        let last_frame = packet.data[1] != 0;
        let raw_payload = &packet.data[4..buf_size];
        let payload_len = raw_payload.len();

        let mut swapped_payload = vec![0u8; payload_len];
        bswap_buf(&mut swapped_payload, raw_payload);

        let mut gb = BitReader::new(&swapped_payload, payload_len);
        gb.skip_bits(skip as u32);

        let tables = get_tables();
        let mut bands = [Band::default(); BANDS];
        let mut mb: i32 = -1;

        // Read subband indexes
        for i in 0..=self.maxbands {
            for ch in 0..2 {
                let t = if i != 0 {
                    gb.get_vlc2(&tables.hdr_vlc.table, 9, 1)
                } else {
                    4
                };
                if t == 4 {
                    bands[i].res[ch] = gb.get_bits(4) as i32;
                } else if i > 0 {
                    bands[i].res[ch] = bands[i - 1].res[ch].wrapping_add(t);
                }
                if bands[i].res[ch] < -1 || bands[i].res[ch] > 17 {
                    return Err(Error::invalid("mpc7: invalid subband index"));
                }
            }
            if bands[i].res[0] != 0 || bands[i].res[1] != 0 {
                mb = i as i32;
                if self.mss {
                    bands[i].msf = gb.get_bits1() as i32;
                }
            }
        }

        if mb >= 0 {
            let mb = mb as usize;
            // Get scale indexes coding method
            for i in 0..=mb {
                for ch in 0..2 {
                    if bands[i].res[ch] != 0 {
                        bands[i].scfi[ch] = gb.get_vlc2(&tables.scfi_vlc.table, 3, 1);
                    }
                }
            }

            // Get scale indexes
            for i in 0..=mb {
                for ch in 0..2 {
                    if bands[i].res[ch] != 0 {
                        bands[i].scf_idx[ch][2] = self.old_dscf[ch][i];
                        bands[i].scf_idx[ch][0] = get_scale_idx(&mut gb, bands[i].scf_idx[ch][2], &tables.dscf_vlc);
                        match bands[i].scfi[ch] {
                            0 => {
                                bands[i].scf_idx[ch][1] = get_scale_idx(&mut gb, bands[i].scf_idx[ch][0], &tables.dscf_vlc);
                                bands[i].scf_idx[ch][2] = get_scale_idx(&mut gb, bands[i].scf_idx[ch][1], &tables.dscf_vlc);
                            }
                            1 => {
                                bands[i].scf_idx[ch][1] = get_scale_idx(&mut gb, bands[i].scf_idx[ch][0], &tables.dscf_vlc);
                                bands[i].scf_idx[ch][2] = bands[i].scf_idx[ch][1];
                            }
                            2 => {
                                bands[i].scf_idx[ch][1] = bands[i].scf_idx[ch][0];
                                bands[i].scf_idx[ch][2] = get_scale_idx(&mut gb, bands[i].scf_idx[ch][1], &tables.dscf_vlc);
                            }
                            _ => {
                                bands[i].scf_idx[ch][2] = bands[i].scf_idx[ch][0];
                                bands[i].scf_idx[ch][1] = bands[i].scf_idx[ch][0];
                            }
                        }
                        self.old_dscf[ch][i] = bands[i].scf_idx[ch][2];
                    }
                }
            }
        }

        // Get quantizers
        let mut q = [[0i32; MPC_FRAME_SIZE]; 2];
        for i in 0..BANDS {
            let off = i * SAMPLES_PER_BAND;
            for ch in 0..2 {
                let mut band_q = [0i32; SAMPLES_PER_BAND];
                idx_to_quant(&mut self.rnd, &mut gb, bands[i].res[ch], &mut band_q, tables);
                q[ch][off..off + SAMPLES_PER_BAND].copy_from_slice(&band_q);
            }
        }

        let mut ch0_pcm = vec![0i16; MPC_FRAME_SIZE];
        let mut ch1_pcm = vec![0i16; MPC_FRAME_SIZE];
        {
            let mut out_slices = [&mut ch0_pcm[..], &mut ch1_pcm[..]];
            let dequant_mb = if mb >= 0 { mb as usize } else { 0 };
            dequantize_and_synth(&mut self.synth, &bands, dequant_mb, &q, &mut out_slices, 2);
        }

        let nb_samples = if last_frame {
            self.lastframelen.min(MPC_FRAME_SIZE)
        } else {
            MPC_FRAME_SIZE
        };

        let bits_used = gb.bits_count() as usize;
        let bits_avail = payload_len * 8;
        if !last_frame && (bits_avail < bits_used || bits_used + 32 <= bits_avail) {
            return Err(Error::invalid("mpc7: bit counts mismatch"));
        }

        if self.frames_to_skip > 0 {
            self.frames_to_skip -= 1;
            return Ok(());
        }

        let mut plane0 = Vec::with_capacity(nb_samples * 2);
        let mut plane1 = Vec::with_capacity(nb_samples * 2);
        for s in &ch0_pcm[..nb_samples] {
            plane0.extend_from_slice(&s.to_le_bytes());
        }
        for s in &ch1_pcm[..nb_samples] {
            plane1.extend_from_slice(&s.to_le_bytes());
        }

        self.queue.push_back(Frame::Audio(AudioFrame {
            samples: nb_samples as u32,
            pts: packet.pts,
            data: vec![plane0, plane1],
        }));

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
        self.frames_to_skip = 32;
        self.queue.clear();
        Ok(())
    }
}

pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(Mpc7Decoder::new(params)?))
}
