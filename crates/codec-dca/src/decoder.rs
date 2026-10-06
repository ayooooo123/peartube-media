// Ported from FFmpeg libavcodec/dcadec.c and dca_parser.c (commit
// 2da55bf), LGPL-2.1-or-later.

//! Top-level DTS decoder: packet framing (bitstream conversion, core +
//! EXSS substream split), the decode dispatch (LBR → XLL → core with the
//! same fallbacks FFmpeg uses) and the frame parser used by the raw `dts`
//! demuxer.

use crate::core::CoreDecoder;
use crate::dca::{self, decoder_packets as pkt};
use crate::exss::{exss_parse, ExssParser};
use crate::lbr::LbrDecoder;
use crate::xll::{XllDecoder, XllError};

pub const MIN_PACKET_SIZE: usize = 16;
pub const MAX_PACKET_SIZE: usize = 0x104000;

/// `DCAContext`.
pub struct DcaDecoder {
    pub core: CoreDecoder,
    pub exss: ExssParser,
    pub xll: XllDecoder,
    pub lbr: LbrDecoder,

    pub packet: i32,

    /// Pending output frame: (sample_rate, planes, sample_format_bits).
    pub pending: Option<PendingFrame>,

    /// XLL band buffers carried between parse and filter.
    pub xll_buffers: Option<Vec<crate::xll::ChsBuffers>>,
}

/// One decoded audio frame: interleaved-ready planes + parameters.
pub struct PendingFrame {
    pub sample_rate: u32,
    /// Planes in output order (one Vec per channel).
    pub planes_f32: Vec<Vec<f32>>,
    pub planes_s32: Vec<Vec<i32>>,
    /// 16 (s16), 24-in-32 (s32-in-24) or 0 for f32.
    pub bits_per_sample: u32,
    pub pts: Option<i64>,
}



impl DcaDecoder {
    pub fn new() -> Self {
        Self {
            core: CoreDecoder::new(),
            exss: ExssParser::default(),
            xll: XllDecoder::new(),
            lbr: LbrDecoder::new(),
            packet: 0,
            pending: None,
            xll_buffers: None,
        }
    }

    /// `dcadec_decode_frame`. Returns Ok(true) when a frame was produced.
    pub fn decode_packet(&mut self, input: &[u8], pts: Option<i64>) -> Result<bool, &'static str> {
        if input.len() < MIN_PACKET_SIZE || input.len() > MAX_PACKET_SIZE {
            return Err("invalid packet size");
        }

        let prev_packet = self.packet;
        let mut input = input;
        let mut input_size = input.len();

        // Convert input to BE format
        let mrk = u32::from_be_bytes([input[0], input[1], input[2], input[3]]);
        if mrk != dca::DCA_SYNCWORD_CORE_BE && mrk != dca::DCA_SYNCWORD_SUBSTREAM {
            // Try every offset (FFmpeg walks input + i for i in 0..len-15)
            // until avpriv_dca_convert_bitstream succeeds.
            let mut converted: Vec<u8> = Vec::new();
            let mut ok = false;
            for i in 0..input_size.saturating_sub(MIN_PACKET_SIZE) + 1 {
                let sub = &input[i..];
                let mut dst = vec![0u8; sub.len() + 16];
                if let Some(n) = dca::convert_bitstream(sub, &mut dst) {
                    dst.truncate(n);
                    converted = dst;
                    input = &input[i..];
                    input_size = n;
                    ok = true;
                    break;
                }
            }
            if !ok {
                return Err("not a valid DCA frame");
            }
            // Keep the converted bytes alive for the rest of the call.
            self.core.buffer = converted;
        }

        self.packet = 0;

        // Keep a stable byte buffer: the core/xll/lbr parsers borrow slices
        // of it; we clone into owned storage up front.
        let storage: Vec<u8> = if !self.core.buffer.is_empty()
            && self.core.buffer.len() == input_size
            && input_size != input.len()
        {
            std::mem::take(&mut self.core.buffer)
        } else {
            input[..input_size].to_vec()
        };
        let data: &[u8] = &storage;

        // Parse backward compatible core sub-stream
        let mut consumed_core = 0usize;
        if u32::from_be_bytes([data[0], data[1], data[2], data[3]]) == dca::DCA_SYNCWORD_CORE_BE {
            self.core.core_parse(data)?;

            self.packet |= pkt::DCA_PACKET_CORE;

            // EXXS data must be aligned on 4-byte boundary
            let frame_size = self.core.frame_size.div_ceil(4) * 4;
            if input_size - 4 > frame_size {
                consumed_core = frame_size;
            }
        }

        if std::env::var("DCA_TRACE").is_ok() {
            eprintln!("TRACE-R dec: in={} fs={} consumed={} exss_sync={:?}", input_size, self.core.frame_size, consumed_core,
                u32::from_be_bytes([data[consumed_core.min(data.len().saturating_sub(1))], data[consumed_core.min(data.len().saturating_sub(1)) + 1], data[consumed_core.min(data.len().saturating_sub(1)) + 2], data[consumed_core.min(data.len().saturating_sub(1)) + 3]]));
        }
        let mut asset_index: Option<usize> = None;
        if !self.core.core_only {
            // Parse extension sub-stream (EXSS)
            let exss_data = &data[consumed_core..];
            if exss_data.len() >= 4
                && u32::from_be_bytes([exss_data[0], exss_data[1], exss_data[2], exss_data[3]])
                    == dca::DCA_SYNCWORD_SUBSTREAM
            {
                match exss_parse(&mut self.exss, exss_data) {
                    Err(e) => {
                        if std::env::var("DCA_TRACE").is_ok() {
                            eprintln!("TRACE-R exssparse ERR: {e:?}");
                        }
                    } // conceal, like FFmpeg without EXPLODE
                    Ok(()) => {
                        self.packet |= pkt::DCA_PACKET_EXSS;
                        asset_index = Some(consumed_core);
                    }
                }
            }

            // Parse XLL component in EXSS
            if let Some(off) = asset_index {
                let asset = self.exss.assets[0].clone();
                if asset.has_xll() {
                    match self.xll.xll_parse(&data[off..], &asset) {
                        Err(XllError::Again) => {
                            // Conceal XLL synchronization error
                            if (prev_packet & pkt::DCA_PACKET_XLL) != 0 && (self.packet & pkt::DCA_PACKET_CORE) != 0 {
                                self.packet |= pkt::DCA_PACKET_XLL | pkt::DCA_PACKET_RECOVERY;
                            }
                        }
                        Err(_) => {}
                        Ok(buffers) => {
                            self.xll_buffers = Some(buffers);
                            self.packet |= pkt::DCA_PACKET_XLL;
                        }
                    }
                }
            }

            // Parse LBR component in EXSS
            if let Some(off) = asset_index {
                let asset = self.exss.assets[0].clone();
                if asset.has_lbr() {
                    if self.lbr.lbr_parse(&data[off..], &asset).is_ok() {
                        self.packet |= pkt::DCA_PACKET_LBR;
                    }
                }
            }

            // Parse core extensions in EXSS or backward compatible core
            if self.packet & pkt::DCA_PACKET_CORE != 0 {
                let asset = asset_index.map(|off| {
                    let mut a = self.exss.assets[0].clone();
                    a.asset_offset += off;
                    a.core_offset += off;
                    a.xbr_offset += off;
                    a.xxch_offset += off;
                    a.x96_offset += off;
                    a.lbr_offset += off;
                    a.xll_offset += off;
                    a
                });
                self.core.core_parse_exss(data, asset.as_ref())?;
            }
        }

        // Share the packet flags with the sub-decoders (FFmpeg keeps one
        // dca->packet for the whole DCAContext).
        self.core.packet = self.packet;
        // Filter the frame
        let pending = if self.packet & pkt::DCA_PACKET_LBR != 0 {
            let (rate, planes) = self.lbr.lbr_filter_frame()?;
            PendingFrame {
                sample_rate: rate,
                planes_f32: planes,
                planes_s32: Vec::new(),
                bits_per_sample: 0,
                pts,
            }
        } else if self.packet & pkt::DCA_PACKET_XLL != 0 {
            #[allow(unused_assignments)]
            let mut out: Option<PendingFrame> = None;

            if self.packet & pkt::DCA_PACKET_CORE != 0 {
                let mut x96_synth: i32 = -1;

                // Enable X96 synthesis if needed
                if self.xll.chset[0].freq == 96000 && self.core.sample_rate == 48000 {
                    x96_synth = 1;
                }

                self.core.core_filter_fixed(x96_synth)?;

                // Force lossy downmixed output on the first core frame filtered.
                if (prev_packet & pkt::DCA_PACKET_RESIDUAL) == 0
                    && self.xll.nreschsets > 0
                    && self.xll.nchsets > 1
                {
                    self.packet |= pkt::DCA_PACKET_RECOVERY;
                }

                // Set 'residual ok' flag for the next frame
                self.packet |= pkt::DCA_PACKET_RESIDUAL;
            }

            let mut buffers = self.xll_buffers.take().unwrap_or_default();
            match self.xll.xll_filter_frame(&mut buffers, &mut self.core) {
                Ok((rate, planes)) => {
                    out = Some(PendingFrame {
                        sample_rate: rate,
                        planes_f32: Vec::new(),
                        planes_s32: planes,
                        bits_per_sample: self.xll.chset[0].storage_bit_res as u32,
                        pts,
                    });
                }
                Err(e) => {
                    // Fall back to core unless hard error
                    if self.packet & pkt::DCA_PACKET_CORE == 0 {
                        return Err(xll_err_msg(e));
                    }
                    if !matches!(e, XllError::InvalidData) {
                        return Err(xll_err_msg(e));
                    }
                    let (rate, planes) = self.core.filter_frame_float()?;
                    out = Some(PendingFrame {
                        sample_rate: rate,
                        planes_f32: planes,
                        planes_s32: Vec::new(),
                        bits_per_sample: 0,
                        pts,
                    });
                }
            }

            if self.xll_buffers.is_none() {
                self.xll_buffers = Some(buffers);
            }
            match out {
                Some(p) => p,
                None => return Err("XLL produced no frame"),
            }
        } else if self.packet & pkt::DCA_PACKET_CORE != 0 {
            // Core-only frame: FFmpeg uses fixed point when falling back
            // from XLL, float otherwise.
            let was_fixed = self.core.filter_mode & dca::DCA_FILTER_MODE_FIXED != 0;
            let frame = if was_fixed {
                let (rate, planes) = self.core.filter_frame_fixed(false)?;
                PendingFrame {
                    sample_rate: rate,
                    planes_f32: Vec::new(),
                    planes_s32: planes,
                    bits_per_sample: 24,
                    pts,
                }
            } else {
                let (rate, planes) = self.core.filter_frame_float()?;
                PendingFrame {
                    sample_rate: rate,
                    planes_f32: planes,
                    planes_s32: Vec::new(),
                    bits_per_sample: 0,
                    pts,
                }
            };
            if self.core.filter_mode & dca::DCA_FILTER_MODE_FIXED != 0 {
                self.packet |= pkt::DCA_PACKET_RESIDUAL;
            }
            frame
        } else {
            return Err("no valid DCA sub-stream found");
        };

        self.pending = Some(pending);
        Ok(true)
    }

    /// `dcadec_flush`.
    pub fn flush(&mut self) {
        self.core.core_flush();
        self.xll.xll_flush();
        self.lbr.lbr_flush();
        self.packet &= pkt::DCA_PACKET_MASK;
        self.pending = None;
        self.xll_buffers = None;
    }
}

impl Default for DcaDecoder {
    fn default() -> Self {
        Self::new()
    }
}

fn xll_err_msg(e: XllError) -> &'static str {
    match e {
        XllError::Again => "XLL sync not found",
        XllError::InvalidData => "invalid XLL data",
        XllError::Unsupported => "unsupported XLL feature",
        XllError::Invalid => "invalid XLL frame",
    }
}
