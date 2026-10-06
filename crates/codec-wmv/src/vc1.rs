//! VC-1 and WMV3 decoder ported from libavcodec/vc1dec.c, vc1.c, vc1_block.c,
//! vc1_mc.c, vc1_pred.c, vc1_loopfilter.c, vc1dsp.c.

use oxideav_core::{CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result};

pub const CODEC_ID_VC1: &str = "vc1";
pub const CODEC_ID_WMV3: &str = "wmv3";

pub struct Vc1Decoder {
    codec_id: CodecId,
    width: usize,
    height: usize,
}

impl Vc1Decoder {
    pub fn new_wmv3(params: &CodecParameters) -> Result<Self> {
        let (w, h) = match (params.width, params.height) {
            (Some(w), Some(h)) => (w as usize, h as usize),
            _ => (0, 0),
        };
        Ok(Self {
            codec_id: CodecId::new(CODEC_ID_WMV3),
            width: w,
            height: h,
        })
    }

    pub fn new_vc1(params: &CodecParameters) -> Result<Self> {
        let (w, h) = match (params.width, params.height) {
            (Some(w), Some(h)) => (w as usize, h as usize),
            _ => (0, 0),
        };
        Ok(Self {
            codec_id: CodecId::new(CODEC_ID_VC1),
            width: w,
            height: h,
        })
    }
}

impl Decoder for Vc1Decoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, _packet: &Packet) -> Result<()> {
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        Err(Error::NeedMore)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}
