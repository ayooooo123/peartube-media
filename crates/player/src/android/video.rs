use super::backend::BackendShared;
use super::clock::monotonic_now_ns;
use crate::backend::{Clock, SinkError, VideoSink};
use ndk::hardware_buffer_format::HardwareBufferFormat;
use ndk::media::media_codec::{
    DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection,
};
use ndk::media::media_format::MediaFormat;
use ndk::native_window::NativeWindow;
use oxideav_core::{CodecParameters, Packet, PixelFormat, VideoFrame};
use crate::annexb::convert_packet_to_annex_b;
use oxideav_pixfmt::FrameInfo;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub struct SendMediaCodec(pub MediaCodec);
unsafe impl Send for SendMediaCodec {}
unsafe impl Sync for SendMediaCodec {}

pub struct AndroidVideoSink {
    backend: Arc<BackendShared>,
    clock: Arc<dyn Clock>,
    codec: Option<Arc<SendMediaCodec>>,
    output_thread: Option<JoinHandle<()>>,
    /// The output thread's completion receiver; teardown waits on it
    /// briefly instead of hanging on `join` for a wedged codec. The
    /// thread sends on drop via `DoneSignal`.
    output_done: Option<std::sync::mpsc::Receiver<()>>,
    /// Stop flags of live output-thread generations. Teardown sets every
    /// flag; each reopen creates a fresh one so an abandoned older thread
    /// can never be resurrected by a later reopen resetting its flag.
    stop_output_signal: Mutex<Vec<Arc<AtomicBool>>>,
    midstream_error: Arc<Mutex<Option<String>>>,
    is_playing: Arc<AtomicBool>,
    /// Length-prefix size taken from avcC/hvcC.
    nal_length_size: usize,
    /// How the demuxer frames this stream's packets: `true` = Annex B start
    /// codes (extradata was Annex B), `false` = length-prefixed AVCC/HVCC.
    /// Decided once from the extradata, never sniffed per packet: a valid
    /// 4-byte AVCC NAL length of 256–511 starts with `00 00 01`, which a
    /// sniffer would misread as Annex B.
    packets_are_annex_b: bool,
    software_frame_info: Option<(PixelFormat, u32, u32)>,
    last_compressed_params: Option<CodecParameters>,
    is_compressed: bool,
    /// Set when the codec was torn down for window loss/suspend: push_packet
    /// holds packets until the next keyframe instead of erroring, so the
    /// rebuilt codec starts from a clean point.
    awaiting_keyframe: bool,
    /// The type-derived decoder stalled and we retried on the software one.
    tried_software_decoder: bool,
    /// The type-derived decoder for this stream proved unusable (stalled);
    /// later re-opens (new window, resume) go straight to the software one.
    prefer_software: bool,
}

impl AndroidVideoSink {
    pub fn new(backend: Arc<BackendShared>, clock: Arc<dyn Clock>) -> Self {
        Self {
            backend,
            clock,
            codec: None,
            output_thread: None,
            output_done: None,
            stop_output_signal: Mutex::new(Vec::new()),
            midstream_error: Arc::new(Mutex::new(None)),
            is_playing: Arc::new(AtomicBool::new(true)),
            nal_length_size: 4,
            packets_are_annex_b: false,
            software_frame_info: None,
            last_compressed_params: None,
            is_compressed: false,
            awaiting_keyframe: false,
            tried_software_decoder: false,
            prefer_software: false,
        }
    }

    pub fn teardown_codec(&mut self) {
        // Stop every live output-thread generation. Reopens push a fresh
        // flag per generation and never reset an old one, so a thread that
        // unblocks after teardown always sees `true` and exits instead of
        // resuming on its retired codec.
        for flag in self.stop_output_signal.lock().iter() {
            flag.store(true, Ordering::SeqCst);
        }
        if let Some(codec) = self.codec.take() {
            let _ = codec.0.stop();
            // Dropping codec releases AMediaCodec (aborts a wedged
            // dequeue call in the output thread).
        }
        if let Some(handle) = self.output_thread.take() {
            // Wait for the output thread: on a healthy codec it exits
            // within one dequeue timeout. A wedged codec can block its
            // dequeue in binder; the wait below still applies so this call
            // returns, but the thread's generation flag stays `true` and it
            // exits as soon as its pending call aborts.
            let deadline = Instant::now() + Duration::from_millis(500);
            while Instant::now() < deadline {
                match self
                    .output_done
                    .as_ref()
                    .expect("output thread implies a done channel")
                    .recv_timeout(Duration::from_millis(20))
                {
                    Ok(()) => {
                        let _ = handle.join();
                        break;
                    }
                    // Sender dropped: the output thread exited.
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        let _ = handle.join();
                        break;
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                }
                if Instant::now() >= deadline {
                    break;
                }
            }
            self.output_done = None;
            // Every generation is now stopped (or wedged with a `true` flag
            // it holds a private Arc of — it exits when its binder call
            // aborts). Drop our Arcs so the list doesn't grow across
            // reopens; a wedged thread is unaffected because it holds its
            // own clone, and that clone reads `true`.
            self.stop_output_signal.lock().clear();
        }
    }

    pub fn suspend(&mut self) {
        self.teardown_codec();
    }

    /// The video window went away (`set_video_window(None)`): release the
    /// codec and join its output thread. Called with the sink's mutex held,
    /// so no push is running and none can start; when this returns, nothing
    /// references the old window and pushes report `Unavailable` until a new
    /// window arrives (the codec is rebuilt from the next keyframe).
    pub fn detach_codec_from_window(&mut self) {
        self.teardown_codec();
        self.awaiting_keyframe = self.is_compressed;
    }

    /// A new video window arrived (`set_video_window(Some(w))`): if a
    /// compressed stream had lost its codec, rebuild it on the new window.
    /// Decoding resumes on the next keyframe the engine pushes.
    pub fn on_window_available(&mut self) {
        if self.is_compressed
            && self.codec.is_none()
            && !self.backend.is_suspended.load(Ordering::SeqCst)
        {
            if let Some(params) = self.last_compressed_params.clone() {
                self.open_compressed(&params);
            }
        }
    }

    pub fn resume(&mut self) {
        if self.is_compressed {
            if let Some(params) = self.last_compressed_params.clone() {
                self.open_compressed(&params);
            }
        }
    }

    /// Test hook: prefer the platform's software decoder for this stream
    /// (`c2.android.*`) instead of the highest-ranked decoder for the type.
    /// The emulator's vendor decoders can wedge; a real device never needs
    /// this.
    pub fn prefer_software_decoder(&mut self, prefer: bool) {
        self.prefer_software = prefer;
    }

    fn current_window(&self) -> Option<NativeWindow> {
        self.backend.video_window.read().clone()
    }

    /// Builds and starts the codec. `force_software` uses the platform's
    /// software decoder ("c2.android.*") instead of the highest-ranked
    /// decoder for the type — the retry path when that one never accepts
    /// input (the emulator's vendor decoders can do this).
    fn open_compressed_inner(&mut self, params: &CodecParameters, force_software: bool) -> bool {
        self.teardown_codec();
        self.is_compressed = true;
        self.last_compressed_params = Some(params.clone());

        if self.backend.is_suspended.load(Ordering::SeqCst) {
            return false;
        }

        let window = match self.current_window() {
            Some(w) => w,
            None => return false,
        };

        let mime = match codec_id_to_mime(&params.codec_id.0) {
            Some(m) => m,
            None => return false,
        };

        let mut csd0 = Vec::new();
        let mut csd1 = Vec::new();
        let mut nal_len_size = 4;
        // Stream framing decided once, from the extradata itself.
        let mut packets_annex_b = false;

        if mime == "video/avc" {
            if !params.extradata.is_empty() {
                // Annex B extradata carries the parameter sets inline with
                // start codes; avcC is the ISO/IEC 14496-15 record.
                packets_annex_b = params.extradata.starts_with(&[0, 0, 0, 1])
                    || params.extradata.starts_with(&[0, 0, 1]);
                if let Some((s0, s1, nls)) = parse_avcc_to_annex_b(&params.extradata) {
                    csd0 = s0;
                    csd1 = s1;
                    nal_len_size = nls;
                }
            }
        } else if mime == "video/hevc" {
            if !params.extradata.is_empty() {
                packets_annex_b = params.extradata.starts_with(&[0, 0, 0, 1])
                    || params.extradata.starts_with(&[0, 0, 1]);
                if let Some((s0, nls)) = parse_hvcc_to_annex_b(&params.extradata) {
                    csd0 = s0;
                    nal_len_size = nls;
                }
            }
        } else if !params.extradata.is_empty() {
            csd0 = params.extradata.clone();
        }
        self.nal_length_size = nal_len_size;
        self.packets_are_annex_b = packets_annex_b;

        let codec = if force_software {
            match software_decoder_name(mime).and_then(MediaCodec::from_codec_name) {
                Some(c) => c,
                None => return false,
            }
        } else {
            match MediaCodec::from_decoder_type(mime) {
                Some(c) => c,
                None => return false,
            }
        };

        let mut format = MediaFormat::new();
        format.set_str("mime", mime);
        if let Some(w) = params.width {
            format.set_i32("width", w as i32);
        }
        if let Some(h) = params.height {
            format.set_i32("height", h as i32);
        }
        if !csd0.is_empty() {
            format.set_buffer("csd-0", &csd0);
        }
        if !csd1.is_empty() {
            format.set_buffer("csd-1", &csd1);
        }

        // A codec we just tore down releases the surface asynchronously
        // (`stop` can take seconds); configuring the next decoder on the
        // same window then fails with EINVAL ("already connected"). Retry
        // on a fresh codec for up to ~3 s.
        let mut configured = false;
        let mut codec_opt = Some(codec);
        for attempt in 0..6u32 {
            let codec = codec_opt.as_ref().unwrap();
            match codec.configure(&format, Some(&window), MediaCodecDirection::Decoder) {
                Ok(()) => {
                    configured = true;
                    break;
                }
                Err(_) if attempt < 5 => {
                    // Recreate: a failed configure leaves the codec unusable.
                    let mime_s = mime;
                    codec_opt = if force_software {
                        software_decoder_name(mime_s).and_then(MediaCodec::from_codec_name)
                    } else {
                        MediaCodec::from_decoder_type(mime_s)
                    };
                    if codec_opt.is_none() {
                        return false;
                    }
                    std::thread::sleep(Duration::from_millis(300));
                }
                Err(_) => return false,
            }
        }
        if !configured {
            return false;
        }
        let codec = codec_opt.unwrap();

        if codec.start().is_err() {
            return false;
        }

        let codec_arc = Arc::new(SendMediaCodec(codec));
        self.codec = Some(codec_arc.clone());
        // No flag reset: the new generation registered its own fresh flag
        // below; older generations keep their `true` flags.
        *self.midstream_error.lock() = None;
        // NOTE: `awaiting_keyframe` is deliberately NOT cleared here. When
        // the codec was rebuilt after a window loss, the next packet pushed
        // must still be a keyframe; the gate clears itself in `push_packet`
        // when a keyframe arrives. A fresh-open (no loss) enters with the
        // gate already false.

        // Output thread. Each generation gets its own stop flag: a reopen
        // must never reset the flag an abandoned (wedged) older thread is
        // watching — that would resurrect it on its retired codec.
        let thread_codec = codec_arc;
        let thread_clock = self.clock.clone();
        let thread_stop = Arc::new(AtomicBool::new(false));
        self.stop_output_signal.lock().push(thread_stop.clone());
        let thread_err = self.midstream_error.clone();
        let thread_playing = self.is_playing.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        self.output_done = Some(done_rx);

        let handle = std::thread::spawn(move || {
            let _done = DoneSignal(done_tx);
            while !thread_stop.load(Ordering::Relaxed) {
                if !thread_playing.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }

                match thread_codec
                    .0
                    .dequeue_output_buffer(Duration::from_millis(50))
                {
                    Ok(DequeuedOutputBufferInfoResult::Buffer(out_buf)) => {
                        let pts_us = out_buf.info().presentation_time_us();
                        let pts = Duration::from_micros(pts_us.max(0) as u64);
                        let target_mono_ns = thread_clock.monotonic_ns_at(pts);
                        let now_mono_ns = monotonic_now_ns();

                        if let Some(target_ns) = target_mono_ns {
                            if target_ns < now_mono_ns - 30_000_000 {
                                // Drop late buffer
                                let _ = thread_codec.0.release_output_buffer(out_buf, false);
                            } else {
                                let _ = thread_codec
                                    .0
                                    .release_output_buffer_at_time(out_buf, target_ns);
                            }
                        } else {
                            let _ = thread_codec.0.release_output_buffer(out_buf, true);
                        }
                    }
                    Ok(DequeuedOutputBufferInfoResult::TryAgainLater) => {}
                    Ok(DequeuedOutputBufferInfoResult::OutputFormatChanged) => {}
                    Ok(DequeuedOutputBufferInfoResult::OutputBuffersChanged) => {}
                    Err(e) => {
                        *thread_err.lock() =
                            Some(format!("MediaCodec dequeue_output_buffer error: {e:?}"));
                        break;
                    }
                }
            }
        });

        self.output_thread = Some(handle);
        true
    }
}

/// Sends on the done channel when dropped, i.e. when the output thread's
/// loop exits (normally, on error, or after its pending binder call
/// finally aborts). The teardown half is the `Receiver`.
struct DoneSignal(std::sync::mpsc::Sender<()>);
impl Drop for DoneSignal {
    fn drop(&mut self) {
        // The receiver half may already be gone (teardown gave up); ignore.
        let _ = self.0.send(());
    }
}

impl Drop for AndroidVideoSink {
    fn drop(&mut self) {
        self.teardown_codec();
    }
}

/// Well-known names of Android's software (c2.android.*) decoders, used as
/// a fallback when the type-derived (highest-ranked) decoder is unusable.
fn software_decoder_name(mime: &str) -> Option<&'static str> {
    match mime {
        "video/avc" => Some("c2.android.avc.decoder"),
        "video/hevc" => Some("c2.android.hevc.decoder"),
        "video/x-vnd.on2.vp8" => Some("c2.android.vp8.decoder"),
        "video/x-vnd.on2.vp9" => Some("c2.android.vp9.decoder"),
        "video/av01" => Some("c2.android.av1.decoder"),
        "video/mp4v-es" => Some("c2.android.mpeg4.decoder"),
        "video/3gpp" => Some("c2.android.h263.decoder"),
        _ => None,
    }
}

fn codec_id_to_mime(codec_id: &str) -> Option<&'static str> {
    match codec_id.to_ascii_lowercase().as_str() {
        "h264" | "avc" => Some("video/avc"),
        "hevc" | "h265" => Some("video/hevc"),
        "vp8" => Some("video/x-vnd.on2.vp8"),
        "vp9" => Some("video/x-vnd.on2.vp9"),
        "av1" => Some("video/av01"),
        "mpeg4" | "mp4v-es" => Some("video/mp4v-es"),
        "h263" | "3gpp" => Some("video/3gpp"),
        "mpeg2video" | "mpeg2" => Some("video/mpeg2"),
        _ => None,
    }
}

fn is_annex_b(data: &[u8]) -> bool {
    data.starts_with(&[0, 0, 0, 1]) || data.starts_with(&[0, 0, 1])
}

fn parse_avcc_to_annex_b(data: &[u8]) -> Option<(Vec<u8>, Vec<u8>, usize)> {
    if is_annex_b(data) {
        return Some((data.to_vec(), Vec::new(), 4));
    }
    if data.len() < 7 || data[0] != 1 {
        return None;
    }
    let nal_length_size = ((data[4] & 0x03) + 1) as usize;
    let num_sps = (data[5] & 0x1F) as usize;
    let mut offset = 6;
    let mut csd0 = Vec::new();

    for _ in 0..num_sps {
        if offset + 2 > data.len() {
            return None;
        }
        let sps_len = u16::from_be_bytes([data[offset], data[offset + 1]]) as usize;
        offset += 2;
        if offset + sps_len > data.len() {
            return None;
        }
        csd0.extend_from_slice(&[0, 0, 0, 1]);
        csd0.extend_from_slice(&data[offset..offset + sps_len]);
        offset += sps_len;
    }

    let mut csd1 = Vec::new();
    if offset < data.len() {
        let num_pps = data[offset] as usize;
        offset += 1;
        for _ in 0..num_pps {
            if offset + 2 > data.len() {
                return None;
            }
            let pps_len = u16::from_be_bytes([data[offset], data[offset + 1]]) as usize;
            offset += 2;
            if offset + pps_len > data.len() {
                return None;
            }
            csd1.extend_from_slice(&[0, 0, 0, 1]);
            csd1.extend_from_slice(&data[offset..offset + pps_len]);
            offset += pps_len;
        }
    }

    Some((csd0, csd1, nal_length_size))
}

fn parse_hvcc_to_annex_b(data: &[u8]) -> Option<(Vec<u8>, usize)> {
    if is_annex_b(data) {
        return Some((data.to_vec(), 4));
    }
    if data.len() < 23 || data[0] != 1 {
        return None;
    }
    let nal_length_size = ((data[21] & 0x03) + 1) as usize;
    let num_arrays = data[22] as usize;
    let mut offset = 23;
    let mut csd0 = Vec::new();

    for _ in 0..num_arrays {
        if offset + 3 > data.len() {
            return None;
        }
        let num_nalus = u16::from_be_bytes([data[offset + 1], data[offset + 2]]) as usize;
        offset += 3;
        for _ in 0..num_nalus {
            if offset + 2 > data.len() {
                return None;
            }
            let nalu_len = u16::from_be_bytes([data[offset], data[offset + 1]]) as usize;
            offset += 2;
            if offset + nalu_len > data.len() {
                return None;
            }
            csd0.extend_from_slice(&[0, 0, 0, 1]);
            csd0.extend_from_slice(&data[offset..offset + nalu_len]);
            offset += nalu_len;
        }
    }

    Some((csd0, nal_length_size))
}

impl VideoSink for AndroidVideoSink {
    fn open_compressed(&mut self, params: &CodecParameters) -> bool {
        let force = self.prefer_software;
        if !force {
            self.tried_software_decoder = false;
        }
        self.open_compressed_inner(params, force)
    }

    fn push_packet(&mut self, packet: &Packet, pts: Duration) -> Result<(), SinkError> {
        if self.backend.is_suspended.load(Ordering::SeqCst) {
            return Err(SinkError::Unavailable);
        }
        if self.current_window().is_none() {
            return Err(SinkError::Unavailable);
        }
        // After a codec teardown (window loss, suspend, fallback), wait for
        // the next keyframe so the rebuilt codec starts from a clean point.
        if self.awaiting_keyframe {
            if packet.flags.keyframe {
                self.awaiting_keyframe = false;
            } else {
                return Ok(());
            }
        }
        let err_opt = self.midstream_error.lock().take();
        if let Some(err) = err_opt {
            self.teardown_codec();
            self.awaiting_keyframe = self.is_compressed;
            return Err(SinkError::Fallback(err));
        }

        let codec = match self.codec.clone() {
            Some(c) => c,
            None => return Err(SinkError::Fallback("decoder not active".into())),
        };

        // Framing comes from the extradata decision made at open time, not
        // from sniffing: a 4-byte AVCC NAL length of 256–511 starts with
        // `00 00 01` and would be misread as Annex B.
        let annex_b_data = if self.last_compressed_params.as_ref().map_or(false, |p| {
            let mime = codec_id_to_mime(&p.codec_id.0).unwrap_or("");
            mime == "video/avc" || mime == "video/hevc"
        }) && !self.packets_are_annex_b
        {
            convert_packet_to_annex_b(&packet.data, self.nal_length_size)
        } else {
            packet.data.clone()
        };

        // Dequeue an input buffer. A healthy decoder runs out of input
        // buffers while it holds frames for B-frame reordering
        // (output.delay up to 8): no output is released yet, so the output
        // thread cannot return input buffers either. That state is normal —
        // wait for the output thread to make progress rather than failing;
        // only give up (and allow the software-decoder retry) when NO output
        // was ever dequeued and the wait exceeds the stall budget.
        let start_dequeue = Instant::now();
        let stall_budget = Duration::from_secs(300);
        let mut buf = loop {
            let err_opt = self.midstream_error.lock().take();
            if let Some(err) = err_opt {
                self.teardown_codec();
                self.awaiting_keyframe = self.is_compressed;
                return Err(SinkError::Fallback(err));
            }

            match codec.0.dequeue_input_buffer(Duration::from_millis(20)) {
                Ok(DequeuedInputBufferResult::Buffer(buf)) => {
                    break buf;
                }
                Ok(DequeuedInputBufferResult::TryAgainLater) => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => {
                    self.teardown_codec();
                    self.awaiting_keyframe = self.is_compressed;
                    return Err(SinkError::Fallback(format!(
                        "dequeue_input_buffer error: {e:?}"
                    )));
                }
            }

            if start_dequeue.elapsed() < stall_budget {
                continue;
            }
            // Stalled past the budget: give up on this decoder.
            self.teardown_codec();
            self.awaiting_keyframe = self.is_compressed;
            if !self.tried_software_decoder {
                self.tried_software_decoder = true;
                self.prefer_software = true;
                if let Some(params) = self.last_compressed_params.clone() {
                    if self.open_compressed_inner(&params, true) {
                        return self.push_packet(packet, pts);
                    }
                }
            }
            return Err(SinkError::Fallback("input buffer dequeue timed out".into()));
        };

        let raw_dest = buf.buffer_mut();
        if raw_dest.len() < annex_b_data.len() {
            self.teardown_codec();
            self.awaiting_keyframe = self.is_compressed;
            return Err(SinkError::Fallback("input buffer capacity too small".into()));
        }

        unsafe {
            std::ptr::copy_nonoverlapping(
                annex_b_data.as_ptr(),
                raw_dest.as_mut_ptr().cast(),
                annex_b_data.len(),
            );
        }

        codec
            .0
            .queue_input_buffer(buf, 0, annex_b_data.len(), pts.as_micros() as u64, 0)
            .map_err(|e| {
                self.teardown_codec();
                self.awaiting_keyframe = self.is_compressed;
                SinkError::Fallback(format!("queue_input_buffer error: {e:?}"))
            })?;

        Ok(())
    }

    fn open_frames(&mut self, params: &CodecParameters) -> Result<(), SinkError> {
        // Releasing the codec before CPU-locking the window!
        self.teardown_codec();
        self.is_compressed = false;

        if self.backend.is_suspended.load(Ordering::SeqCst) {
            return Err(SinkError::Unavailable);
        }
        if self.current_window().is_none() {
            return Err(SinkError::Unavailable);
        }

        self.software_frame_info = Some((
            params.pixel_format.unwrap_or(PixelFormat::Yuv420P),
            params.width.unwrap_or(0),
            params.height.unwrap_or(0),
        ));

        Ok(())
    }

    fn push_frame(&mut self, frame: &VideoFrame, pts: Duration) -> Result<(), SinkError> {
        // Releasing the codec before CPU-locking the window
        self.teardown_codec();

        if self.backend.is_suspended.load(Ordering::SeqCst) {
            return Err(SinkError::Unavailable);
        }
        let window = match self.current_window() {
            Some(w) => w,
            None => return Err(SinkError::Unavailable),
        };

        let (src_fmt, w, h) = self
            .software_frame_info
            .ok_or_else(|| SinkError::Fatal("open_frames not called".into()))?;

        if w == 0 || h == 0 {
            return Ok(());
        }

        let frame_info = FrameInfo::new(src_fmt, w, h);
        let rgba_frame = oxideav_pixfmt::convert(
            frame,
            frame_info,
            PixelFormat::Rgba,
            &oxideav_pixfmt::ConvertOptions::default(),
        )
        .map_err(|e| SinkError::Fatal(format!("pixel conversion failed: {e:?}")))?;

        window
            .set_buffers_geometry(w as i32, h as i32, Some(HardwareBufferFormat::R8G8B8A8_UNORM))
            .map_err(|e| SinkError::Fatal(format!("setBuffersGeometry failed: {e:?}")))?;

        // Wait until pts
        if let Some(target_mono_ns) = self.clock.monotonic_ns_at(pts) {
            let now_mono_ns = monotonic_now_ns();
            if target_mono_ns > now_mono_ns {
                std::thread::sleep(Duration::from_nanos((target_mono_ns - now_mono_ns) as u64));
            }
        }

        let mut guard = window
            .lock(None)
            .map_err(|e| SinkError::Fatal(format!("ANativeWindow_lock failed: {e:?}")))?;

        let src_stride = rgba_frame.planes[0].stride;
        let src_bytes = &rgba_frame.planes[0].data;
        let dst_stride_bytes = guard.stride() * 4;
        let dst_bits = guard.bits() as *mut u8;
        let copy_width_bytes = (w as usize * 4).min(dst_stride_bytes).min(src_stride);
        let copy_height = (h as usize).min(guard.height());

        for y in 0..copy_height {
            unsafe {
                let src_row = &src_bytes[y * src_stride..y * src_stride + copy_width_bytes];
                let dst_row = dst_bits.add(y * dst_stride_bytes);
                std::ptr::copy_nonoverlapping(src_row.as_ptr(), dst_row, copy_width_bytes);
            }
        }

        // Dropping guard unlocks and posts
        Ok(())
    }

    fn flush(&mut self) {
        if let Some(codec) = self.codec.as_ref() {
            let _ = codec.0.flush();
        }
    }

    fn set_playing(&mut self, playing: bool) {
        self.is_playing.store(playing, Ordering::SeqCst);
    }
}
