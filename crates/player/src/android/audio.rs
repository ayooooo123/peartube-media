use super::clock::{AudioClock, AudioClockState, SendAudioStream};
use crate::audio_mix::StereoMix;
use crate::backend::{AudioOutputMode, AudioSink, Clock, PcmFormat, PcmSlice, SinkError};
use crate::clock::current_monotonic_ns;
use crate::iec61937::{Iec61937Codec, Iec61937Packetizer};
use ndk::audio::{AudioDirection, AudioFormat, AudioPerformanceMode, AudioStreamBuilder};
use oxideav_core::{ChannelLayout, ChannelPosition};
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;

const AAUDIO_FORMAT_PCM_I24_PACKED: i32 = ndk_sys::AAUDIO_FORMAT_PCM_I24_PACKED as i32;
const AAUDIO_FORMAT_PCM_I32: i32 = ndk_sys::AAUDIO_FORMAT_PCM_I32 as i32;
const AAUDIO_FORMAT_IEC61937: i32 = ndk_sys::AAUDIO_FORMAT_IEC61937 as i32;

fn aaudio_channel_mask(layout: ChannelLayout) -> Option<u32> {
    let n = usize::from(layout.channel_count());
    if n <= 2 {
        return None;
    }
    let mut mask = 0u32;
    for i in 0..n {
        let bit = match layout.position(i)? {
            ChannelPosition::FrontLeft => 1 << 0,
            ChannelPosition::FrontRight => 1 << 1,
            ChannelPosition::FrontCenter => 1 << 2,
            ChannelPosition::LowFrequency => 1 << 3,
            ChannelPosition::BackLeft => 1 << 4,
            ChannelPosition::BackRight => 1 << 5,
            ChannelPosition::FrontLeftOfCenter => 1 << 6,
            ChannelPosition::FrontRightOfCenter => 1 << 7,
            ChannelPosition::BackCenter => 1 << 8,
            ChannelPosition::SideLeft => 1 << 9,
            ChannelPosition::SideRight => 1 << 10,
            ChannelPosition::TopFrontLeft => 1 << 12,
            ChannelPosition::TopFrontRight => 1 << 14,
            ChannelPosition::TopBackLeft => 1 << 15,
            ChannelPosition::TopBackRight => 1 << 17,
            _ => return None,
        };
        mask |= bit;
    }
    Some(mask)
}

unsafe fn builder_set_raw_format(builder: &mut AudioStreamBuilder, raw_fmt: i32) {
    type FnSetFmt = unsafe extern "C" fn(*mut std::ffi::c_void, i32);
    let lib = unsafe { libc::dlopen(c"libaaudio.so".as_ptr(), libc::RTLD_NOW) };
    if !lib.is_null() {
        let sym = unsafe { libc::dlsym(lib, c"AAudioStreamBuilder_setFormat".as_ptr()) };
        if !sym.is_null() {
            let f: FnSetFmt = unsafe { std::mem::transmute(sym) };
            let raw_ptr = unsafe { *(builder as *mut AudioStreamBuilder as *mut *mut std::ffi::c_void) };
            unsafe { f(raw_ptr, raw_fmt) };
        }
    }
}

unsafe fn builder_set_channel_mask(builder: &mut AudioStreamBuilder, mask: u32) -> bool {
    type FnSetMask = unsafe extern "C" fn(*mut std::ffi::c_void, u32);
    let lib = unsafe { libc::dlopen(c"libaaudio.so".as_ptr(), libc::RTLD_NOW) };
    if lib.is_null() {
        return false;
    }
    let sym = unsafe { libc::dlsym(lib, c"AAudioStreamBuilder_setChannelMask".as_ptr()) };
    if sym.is_null() {
        return false;
    }
    let f: FnSetMask = unsafe { std::mem::transmute(sym) };
    let raw_ptr = unsafe { *(builder as *mut AudioStreamBuilder as *mut *mut std::ffi::c_void) };
    unsafe { f(raw_ptr, mask) };
    true
}

pub struct AndroidAudioSink {
    clock: AudioClock,
    sample_rate: u32,
    channels: u16,
    output_channels: u16,
    output_format: PcmFormat,
    mode: AudioOutputMode,
    layout: ChannelLayout,
    mix: Option<StereoMix>,
    stereo: [f32; 2048],
    iec: Option<Iec61937Packetizer>,
    iec_buf: Vec<u8>,
    iec_carrier_rate: u32,
    iec_carrier_channels: u16,
    suspended: bool,
}

impl AndroidAudioSink {
    pub fn new() -> (Self, Arc<dyn Clock>) {
        let clock = AudioClock { inner: Arc::new(Mutex::new(AudioClockState::new())) };
        let clock_dyn = Arc::new(clock.clone());
        (
            Self {
                clock,
                sample_rate: 0,
                channels: 0,
                output_channels: 0,
                output_format: PcmFormat::F32,
                mode: AudioOutputMode::Auto,
                layout: ChannelLayout::Stereo,
                mix: None,
                stereo: [0.0; 2048],
                iec: None,
                iec_buf: Vec::new(),
                iec_carrier_rate: 0,
                iec_carrier_channels: 0,
                suspended: false,
            },
            clock_dyn,
        )
    }

    pub fn suspend(&mut self) {
        let old = {
            let mut state = self.clock.inner.lock();
            if state.playing {
                state.observe();
            }
            state.playing = false;
            state.stream.take()
        };
        self.suspended = true;
        if let Some(stream) = old {
            let _ = stream.0.request_stop();
        }
    }

    pub fn resume(&mut self) -> Result<(), SinkError> {
        self.suspended = false;
        if let Some(pktzr) = &self.iec {
            let codec = pktzr.codec();
            self.open_iec_stream(codec, self.sample_rate)?;
            return Ok(());
        }
        if self.sample_rate != 0 {
            self.create_pcm_stream(self.sample_rate, self.layout, self.output_format, self.mode)?;
        }
        Ok(())
    }

    fn install_stream(&self, stream: ndk::audio::AudioStream, clock_rate: u32) {
        let old = {
            let mut state = self.clock.inner.lock();
            let old = state.stream.take();
            *state = AudioClockState::new();
            state.rate = clock_rate;
            state.stream = Some(Arc::new(SendAudioStream(stream)));
            old
        };
        if let Some(stream) = old {
            let _ = stream.0.request_stop();
        }
    }

    fn create_pcm_stream(
        &mut self,
        rate: u32,
        layout: ChannelLayout,
        format: PcmFormat,
        mode: AudioOutputMode,
    ) -> Result<(), SinkError> {
        let channels = layout.channel_count();
        let want_multichannel = matches!(mode, AudioOutputMode::BitPerfectPcm | AudioOutputMode::Passthrough)
            && channels > 2;
        let mask = if want_multichannel { aaudio_channel_mask(layout) } else { None };

        if matches!(format, PcmFormat::S16 | PcmFormat::S24 | PcmFormat::S32) || mask.is_some() {
            if let Ok(mut b) = AudioStreamBuilder::new() {
                b = b.sample_rate(rate as i32).direction(AudioDirection::Output);
                b = match mode {
                    AudioOutputMode::BitPerfectPcm | AudioOutputMode::Passthrough => {
                        b.performance_mode(AudioPerformanceMode::None)
                    }
                    _ => b.performance_mode(AudioPerformanceMode::LowLatency),
                };
                let out_ch = if let Some(m) = mask {
                    if unsafe { builder_set_channel_mask(&mut b, m) } {
                        channels
                    } else {
                        b = b.channel_count(i32::from(channels.min(2)));
                        channels.min(2)
                    }
                } else {
                    b = b.channel_count(i32::from(channels.min(2)));
                    channels.min(2)
                };
                let target_fmt = if out_ch < channels {
                    b = b.format(AudioFormat::PCM_Float);
                    PcmFormat::F32
                } else {
                    match format {
                        PcmFormat::S16 => {
                            b = b.format(AudioFormat::PCM_I16);
                            PcmFormat::S16
                        }
                        PcmFormat::S24 => {
                            unsafe { builder_set_raw_format(&mut b, AAUDIO_FORMAT_PCM_I24_PACKED) };
                            PcmFormat::S24
                        }
                        PcmFormat::S32 => {
                            unsafe { builder_set_raw_format(&mut b, AAUDIO_FORMAT_PCM_I32) };
                            PcmFormat::S32
                        }
                        _ => {
                            b = b.format(AudioFormat::PCM_Float);
                            PcmFormat::F32
                        }
                    }
                };
                if let Ok(stream) = b.open_stream() {
                    self.output_channels = out_ch;
                    self.output_format = target_fmt;
                    self.mix = if out_ch < channels {
                        Some(StereoMix::new(layout).map_err(|e| SinkError::Fatal(e.into()))?)
                    } else {
                        None
                    };
                    self.install_stream(stream, rate);
                    return Ok(());
                }
            }
        }

        let out_ch = channels.min(2);
        let stream = AudioStreamBuilder::new()
            .map_err(|e| SinkError::Fatal(format!("AAudio builder: {e:?}")))?
            .channel_count(i32::from(out_ch))
            .sample_rate(rate as i32)
            .format(AudioFormat::PCM_Float)
            .direction(AudioDirection::Output)
            .performance_mode(AudioPerformanceMode::LowLatency)
            .open_stream()
            .map_err(|e| SinkError::Fatal(format!("AAudio open: {e:?}")))?;
        self.output_channels = out_ch;
        self.output_format = PcmFormat::F32;
        self.mix = if out_ch < channels {
            Some(StereoMix::new(layout).map_err(|e| SinkError::Fatal(e.into()))?)
        } else {
            None
        };
        self.install_stream(stream, rate);
        Ok(())
    }

    fn open_iec_stream(&mut self, codec: Iec61937Codec, source_rate: u32) -> Result<bool, SinkError> {
        let (carrier_rate, carrier_ch) = codec.carrier_shape(source_rate);
        let Ok(mut b) = AudioStreamBuilder::new() else {
            return Ok(false);
        };
        b = b
            .sample_rate(carrier_rate as i32)
            .channel_count(i32::from(carrier_ch))
            .direction(AudioDirection::Output)
            .performance_mode(AudioPerformanceMode::None);
        unsafe { builder_set_raw_format(&mut b, AAUDIO_FORMAT_IEC61937) };
        let Ok(stream) = b.open_stream() else {
            return Ok(false);
        };
        // A stream opened as anything but IEC 61937 would play the bitstream as PCM noise.
        if i32::from(stream.format()) != AAUDIO_FORMAT_IEC61937 {
            return Ok(false);
        }
        self.iec_carrier_rate = carrier_rate;
        self.iec_carrier_channels = carrier_ch;
        self.install_stream(stream, carrier_rate);
        Ok(true)
    }

    fn flush_iec_bytes(&mut self, pts: Duration) -> Result<bool, SinkError> {
        if self.iec_buf.is_empty() {
            return Ok(true);
        }
        let (stream, playing) = {
            let state = self.clock.inner.lock();
            (state.stream.clone().ok_or(SinkError::Unavailable)?, state.playing)
        };
        let frame_bytes = usize::from(self.iec_carrier_channels.max(1)) * 2;
        let frames = self.iec_buf.len() / frame_bytes;
        if frames == 0 {
            return Ok(true);
        }
        let before = stream.0.frames_written();
        let timeout = if playing { 20_000_000 } else { 0 };
        let res = unsafe {
            stream
                .0
                .write(self.iec_buf.as_ptr().cast(), frames as i32, timeout)
        };
        let written = match res {
            Ok(n) => n as usize,
            Err(ndk::audio::AudioError::__Unknown(n)) if n > 0 => n as usize,
            Err(e) => return Err(SinkError::Fatal(format!("AAudio IEC61937 write: {e:?}"))),
        };
        if written == 0 {
            return Ok(false);
        }
        self.iec_buf.drain(..written * frame_bytes);
        let mut state = self.clock.inner.lock();
        if state.base.is_none() {
            state.base = Some(pts);
            state.base_frame = before;
            state.started_ns = current_monotonic_ns();
        }
        state.written += written as u64;
        Ok(self.iec_buf.is_empty())
    }
}

impl Drop for AndroidAudioSink {
    fn drop(&mut self) {
        // Video may retain the clock after the audio lane exits.
        self.suspend();
    }
}

impl AudioSink for AndroidAudioSink {
    fn open(&mut self, sample_rate: u32, layout: ChannelLayout) -> Result<(), SinkError> {
        self.open_pcm(sample_rate, layout, PcmFormat::F32, AudioOutputMode::Auto)
    }

    fn open_pcm(
        &mut self,
        sample_rate: u32,
        layout: ChannelLayout,
        format: PcmFormat,
        mode: AudioOutputMode,
    ) -> Result<(), SinkError> {
        let channels = layout.channel_count();
        if sample_rate == 0 || sample_rate > i32::MAX as u32 || channels == 0 || channels > 64 {
            return Err(SinkError::Fatal(format!(
                "invalid audio format {sample_rate} Hz / {channels} channels"
            )));
        }
        self.iec = None;
        self.iec_buf.clear();
        self.sample_rate = sample_rate;
        self.channels = channels;
        self.layout = layout;
        self.mode = mode;
        if !self.suspended {
            self.create_pcm_stream(sample_rate, layout, format, mode)?;
        } else {
            self.output_format = format;
        }
        Ok(())
    }

    fn write(&mut self, pcm: &[f32], pts: Duration) -> Result<usize, SinkError> {
        let channels = self.channels.max(1);
        let bytes = unsafe {
            std::slice::from_raw_parts(pcm.as_ptr().cast::<u8>(), pcm.len() * 4)
        };
        self.write_pcm(
            PcmSlice {
                format: PcmFormat::F32,
                channels,
                data: bytes,
            },
            pts,
        )
    }

    fn write_pcm(&mut self, pcm: PcmSlice<'_>, pts: Duration) -> Result<usize, SinkError> {
        if self.suspended {
            return Err(SinkError::Unavailable);
        }
        let (stream, playing) = {
            let state = self.clock.inner.lock();
            (state.stream.clone().ok_or(SinkError::Unavailable)?, state.playing)
        };
        let total_frames = pcm.frames();
        if total_frames == 0 {
            return Ok(0);
        }
        let f32_fallback;
        let (ptr, frames): (*const std::ffi::c_void, usize) = if let Some(mix) = &self.mix {
            let n = mix.mix_pcm(pcm, &mut self.stereo);
            (self.stereo.as_ptr().cast(), n)
        } else if pcm.format == self.output_format {
            (pcm.data.as_ptr().cast(), total_frames)
        } else {
            f32_fallback = pcm.to_f32_vec();
            (f32_fallback.as_ptr().cast(), total_frames)
        };
        if frames == 0 {
            return Ok(0);
        }
        let before = stream.0.frames_written();
        let timeout = if playing { 20_000_000 } else { 0 };
        let result = unsafe {
            stream
                .0
                .write(ptr, frames.min(i32::MAX as usize) as i32, timeout)
        };
        let written = match result {
            Ok(n) => n as usize,
            Err(ndk::audio::AudioError::__Unknown(n)) if n > 0 => n as usize,
            Err(e) => return Err(SinkError::Fatal(format!("AAudio write: {e:?}"))),
        };
        let mut state = self.clock.inner.lock();
        if written > 0 && state.base.is_none() {
            state.base = Some(pts);
            state.base_frame = before;
            state.started_ns = current_monotonic_ns();
        }
        state.written += written as u64;
        #[cfg(debug_assertions)]
        if super::clock::tracing() {
            if let Ok(ts) = stream.0.timestamp(ndk::audio::Clockid::Monotonic) {
                eprintln!(
                    "ENGINE_SYNC audio base_pts_ns={} base_frame={} written={} frame={} mono_ns={} rate={}",
                    state.base.unwrap_or_default().as_nanos(),
                    state.base_frame,
                    state.written,
                    ts.frame_position,
                    ts.time_nanoseconds,
                    state.rate
                );
            }
        }
        Ok(written)
    }

    fn open_compressed_with_mode(
        &mut self,
        params: &oxideav_core::CodecParameters,
        _first_packet: &[u8],
        mode: AudioOutputMode,
    ) -> Result<bool, SinkError> {
        if mode != AudioOutputMode::Passthrough {
            return Ok(false);
        }
        let source_rate = params.sample_rate.unwrap_or(48_000);
        let Some(codec) = Iec61937Codec::from_codec_id(params.codec_id.as_str(), true) else {
            return Ok(false);
        };
        if self.suspended {
            return Ok(false);
        }
        if !self.open_iec_stream(codec, source_rate)? {
            if matches!(codec, Iec61937Codec::DtsHd { .. })
                && self.open_iec_stream(Iec61937Codec::DtsCore, source_rate)?
            {
                self.sample_rate = source_rate;
                self.iec = Some(Iec61937Packetizer::new(Iec61937Codec::DtsCore));
                self.iec_buf.clear();
                return Ok(true);
            }
            return Ok(false);
        }
        self.sample_rate = source_rate;
        self.iec = Some(Iec61937Packetizer::new(codec));
        self.iec_buf.clear();
        Ok(true)
    }

    fn write_packet(&mut self, packet: &[u8], pts: Duration, _frames: u32) -> Result<bool, SinkError> {
        if self.suspended {
            return Err(SinkError::Unavailable);
        }
        if !self.iec_buf.is_empty() && !self.flush_iec_bytes(pts)? {
            return Ok(false);
        }
        let Some(pktzr) = &mut self.iec else {
            return Err(SinkError::Fatal("AAudio IEC61937 sink not open".into()));
        };
        pktzr
            .push_packet(packet, &mut self.iec_buf)
            .map_err(|e| SinkError::Fatal(e.to_string()))?;
        let _ = self.flush_iec_bytes(pts)?;
        Ok(true)
    }

    fn drain(&mut self) -> Result<bool, SinkError> {
        if let Some(pktzr) = &mut self.iec {
            pktzr
                .drain(&mut self.iec_buf)
                .map_err(|e| SinkError::Fatal(e.to_string()))?;
        }
        if !self.iec_buf.is_empty() {
            return self.flush_iec_bytes(Duration::ZERO);
        }
        Ok(true)
    }

    fn play(&mut self) {
        // Every master-clock reader takes the clock state lock: update the
        // state under it, make the platform call after releasing it.
        let stream = {
            let mut state = self.clock.inner.lock();
            if state.playing { return; }
            state.started_ns = current_monotonic_ns();
            state.playing = true;
            state.stream.clone()
        };
        if let Some(stream) = stream { let _ = stream.0.request_start(); }
    }

    fn pause(&mut self) {
        let stream = {
            let mut state = self.clock.inner.lock();
            if !state.playing { return; }
            state.observe();
            state.playing = false;
            state.stream.clone()
        };
        if let Some(stream) = stream { let _ = stream.0.request_pause(); }
    }

    fn flush(&mut self) {
        self.iec_buf.clear();
        if let Some(pktzr) = &mut self.iec {
            pktzr.reset();
        }
        // Recreating makes seek atomic with respect to AAudio's async state
        // transitions; an old timestamp can never anchor the new samples.
        if self.sample_rate != 0 && !self.suspended {
            let playing = self.clock.inner.lock().playing;
            let reopened = if let Some(pktzr) = &self.iec {
                let codec = pktzr.codec();
                self.open_iec_stream(codec, self.sample_rate).unwrap_or(false)
            } else {
                self.create_pcm_stream(self.sample_rate, self.layout, self.output_format, self.mode)
                    .is_ok()
            };
            if reopened && playing {
                self.play();
            }
        }
    }

    fn clock(&self) -> Arc<dyn Clock> { Arc::new(self.clock.clone()) }
}
