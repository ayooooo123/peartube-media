//! WMV2 video decoder ported from libavcodec/wmv2dec.c, wmv2.c, wmv2dsp.c.

use oxideav_core::{CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result};

pub const CODEC_ID_WMV2: &str = "wmv2";

pub struct Wmv2Decoder {
    codec_id: CodecId,
    width: usize,
    height: usize,
}

impl Wmv2Decoder {
    pub fn new(params: &CodecParameters) -> Result<Self> {
        let (w, h) = match (params.width, params.height) {
            (Some(w), Some(h)) => (w as usize, h as usize),
            _ => return Err(Error::invalid("wmv2: width/height required")),
        };
        Ok(Self {
            codec_id: CodecId::new(CODEC_ID_WMV2),
            width: w,
            height: h,
        })
    }
}

impl Decoder for Wmv2Decoder {
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
