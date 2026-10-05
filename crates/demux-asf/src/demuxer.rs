//! Ported from FFmpeg commit 2da55bf (libavformat/asfdec_f.c, asf.c, asf.h, asf_tags.c).
//! Licensed under LGPL-2.1-or-later.

use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};

use oxideav_core::{
    AttachedPicture, Chapter, CodecId, CodecParameters, CodecResolver, CodecTag, Demuxer, Error,
    MediaType, Packet, PictureType, ProbeContext, ReadSeek, Result, StreamInfo, TimeBase,
    Timestamp,
};

use crate::guid::*;

/// Maximum supported number of streams in an ASF file.
const MAX_STREAMS: usize = 128;
/// Maximum allocation size for any extradata buffer (1 MiB).
const MAX_EXTRADATA_SIZE: usize = 1024 * 1024;
/// Maximum allocation size for an attached picture (32 MiB).
const MAX_PICTURE_SIZE: usize = 32 * 1024 * 1024;
/// Maximum length of UTF-16 strings to read (64 KiB).
const MAX_STRING_BYTES: usize = 65536;
/// FFmpeg asfdec_f.c: minimum bytes that must remain in a packet for its
/// segments to be parsed in place.
const FRAME_HEADER_SIZE: i64 = 6;

#[derive(Clone, Debug, Default)]
struct AsfMainHeader {
    file_size: u64,
    #[allow(dead_code)]
    create_time: u64,
    play_time: u64,
    #[allow(dead_code)]
    send_time: u64,
    preroll: u32,
    flags: u32,
    min_pktsize: u32,
    max_pktsize: u32,
    #[allow(dead_code)]
    max_bitrate: u32,
}

#[derive(Clone, Debug, Default)]
struct PayloadExtension {
    ext_type: u8,
    size: u16,
}

#[derive(Clone, Debug, Default)]
struct StreamState {
    assembled_packet: Vec<u8>,
    assembled_len: usize,
    packet_obj_size: u32,
    /// Raw per-segment timestamp from the replic block; applied to `pts`
    /// when a new packet object is allocated (FFmpeg alloc-time dts).
    pending_pts: i64,
    pts: i64,
    keyframe: bool,
    skip_to_key: bool,
    ds_span: u8,
    ds_packet_size: u16,
    ds_chunk_size: u16,
    stream_language_index: u16,
    payload_extensions: Vec<PayloadExtension>,
}

#[derive(Clone, Debug)]
struct IndexEntry {
    pts: i64,
    pos: u64,
}

pub struct AsfDemuxer {
    input: Box<dyn ReadSeek>,
    streams: Vec<StreamInfo>,
    asf_id_to_stream_idx: [Option<usize>; MAX_STREAMS],
    asf_streams: Vec<StreamState>,
    hdr: AsfMainHeader,
    data_offset: u64,
    data_object_size: u64,
    metadata: Vec<(String, String)>,
    chapters: Vec<Chapter>,
    attached_pictures: Vec<AttachedPicture>,
    pending_packets: VecDeque<Packet>,
    index: Vec<IndexEntry>,
    index_read: bool,

    // Packet parsing state (matches FFmpeg ASFContext)
    packet_size_left: i64,
    packet_padsize: u32,
    packet_flags: u8,
    packet_property: u8,
    packet_timestamp: u32,
    packet_segsizetype: u8,
    packet_segments: i32,
    packet_time_start: u32,
    packet_time_delta: u32,
    packet_multi_size: u32,
    uses_std_ecc: i8,
    current_asf_stream_id: usize,
    current_key_frame: bool,
    current_frag_offset: u32,
    current_replic_size: u32,
    /// Set when the input is an Argo ASF file (magic "ASF\0"): the packet
    /// path switches to FFmpeg's argo_asf read_packet logic.
    argo: Option<ArgoState>,
}

/// Argo ASF demux state (FFmpeg ArgoASFDemuxContext).
struct ArgoState {
    blocks_read: u32,
    num_blocks: u32,
    num_samples: u32,
    block_align: u32,
}

impl AsfDemuxer {
    pub fn open(
        mut input: Box<dyn ReadSeek>,
        codecs: &dyn CodecResolver,
    ) -> Result<Box<dyn Demuxer>> {
        let mut header_guid = [0u8; 16];
        input.read_exact(&mut header_guid)?;
        if &header_guid[0..4] == b"ASF\x00" {
            // Argo ASF (Disney/Croc game audio): FFmpeg's separate argo_asf
            // demuxer claims these; ours carries the same packets so the
            // extension keeps working.
            input.seek(SeekFrom::Start(0))?;
            return Self::open_argo(input);
        }
        if header_guid != ASF_HEADER {
            return Err(Error::invalid("not an ASF header"));
        }

        let _header_size = read_u64_le(&mut *input)?;
        let _num_objects = read_u32_le(&mut *input)?;
        let _reserved1 = read_u8(&mut *input)?;
        let _reserved2 = read_u8(&mut *input)?;

        let mut demuxer = Self {
            input,
            streams: Vec::new(),
            asf_id_to_stream_idx: [None; MAX_STREAMS],
            asf_streams: (0..MAX_STREAMS).map(|_| StreamState::default()).collect(),
            hdr: AsfMainHeader::default(),
            data_offset: 0,
            data_object_size: u64::MAX,
            metadata: Vec::new(),
            chapters: Vec::new(),
            attached_pictures: Vec::new(),
            pending_packets: VecDeque::new(),
            index: Vec::new(),
            index_read: false,
            packet_size_left: 0,
            packet_padsize: 0,
            packet_flags: 0,
            packet_property: 0,
            packet_timestamp: 0,
            packet_segsizetype: 0,
            packet_segments: 0,
            packet_time_start: 0,
            packet_time_delta: 0,
            packet_multi_size: 0,
            uses_std_ecc: 0,
            current_asf_stream_id: 0,
            current_key_frame: false,
            current_frag_offset: 0,
            current_replic_size: 0,
            argo: None,
        };

        demuxer.read_header(codecs)?;
        Ok(Box::new(demuxer))
    }

    fn read_header(&mut self, codecs: &dyn CodecResolver) -> Result<()> {
        let mut stream_languages: Vec<String> = Vec::new();
        let mut stream_bitrates = [0u32; MAX_STREAMS];

        loop {
            let gpos = self.input.stream_position()?;
            let mut g = [0u8; 16];
            match self.input.read_exact(&mut g) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(Error::Io(e)),
            }

            let gsize = read_u64_le(&mut *self.input)?;
            if g == ASF_DATA_HEADER {
                let data_obj_offset = self.input.stream_position()?;
                if (self.hdr.flags & 0x01) == 0 && gsize >= 100 {
                    self.data_object_size = gsize.saturating_sub(24);
                } else {
                    self.data_object_size = u64::MAX;
                }
                let mut _client_id = [0u8; 16];
                self.input.read_exact(&mut _client_id)?;
                let _total_packets = read_u64_le(&mut *self.input)?;
                let _res1 = read_u8(&mut *self.input)?;
                let _res2 = read_u8(&mut *self.input)?;
                self.data_offset = self.input.stream_position()?;
                let _ = data_obj_offset;
                break;
            }

            if gsize < 24 {
                return Err(Error::invalid("invalid ASF object size"));
            }

            if g == ASF_FILE_HEADER {
                self.read_file_properties()?;
            } else if g == ASF_STREAM_HEADER {
                self.read_stream_properties(gsize, codecs)?;
            } else if g == ASF_COMMENT_HEADER {
                self.read_content_desc()?;
            } else if g == ASF_EXTENDED_CONTENT_HEADER {
                self.read_ext_content_desc(codecs)?;
            } else if g == ASF_LANGUAGE_GUID {
                stream_languages = self.read_language_list()?;
            } else if g == ASF_METADATA_HEADER || g == ASF_METADATA_LIBRARY_HEADER {
                self.read_metadata(codecs)?;
            } else if g == ASF_EXT_STREAM_HEADER {
                self.read_ext_stream_properties(&mut stream_bitrates)?;
                continue;
            } else if g == ASF_HEAD1_GUID {
                let mut _inner_guid = [0u8; 16];
                self.input.read_exact(&mut _inner_guid)?;
                self.input.seek(SeekFrom::Current(6))?;
                continue;
            } else if g == ASF_MARKER_HEADER {
                self.read_markers()?;
            }

            let target_pos = gpos + gsize;
            self.input.seek(SeekFrom::Start(target_pos))?;
        }

        // Apply bitrates and languages to streams
        for (asf_id, &opt_idx) in self.asf_id_to_stream_idx.iter().enumerate() {
            if let Some(idx) = opt_idx {
                if let Some(st) = self.streams.get_mut(idx) {
                    if st.params.bit_rate.is_none() || st.params.bit_rate == Some(0) {
                        if stream_bitrates[asf_id] > 0 {
                            st.params.bit_rate = Some(stream_bitrates[asf_id] as u64);
                        }
                    }
                    let lang_idx = self.asf_streams[asf_id].stream_language_index as usize;
                    if let Some(rfc1766) = stream_languages.get(lang_idx) {
                        if let Some(iso) = rfc1766_to_iso639_2(rfc1766) {
                            st.params.language = Some(iso.to_string());
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Argo ASF demuxer, ported from FFmpeg libavformat/argo_asf.c.
    /// Layout: file header (24 B), chunk header (20 B), then `num_blocks`
    /// blocks of `block_align` bytes; packets carry up to 32 blocks.
    fn open_argo(mut input: Box<dyn ReadSeek>) -> Result<Box<dyn Demuxer>> {
        let mut buf = [0u8; 24];
        input.read_exact(&mut buf)?;
        let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let version_major = u16::from_le_bytes([buf[4], buf[5]]);
        let version_minor = u16::from_le_bytes([buf[6], buf[7]]);
        let num_chunks = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
        let chunk_offset = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
        let mut name = [0u8; 8];
        name.copy_from_slice(&buf[16..24]);
        let name = String::from_utf8_lossy(&name)
            .trim_end_matches('\0')
            .to_string();

        // ff_argo_asf_validate_file_header
        if magic != 0x00465341 || num_chunks == 0 || chunk_offset < 24 {
            return Err(Error::invalid("invalid argo asf file header"));
        }
        if num_chunks != 1 {
            return Err(Error::invalid("argo asf: expected 1 chunk"));
        }

        input.seek(SeekFrom::Start(chunk_offset as u64))?;
        let mut ck = [0u8; 20];
        input.read_exact(&mut ck)?;
        let num_blocks = u32::from_le_bytes([ck[0], ck[1], ck[2], ck[3]]);
        let num_samples = u32::from_le_bytes([ck[4], ck[5], ck[6], ck[7]]);
        let sample_rate = u16::from_le_bytes([ck[12], ck[13]]) as u32;
        let flags = u32::from_le_bytes([ck[16], ck[17], ck[18], ck[19]]);

        // ff_argo_asf_fill_stream
        const ASF_SAMPLE_COUNT: u32 = 32;
        if num_samples != ASF_SAMPLE_COUNT {
            return Err(Error::invalid("argo asf: invalid sample count"));
        }
        const ASF_CF_BITS_PER_SAMPLE: u32 = 1 << 0;
        const ASF_CF_STEREO: u32 = 1 << 1;
        const ASF_CF_ALWAYS1: u32 = (1 << 2) | (1 << 3);
        const ASF_CF_ALWAYS0: u32 =
            !(ASF_CF_BITS_PER_SAMPLE | ASF_CF_STEREO | ASF_CF_ALWAYS1);
        if (flags & ASF_CF_ALWAYS1) != ASF_CF_ALWAYS1 || (flags & ASF_CF_ALWAYS0) != 0 {
            return Err(Error::invalid("argo asf: nonstandard flags"));
        }
        if (flags & ASF_CF_BITS_PER_SAMPLE) == 0 {
            return Err(Error::invalid("argo asf: non 16-bit samples"));
        }
        let channels: u16 = if (flags & ASF_CF_STEREO) != 0 { 2 } else { 1 };
        // v1.1 files (FX Fighter) are marked 44100 but are actually 22050
        let sample_rate = if version_major == 1 && version_minor == 1 {
            22050
        } else {
            sample_rate
        };
        let block_align =
            channels as u32 + (num_samples / 2) * channels as u32;

        let mut params = CodecParameters::audio(CodecId::new("adpcm_argo"));
        params.sample_rate = Some(sample_rate);
        params.channels = Some(channels);
        params.bit_rate =
            Some(channels as u64 * sample_rate as u64 * 4);
        params.extradata = name.into_bytes();

        let time_base = TimeBase::new(1, sample_rate as i64);
        let stream = StreamInfo {
            index: 0,
            time_base,
            duration: Some((num_blocks * num_samples) as i64),
            start_time: Some(0),
            params,
        };

        let demuxer = Self {
            input,
            streams: vec![stream],
            asf_id_to_stream_idx: [None; MAX_STREAMS],
            asf_streams: (0..MAX_STREAMS).map(|_| StreamState::default()).collect(),
            hdr: AsfMainHeader::default(),
            data_offset: (chunk_offset + 20) as u64,
            data_object_size: u64::MAX,
            metadata: Vec::new(),
            chapters: Vec::new(),
            attached_pictures: Vec::new(),
            pending_packets: VecDeque::new(),
            index: Vec::new(),
            index_read: true,
            packet_size_left: 0,
            packet_padsize: 0,
            packet_flags: 0,
            packet_property: 0,
            packet_timestamp: 0,
            packet_segsizetype: 0,
            packet_segments: 0,
            packet_time_start: 0,
            packet_time_delta: 0,
            packet_multi_size: 0,
            uses_std_ecc: -1,
            current_asf_stream_id: 0,
            current_key_frame: false,
            current_frag_offset: 0,
            current_replic_size: 0,
            argo: Some(ArgoState {
                blocks_read: 0,
                num_blocks,
                num_samples,
                block_align,
            }),
        };
        Ok(Box::new(demuxer))
    }

    /// Argo ASF packet read, ported from FFmpeg argo_asf_read_packet:
    /// emit `block_align * min(32, num_blocks - blocks_read)` bytes; pts is
    /// `blocks_read * num_samples`, duration `num_samples * blocks_in_packet`.
    fn argo_next_packet(&mut self) -> Result<Packet> {
        let argo = self.argo.as_ref().expect("argo state");
        if argo.blocks_read >= argo.num_blocks {
            return Err(Error::Eof);
        }
        let remaining_blocks = argo.num_blocks - argo.blocks_read;
        let blocks = remaining_blocks.min(32);
        let want = (argo.block_align * blocks) as usize;

        let mut data = vec![0u8; want];
        let mut filled = 0usize;
        while filled < want {
            match self.input.read(&mut data[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(Error::Io(e)),
            }
        }
        if filled == 0 {
            return Err(Error::Eof);
        }
        data.truncate(filled);
        // "Something real screwy is going on": partial block -> invalid data
        if filled % argo.block_align as usize != 0 {
            return Err(Error::invalid("argo asf: packet not block aligned"));
        }
        let blocks_read = filled / argo.block_align as usize;

        let argo = self.argo.as_mut().expect("argo state");
        let pts = (argo.blocks_read * argo.num_samples) as i64;
        let duration = (argo.num_samples * blocks_read as u32) as i64;
        argo.blocks_read += blocks_read as u32;

        let tb = self.streams[0].time_base;
        let mut pkt = Packet::new(0, tb, data);
        pkt.pts = Some(pts);
        pkt.dts = Some(pts);
        pkt.duration = Some(duration);
        pkt.flags.keyframe = true;
        Ok(pkt)
    }

    fn read_file_properties(&mut self) -> Result<()> {
        let mut _client_guid = [0u8; 16];
        self.input.read_exact(&mut _client_guid)?;
        self.hdr.file_size = read_u64_le(&mut *self.input)?;
        self.hdr.create_time = read_u64_le(&mut *self.input)?;
        let _num_packets = read_u64_le(&mut *self.input)?;
        self.hdr.play_time = read_u64_le(&mut *self.input)?;
        self.hdr.send_time = read_u64_le(&mut *self.input)?;
        self.hdr.preroll = read_u32_le(&mut *self.input)?;
        let _ignore = read_u32_le(&mut *self.input)?;
        self.hdr.flags = read_u32_le(&mut *self.input)?;
        self.hdr.min_pktsize = read_u32_le(&mut *self.input)?;
        self.hdr.max_pktsize = read_u32_le(&mut *self.input)?;
        if self.hdr.min_pktsize >= (1 << 29) || self.hdr.max_pktsize >= (1 << 29) {
            return Err(Error::invalid("invalid packet size in ASF file header"));
        }
        self.hdr.max_bitrate = read_u32_le(&mut *self.input)?;
        Ok(())
    }

    fn read_stream_properties(&mut self, gsize: u64, codecs: &dyn CodecResolver) -> Result<()> {
        let pos1 = self.input.stream_position()?;
        let mut stream_type = [0u8; 16];
        self.input.read_exact(&mut stream_type)?;

        let mut is_dvr_ms_audio = false;
        let media_type = if stream_type == ASF_AUDIO_STREAM {
            MediaType::Audio
        } else if stream_type == ASF_VIDEO_STREAM {
            MediaType::Video
        } else if stream_type == ASF_JFIF_MEDIA {
            MediaType::Video
        } else if stream_type == ASF_COMMAND_STREAM {
            // FFmpeg: AVMEDIA_TYPE_DATA, codec id left NONE
            MediaType::Data
        } else if stream_type == ASF_EXT_STREAM_EMBED_STREAM_HEADER {
            MediaType::Unknown
        } else {
            // Unhandled stream type (script, etc.)
            let pos2 = self.input.stream_position()?;
            let remaining = gsize.saturating_sub(pos2 - pos1 + 24);
            self.input.seek(SeekFrom::Current(remaining as i64))?;
            return Ok(());
        };

        let mut _err_corr_guid = [0u8; 16];
        self.input.read_exact(&mut _err_corr_guid)?;
        let _time_offset = read_u64_le(&mut *self.input)?;
        let type_specific_size = read_u32_le(&mut *self.input)?;
        let _err_corr_data_len = read_u32_le(&mut *self.input)?;
        let flags = read_u16_le(&mut *self.input)?;
        let stream_id = (flags & 0x7F) as usize;
        let _reserved = read_u32_le(&mut *self.input)?;

        let mut actual_media_type = media_type;
        if stream_type == ASF_EXT_STREAM_EMBED_STREAM_HEADER {
            let mut ext_guid = [0u8; 16];
            self.input.read_exact(&mut ext_guid)?;
            if ext_guid == ASF_EXT_STREAM_AUDIO_STREAM {
                actual_media_type = MediaType::Audio;
                is_dvr_ms_audio = true;
                let mut _dummy = [0u8; 16];
                self.input.read_exact(&mut _dummy)?;
                let _ = read_u32_le(&mut *self.input)?;
                let _ = read_u32_le(&mut *self.input)?;
                let _ = read_u32_le(&mut *self.input)?;
                self.input.read_exact(&mut _dummy)?;
                let _ = read_u32_le(&mut *self.input)?;
            }
        }

        if stream_id >= MAX_STREAMS {
            return Err(Error::invalid("ASF stream id out of range"));
        }

        let stream_index = self.streams.len() as u32;
        self.asf_id_to_stream_idx[stream_id] = Some(stream_index as usize);

        let time_base = TimeBase::new(1, 1000);
        let duration = if (self.hdr.flags & 0x01) == 0 {
            let dur_ms = (self.hdr.play_time / 10000).saturating_sub(self.hdr.preroll as u64);
            Some(dur_ms as i64)
        } else {
            None
        };

        if actual_media_type == MediaType::Audio {
            let wfx = self.read_waveformatex(type_specific_size as usize)?;
            let asf_st = &mut self.asf_streams[stream_id];
            let pos2 = self.input.stream_position()?;
            if gsize >= pos2 + 8 - pos1 + 24 {
                asf_st.ds_span = read_u8(&mut *self.input)?;
                asf_st.ds_packet_size = read_u16_le(&mut *self.input)?;
                asf_st.ds_chunk_size = read_u16_le(&mut *self.input)?;
                let _ds_data_size = read_u16_le(&mut *self.input)?;
                let _ds_silence = read_u8(&mut *self.input)?;
                if asf_st.ds_span > 1 {
                    if asf_st.ds_chunk_size == 0
                        || (asf_st.ds_packet_size / asf_st.ds_chunk_size <= 1)
                        || (asf_st.ds_packet_size % asf_st.ds_chunk_size != 0)
                    {
                        asf_st.ds_span = 0;
                    }
                }
            }

            let format_tag = wfx.format_tag;
            let tag = CodecTag::wave_format(format_tag);
            let mut probe = ProbeContext::new(&tag);
            probe.channels = Some(wfx.channels);
            probe.sample_rate = Some(wfx.samples_per_sec);
            probe.bits_per_sample = Some(wfx.bits_per_sample);
            if !wfx.extradata.is_empty() {
                probe.header = Some(&wfx.extradata);
            }

            let codec_id = if is_dvr_ms_audio {
                CodecId::new("mp2")
            } else {
                codecs
                    .resolve_tag(&probe)
                    .unwrap_or_else(|| fallback_wave_format_codec_id(format_tag))
            };

            let mut params = CodecParameters::audio(codec_id);
            params.sample_rate = Some(wfx.samples_per_sec);
            params.channels = Some(wfx.channels);
            params.bit_rate = Some(wfx.avg_bytes_per_sec.saturating_mul(8) as u64);
            params.extradata = wfx.extradata;
            params.tag = Some(tag);

            let mut stream_info = StreamInfo {
                index: stream_index,
                time_base,
                duration,
                start_time: Some(0),
                params,
            };
            let _ = &mut stream_info;
            self.streams.push(stream_info);
        } else if actual_media_type == MediaType::Video {
            if stream_type == ASF_JFIF_MEDIA {
                let tag = CodecTag::fourcc(b"MJPG");
                let mut params = CodecParameters::video(CodecId::new("mjpeg"));
                params.tag = Some(tag);
                self.streams.push(StreamInfo {
                    index: stream_index,
                    time_base,
                    duration,
                    start_time: Some(0),
                    params,
                });
            } else {
                let bmi = self.read_bitmapinfoheader(type_specific_size as usize)?;
                let tag = CodecTag::fourcc(&bmi.compression);
                let mut probe = ProbeContext::new(&tag);
                probe.width = Some(bmi.width);
                probe.height = Some(bmi.height);
                probe.bits_per_sample = Some(bmi.bit_count);
                if !bmi.extradata.is_empty() {
                    probe.header = Some(&bmi.extradata);
                }

                let codec_id = codecs
                    .resolve_tag(&probe)
                    .unwrap_or_else(|| fallback_fourcc_codec_id(&bmi.compression));

                let mut params = CodecParameters::video(codec_id);
                params.width = Some(bmi.width);
                params.height = Some(bmi.height);
                params.extradata = bmi.extradata;
                params.tag = Some(tag);

                self.streams.push(StreamInfo {
                    index: stream_index,
                    time_base,
                    duration,
                    start_time: Some(0),
                    params,
                });
            }
        } else if actual_media_type == MediaType::Data {
            // FFmpeg leaves the codec id NONE for command streams; surface a
            // stable "unknown" id so the pipeline can still route packets.
            let params = CodecParameters::data(CodecId::new("unknown"));
            self.streams.push(StreamInfo {
                index: stream_index,
                time_base,
                duration,
                start_time: Some(0),
                params,
            });
        } else if actual_media_type == MediaType::Unknown {
            // ext stream embed that is not audio: FFmpeg keeps type UNKNOWN
            let params = CodecParameters::data(CodecId::new("unknown"));
            self.streams.push(StreamInfo {
                index: stream_index,
                time_base,
                duration,
                start_time: Some(0),
                params,
            });
        }

        let pos2 = self.input.stream_position()?;
        let remaining = gsize.saturating_sub(pos2 - pos1 + 24);
        if remaining > 0 {
            self.input.seek(SeekFrom::Current(remaining as i64))?;
        }

        Ok(())
    }

    fn read_waveformatex(&mut self, total_size: usize) -> Result<WaveFormatEx> {
        if total_size < 14 {
            return Err(Error::invalid("WAVEFORMATEX too short"));
        }
        let format_tag = read_u16_le(&mut *self.input)?;
        let channels = read_u16_le(&mut *self.input)?;
        if channels > 64 {
            return Err(Error::invalid("channel count exceeds limit"));
        }
        let samples_per_sec = read_u32_le(&mut *self.input)?;
        let avg_bytes_per_sec = read_u32_le(&mut *self.input)?;
        let block_align = read_u16_le(&mut *self.input)?;
        let bits_per_sample = if total_size >= 16 {
            read_u16_le(&mut *self.input)?
        } else {
            0
        };

        let mut extradata = Vec::new();
        let mut actual_format_tag = format_tag;
        if total_size >= 18 {
            let cb_size = read_u16_le(&mut *self.input)? as usize;
            let ext_avail = total_size.saturating_sub(18);
            let ext_to_read = cb_size.min(ext_avail).min(MAX_EXTRADATA_SIZE);
            if ext_to_read > 0 {
                let mut raw = vec![0u8; ext_to_read];
                self.input.read_exact(&mut raw)?;
                if format_tag == 0xFFFE && ext_to_read >= 22 {
                    // WAVEFORMATEXTENSIBLE: first 2 bytes are valid bits, 4 bytes channel mask, 16 bytes subformat GUID
                    let subformat_tag = u16::from_le_bytes([raw[6], raw[7]]);
                    if subformat_tag != 0 {
                        actual_format_tag = subformat_tag;
                    }
                    if ext_to_read > 22 {
                        extradata = raw[22..].to_vec();
                    }
                } else {
                    extradata = raw;
                }
            }
            let remaining = ext_avail.saturating_sub(ext_to_read);
            if remaining > 0 {
                self.input.seek(SeekFrom::Current(remaining as i64))?;
            }
        }

        Ok(WaveFormatEx {
            format_tag: actual_format_tag,
            channels,
            samples_per_sec,
            avg_bytes_per_sec,
            block_align,
            bits_per_sample,
            extradata,
        })
    }

    fn read_bitmapinfoheader(&mut self, total_size: usize) -> Result<BitmapInfoHeader> {
        if total_size < 51 {
            return Err(Error::invalid("video stream properties too short"));
        }
        let _enc_width = read_u32_le(&mut *self.input)?;
        let _enc_height = read_u32_le(&mut *self.input)?;
        let _flags = read_u8(&mut *self.input)?;
        let _fmt_data_size = read_u16_le(&mut *self.input)?;

        let size_x = read_u32_le(&mut *self.input)?;
        let width = read_u32_le(&mut *self.input)?;
        let height_signed = read_i32_le(&mut *self.input)?;
        let height = height_signed.unsigned_abs();
        let planes = read_u16_le(&mut *self.input)?;
        let bit_count = read_u16_le(&mut *self.input)?;
        let mut compression = [0u8; 4];
        self.input.read_exact(&mut compression)?;
        let size_image = read_u32_le(&mut *self.input)?;
        let x_pels = read_i32_le(&mut *self.input)?;
        let y_pels = read_i32_le(&mut *self.input)?;
        let clr_used = read_u32_le(&mut *self.input)?;
        let clr_important = read_u32_le(&mut *self.input)?;

        // Cap frame dimensions
        if width > 16384 || height > 16384 || (width as u64 * height as u64) > (8192 * 8192) {
            return Err(Error::invalid("video dimensions exceed limits"));
        }

        let mut extradata = Vec::new();
        if size_x > 40 {
            let ext_len = ((size_x - 40) as usize).min(MAX_EXTRADATA_SIZE);
            extradata = vec![0u8; ext_len];
            self.input.read_exact(&mut extradata)?;
        }

        Ok(BitmapInfoHeader {
            width,
            height,
            planes,
            bit_count,
            compression,
            size_image,
            x_pels_per_meter: x_pels,
            y_pels_per_meter: y_pels,
            clr_used,
            clr_important,
            extradata,
        })
    }

    fn read_content_desc(&mut self) -> Result<()> {
        let len1 = read_u16_le(&mut *self.input)? as usize;
        let len2 = read_u16_le(&mut *self.input)? as usize;
        let len3 = read_u16_le(&mut *self.input)? as usize;
        let len4 = read_u16_le(&mut *self.input)? as usize;
        let len5 = read_u16_le(&mut *self.input)? as usize;

        if len1 > 0 {
            let title = self.read_utf16_le(len1)?;
            if !title.is_empty() {
                self.metadata.push(("title".to_string(), title));
            }
        }
        if len2 > 0 {
            let author = self.read_utf16_le(len2)?;
            if !author.is_empty() {
                self.metadata.push(("artist".to_string(), author.clone()));
                self.metadata.push(("author".to_string(), author));
            }
        }
        if len3 > 0 {
            let copyright = self.read_utf16_le(len3)?;
            if !copyright.is_empty() {
                self.metadata.push(("copyright".to_string(), copyright));
            }
        }
        if len4 > 0 {
            let comment = self.read_utf16_le(len4)?;
            if !comment.is_empty() {
                self.metadata.push(("comment".to_string(), comment));
            }
        }
        if len5 > 0 {
            self.input.seek(SeekFrom::Current(len5 as i64))?;
        }
        Ok(())
    }

    fn read_ext_content_desc(&mut self, codecs: &dyn CodecResolver) -> Result<()> {
        let desc_count = read_u16_le(&mut *self.input)? as usize;
        for _ in 0..desc_count.min(1024) {
            let mut name_len = read_u16_le(&mut *self.input)? as usize;
            if name_len % 2 != 0 {
                name_len += 1;
            }
            let name = self.read_utf16_le(name_len)?;
            let mut value_type = read_u16_le(&mut *self.input)?;
            let mut value_len = read_u16_le(&mut *self.input)? as usize;
            if value_type == 0 && value_len % 2 != 0 {
                value_len += 1;
            }
            if value_type == 2 {
                value_type = 3; // DWORD
            }

            self.handle_tag(&name, value_type, value_len, codecs)?;
        }
        Ok(())
    }

    fn read_metadata(&mut self, codecs: &dyn CodecResolver) -> Result<()> {
        let n = read_u16_le(&mut *self.input)? as usize;
        for _ in 0..n.min(2048) {
            let _lang_idx = read_u16_le(&mut *self.input)?;
            let _stream_num = read_u16_le(&mut *self.input)?;
            let name_len = read_u16_le(&mut *self.input)? as usize;
            let value_type = read_u16_le(&mut *self.input)?;
            let value_len = read_u32_le(&mut *self.input)? as usize;
            let name = self.read_utf16_le(name_len)?;

            self.handle_tag(&name, value_type, value_len, codecs)?;
        }
        Ok(())
    }

    fn handle_tag(
        &mut self,
        name: &str,
        value_type: u16,
        value_len: usize,
        _codecs: &dyn CodecResolver,
    ) -> Result<()> {
        let start_pos = self.input.stream_position()?;
        if value_type == 0 {
            let val = self.read_utf16_le(value_len)?;
            if !val.is_empty() {
                match name {
                    "WM/AlbumTitle" => self.metadata.push(("album".to_string(), val)),
                    "WM/AlbumArtist" => self.metadata.push(("album_artist".to_string(), val)),
                    "WM/Genre" => self.metadata.push(("genre".to_string(), val)),
                    "WM/Year" => self.metadata.push(("date".to_string(), val)),
                    "WM/TrackNumber" => self.metadata.push(("track".to_string(), val)),
                    _ => self.metadata.push((name.to_string(), val)),
                }
            }
        } else if value_type == 1 {
            // Byte array: check for WM/Picture or ID3
            if name == "WM/Picture" {
                self.parse_wm_picture(value_len)?;
            } else if name == "ID3" {
                self.parse_id3_apic(value_len)?;
            }
        } else if value_type == 2 || value_type == 5 {
            let val = read_u16_le(&mut *self.input)?;
            self.metadata.push((name.to_string(), val.to_string()));
        } else if value_type == 3 {
            let val = read_u32_le(&mut *self.input)?;
            self.metadata.push((name.to_string(), val.to_string()));
        } else if value_type == 4 {
            let val = read_u64_le(&mut *self.input)?;
            self.metadata.push((name.to_string(), val.to_string()));
        }

        let target_pos = start_pos + value_len as u64;
        self.input.seek(SeekFrom::Start(target_pos))?;
        Ok(())
    }

    fn parse_wm_picture(&mut self, val_len: usize) -> Result<()> {
        if val_len < 9 || val_len > MAX_PICTURE_SIZE {
            return Ok(());
        }
        let pic_type_byte = read_u8(&mut *self.input)?;
        let picsize = read_u32_le(&mut *self.input)? as usize;
        let mut remaining = val_len.saturating_sub(5);

        let mime = self.read_utf16_null_terminated(&mut remaining)?;
        let desc = self.read_utf16_null_terminated(&mut remaining)?;

        if picsize > 0 && picsize <= remaining && picsize <= MAX_PICTURE_SIZE {
            let mut pic_data = vec![0u8; picsize];
            self.input.read_exact(&mut pic_data)?;

            let pic_type = PictureType::from_u8(pic_type_byte);
            let mime_str = if mime.is_empty() { "image/jpeg" } else { &mime };
            let attached_pic = AttachedPicture {
                mime_type: mime_str.to_string(),
                picture_type: pic_type,
                description: desc,
                data: pic_data.clone(),
            };
            self.attached_pictures.push(attached_pic);

            // Add an attached picture stream matching FFmpeg behavior
            let stream_index = self.streams.len() as u32;
            let tag = CodecTag::fourcc(b"MJPG");
            let mut params = CodecParameters::video(CodecId::new("mjpeg"));
            params.tag = Some(tag);

            self.streams.push(StreamInfo {
                index: stream_index,
                time_base: TimeBase::new(1, 90000),
                duration: None,
                start_time: None,
                params,
            });

            // Enqueue initial packet for this attached picture stream
            let mut pkt = Packet::new(stream_index, TimeBase::new(1, 90000), pic_data);
            pkt.flags.keyframe = true;
            self.pending_packets.push_back(pkt);
        }
        Ok(())
    }

    fn parse_id3_apic(&mut self, val_len: usize) -> Result<()> {
        if val_len < 10 || val_len > MAX_PICTURE_SIZE {
            return Ok(());
        }
        let mut buf = vec![0u8; val_len];
        self.input.read_exact(&mut buf)?;
        if buf.starts_with(b"ID3") && buf.len() >= 10 {
            // Find APIC frame
            let mut pos = 10;
            while pos + 10 <= buf.len() {
                let frame_id = &buf[pos..pos + 4];
                let frame_size = u32::from_be_bytes([
                    buf[pos + 4],
                    buf[pos + 5],
                    buf[pos + 6],
                    buf[pos + 7],
                ]) as usize;
                pos += 10;
                if pos + frame_size > buf.len() {
                    break;
                }
                if frame_id == b"APIC" && frame_size >= 4 {
                    let frame = &buf[pos..pos + frame_size];
                    let _encoding = frame[0];
                    let mut i = 1;
                    while i < frame.len() && frame[i] != 0 {
                        i += 1;
                    }
                    let mime = String::from_utf8_lossy(&frame[1..i]).to_string();
                    i += 1; // null
                    if i < frame.len() {
                        let pic_type = PictureType::from_u8(frame[i]);
                        i += 1;
                        while i < frame.len() && frame[i] != 0 {
                            i += 1;
                        }
                        let desc = "";
                        i += 1; // null
                        if i < frame.len() {
                            let pic_data = frame[i..].to_vec();
                            let mime_str = if mime.is_empty() { "image/jpeg" } else { &mime };
                            self.attached_pictures.push(AttachedPicture {
                                mime_type: mime_str.to_string(),
                                picture_type: pic_type,
                                description: desc.to_string(),
                                data: pic_data.clone(),
                            });

                            let stream_index = self.streams.len() as u32;
                            let mut params = CodecParameters::video(CodecId::new("mjpeg"));
                            params.tag = Some(CodecTag::fourcc(b"MJPG"));
                            self.streams.push(StreamInfo {
                                index: stream_index,
                                time_base: TimeBase::new(1, 90000),
                                duration: None,
                                start_time: None,
                                params,
                            });

                            let mut pkt = Packet::new(stream_index, TimeBase::new(1, 90000), pic_data);
                            pkt.flags.keyframe = true;
                            self.pending_packets.push_back(pkt);
                        }
                    }
                    break;
                }
                pos += frame_size;
            }
        }
        Ok(())
    }

    fn read_utf16_null_terminated(&mut self, remaining: &mut usize) -> Result<String> {
        let mut u16_words = Vec::new();
        while *remaining >= 2 {
            let w = read_u16_le(&mut *self.input)?;
            *remaining -= 2;
            if w == 0 {
                break;
            }
            if u16_words.len() < 1024 {
                u16_words.push(w);
            }
        }
        Ok(String::from_utf16_lossy(&u16_words))
    }

    fn read_language_list(&mut self) -> Result<Vec<String>> {
        let count = read_u16_le(&mut *self.input)? as usize;
        let mut list = Vec::new();
        for _ in 0..count.min(128) {
            let lang_len = read_u8(&mut *self.input)? as usize;
            let lang = self.read_utf16_le(lang_len)?;
            list.push(lang);
        }
        Ok(list)
    }

    fn read_ext_stream_properties(
        &mut self,
        stream_bitrates: &mut [u32; MAX_STREAMS],
    ) -> Result<()> {
        let _start_time = read_u64_le(&mut *self.input)?;
        let _end_time = read_u64_le(&mut *self.input)?;
        let leak_rate = read_u32_le(&mut *self.input)?;
        let _bucket_size = read_u32_le(&mut *self.input)?;
        let _init_fullness = read_u32_le(&mut *self.input)?;
        let _alt_leak_rate = read_u32_le(&mut *self.input)?;
        let _alt_bucket_size = read_u32_le(&mut *self.input)?;
        let _alt_init_fullness = read_u32_le(&mut *self.input)?;
        let _max_obj_size = read_u32_le(&mut *self.input)?;
        let _flags = read_u32_le(&mut *self.input)?;
        let stream_num = read_u16_le(&mut *self.input)? as usize;
        let stream_lang_idx = read_u16_le(&mut *self.input)?;
        let _avg_frame_time = read_u64_le(&mut *self.input)?;
        let stream_ct = read_u16_le(&mut *self.input)? as usize;
        let payload_ext_ct = read_u16_le(&mut *self.input)? as usize;

        if stream_num < MAX_STREAMS {
            stream_bitrates[stream_num] = leak_rate;
            self.asf_streams[stream_num].stream_language_index = stream_lang_idx;
            self.asf_streams[stream_num].payload_extensions.clear();
        }

        for _ in 0..stream_ct {
            let _ = read_u16_le(&mut *self.input)?;
            let ext_len = read_u16_le(&mut *self.input)? as i64;
            self.input.seek(SeekFrom::Current(ext_len))?;
        }

        for _ in 0..payload_ext_ct {
            let mut g = [0u8; 16];
            self.input.read_exact(&mut g)?;
            let size = read_u16_le(&mut *self.input)?;
            let ext_len = read_u32_le(&mut *self.input)? as i64;
            self.input.seek(SeekFrom::Current(ext_len))?;
            if stream_num < MAX_STREAMS && self.asf_streams[stream_num].payload_extensions.len() < 8 {
                self.asf_streams[stream_num].payload_extensions.push(PayloadExtension {
                    ext_type: g[0],
                    size,
                });
            }
        }
        Ok(())
    }

    fn read_markers(&mut self) -> Result<()> {
        let mut _reserved = [0u8; 16];
        self.input.read_exact(&mut _reserved)?;
        let count = read_u32_le(&mut *self.input)? as usize;
        let _res2 = read_u16_le(&mut *self.input)?;
        let name_len = read_u16_le(&mut *self.input)? as i64;
        self.input.seek(SeekFrom::Current(name_len))?;

        for i in 0..count.min(2048) {
            let _offset = read_u64_le(&mut *self.input)?;
            let pres_time = read_u64_le(&mut *self.input)?;
            let pres_time_ms =
                (pres_time / 10000).saturating_sub(self.hdr.preroll as u64) as i64;
            let _entry_len = read_u16_le(&mut *self.input)?;
            let _send_time = read_u32_le(&mut *self.input)?;
            let _flags = read_u32_le(&mut *self.input)?;
            let marker_name_len = read_u32_le(&mut *self.input)? as usize;
            let title = self.read_utf16_le(marker_name_len * 2)?;

            let tb = TimeBase::new(1, 1000);
            let start = Timestamp::new(pres_time_ms, tb);
            self.chapters.push(Chapter {
                id: i as u64,
                start,
                end: start,
                title: if title.is_empty() { None } else { Some(title) },
                language: None,
            });
        }
        Ok(())
    }

    fn build_simple_index(&mut self) -> Result<()> {
        if self.index_read {
            return Ok(());
        }
        self.index_read = true;
        if self.data_object_size == u64::MAX {
            return Ok(());
        }
        let cur = self.input.stream_position()?;
        let index_search_pos = self.data_offset + self.data_object_size;
        if self.input.seek(SeekFrom::Start(index_search_pos)).is_err() {
            let _ = self.input.seek(SeekFrom::Start(cur));
            return Ok(());
        }

        loop {
            let mut g = [0u8; 16];
            if self.input.read_exact(&mut g).is_err() {
                break;
            }
            let gsize = match read_u64_le(&mut *self.input) {
                Ok(s) => s,
                Err(_) => break,
            };
            if gsize < 24 {
                break;
            }
            if g == ASF_SIMPLE_INDEX_HEADER {
                let mut _file_id = [0u8; 16];
                if self.input.read_exact(&mut _file_id).is_err() {
                    break;
                }
                let itime = match read_u64_le(&mut *self.input) {
                    Ok(t) => t,
                    Err(_) => break,
                };
                let _pct = match read_u32_le(&mut *self.input) {
                    Ok(p) => p,
                    Err(_) => break,
                };
                let ict = match read_u32_le(&mut *self.input) {
                    Ok(c) => c as usize,
                    Err(_) => break,
                };

                let pkt_size = self.hdr.max_pktsize as u64;
                let mut last_pos = u64::MAX;
                for i in 0..ict.min(65536) {
                    let pktnum = match read_u32_le(&mut *self.input) {
                        Ok(n) => n as u64,
                        Err(_) => break,
                    };
                    let _pktct = match read_u16_le(&mut *self.input) {
                        Ok(c) => c,
                        Err(_) => break,
                    };
                    let pos = self.data_offset + pkt_size * pktnum;
                    let index_pts = ((i as u64).saturating_mul(itime) / 10000)
                        .saturating_sub(self.hdr.preroll as u64) as i64;
                    if pos != last_pos {
                        self.index.push(IndexEntry { pts: index_pts, pos });
                        last_pos = pos;
                    }
                }
                break;
            }
            if self.input.seek(SeekFrom::Current(gsize as i64 - 24)).is_err() {
                break;
            }
        }

        let _ = self.input.seek(SeekFrom::Start(cur));
        Ok(())
    }

    fn reset_packet_state(&mut self) {
        self.packet_size_left = 0;
        self.packet_padsize = 0;
        self.packet_flags = 0;
        self.packet_property = 0;
        self.packet_timestamp = 0;
        self.packet_segsizetype = 0;
        self.packet_segments = 0;
        self.packet_time_start = 0;
        self.packet_time_delta = 0;
        self.packet_multi_size = 0;
        self.current_asf_stream_id = 0;
        self.current_key_frame = false;
        self.current_frag_offset = 0;
        self.current_replic_size = 0;

        for st in &mut self.asf_streams {
            st.assembled_packet.clear();
            st.assembled_len = 0;
            st.packet_obj_size = 0;
        }
    }

    fn read_u8_eof(&mut self) -> Result<Option<u8>> {
        let mut b = [0u8; 1];
        match self.input.read_exact(&mut b) {
            Ok(()) => Ok(Some(b[0])),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
            Err(e) => Err(Error::Io(e)),
        }
    }

    fn read_utf16_le(&mut self, byte_len: usize) -> Result<String> {
        let len = byte_len.min(MAX_STRING_BYTES);
        let mut raw = vec![0u8; len];
        self.input.read_exact(&mut raw)?;
        if byte_len > len {
            self.input.seek(SeekFrom::Current((byte_len - len) as i64))?;
        }
        let mut u16_words = Vec::with_capacity(len / 2);
        for chunk in raw.chunks_exact(2) {
            u16_words.push(u16::from_le_bytes([chunk[0], chunk[1]]));
        }
        let s = String::from_utf16_lossy(&u16_words);
        Ok(s.trim_end_matches('\0').to_string())
    }

    fn get_packet(&mut self) -> Result<()> {
        // FFmpeg: `int rsize = 8` — the baseline accounts for the ECC bytes
        // already consumed plus the timestamp/duration fields read below.
        let mut rsize: i64 = 8;
        let mut c = match self.read_u8_eof()? {
            Some(byte) => byte,
            None => return Err(Error::Eof),
        };
        let d;

        if self.uses_std_ecc > 0 {
            // if we do not know packet size, allow skipping up to 32 kB
            let mut off = 32768i64;
            let mut c_val = -1i32;
            let mut d_val = -1i32;
            let mut e_val = c as i32;
            while off > 0 {
                off -= 1;
                c_val = d_val;
                d_val = e_val;
                match self.read_u8_eof()? {
                    Some(b) => e_val = b as i32,
                    None => return Err(Error::Eof),
                }
                if c_val == 0x82 && d_val == 0 && e_val == 0 {
                    break;
                }
            }

            if (c_val & 0x8F) == 0x82 {
                if d_val != 0 || e_val != 0 {
                    return Err(Error::invalid("bad ecc non zero"));
                }
                c = read_u8(&mut *self.input)?;
                d = read_u8(&mut *self.input)?;
                rsize += 3;
            } else {
                self.input.seek(SeekFrom::Current(-1))?;
                d = read_u8(&mut *self.input)?;
            }
        } else {
            if (c & 0x80) != 0 {
                rsize += 1;
                let mut d_ecc = 0u8;
                let mut e_ecc = 0u8;
                if (c & 0x60) == 0 {
                    d_ecc = read_u8(&mut *self.input)?;
                    e_ecc = read_u8(&mut *self.input)?;
                    let skip_len = (c & 0x0F).saturating_sub(2);
                    self.input.seek(SeekFrom::Current(skip_len as i64))?;
                    rsize += (c & 0x0F) as i64;
                }

                if self.uses_std_ecc == 0 {
                    self.uses_std_ecc = if c == 0x82 && d_ecc == 0 && e_ecc == 0 {
                        1
                    } else {
                        -1
                    };
                }
                c = read_u8(&mut *self.input)?;
            } else {
                self.uses_std_ecc = -1;
            }
            d = read_u8(&mut *self.input)?;
        }

        self.packet_flags = c;
        self.packet_property = d;

        // DO_2BITS(flags >> 5, packet_length, s->packet_size): the 0 case
        // *sets* the variable to the default.
        let packet_length = match (c >> 5) & 3 {
            3 => {
                rsize += 4;
                read_u32_le(&mut *self.input)?
            }
            2 => {
                rsize += 2;
                read_u16_le(&mut *self.input)? as u32
            }
            1 => {
                rsize += 1;
                read_u8(&mut *self.input)? as u32
            }
            _ => self.hdr.max_pktsize,
        };

        // DO_2BITS(flags >> 1, padsize, 0) then DO_2BITS(flags >> 3, padsize, 0):
        // the sequence read lands in `padsize` first and is only overwritten when
        // the padding-length bits select a size; type 0 keeps the sequence value.
        let seq = match (c >> 1) & 3 {
            3 => {
                rsize += 4;
                read_u32_le(&mut *self.input)?
            }
            2 => {
                rsize += 2;
                read_u16_le(&mut *self.input)? as u32
            }
            1 => {
                rsize += 1;
                read_u8(&mut *self.input)? as u32
            }
            _ => 0,
        };
        let mut padsize = match (c >> 3) & 3 {
            3 => {
                rsize += 4;
                read_u32_le(&mut *self.input)?
            }
            2 => {
                rsize += 2;
                read_u16_le(&mut *self.input)? as u32
            }
            1 => {
                rsize += 1;
                read_u8(&mut *self.input)? as u32
            }
            _ => seq, // type 0: keep the sequence value FFmpeg left in padsize
        };

        if packet_length == 0 || packet_length >= (1 << 29) {
            return Err(Error::invalid("invalid packet_length"));
        }
        if padsize >= packet_length {
            return Err(Error::invalid("invalid padsize"));
        }

        self.packet_timestamp = read_u32_le(&mut *self.input)?;
        let _duration = read_u16_le(&mut *self.input)?;

        if (c & 0x01) != 0 {
            self.packet_segsizetype = read_u8(&mut *self.input)?;
            rsize += 1;
            self.packet_segments = (self.packet_segsizetype & 0x3F) as i32;
        } else {
            self.packet_segments = 1;
            self.packet_segsizetype = 0x80;
        }

        if rsize > (packet_length - padsize) as i64 {
            return Err(Error::invalid("packet header exceeds payload space"));
        }

        self.packet_size_left = (packet_length - padsize) as i64 - rsize;
        if packet_length < self.hdr.min_pktsize {
            padsize += self.hdr.min_pktsize - packet_length;
        }
        self.packet_padsize = padsize;

        Ok(())
    }
}

impl Demuxer for AsfDemuxer {
    fn format_name(&self) -> &str {
        "asf"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn duration_micros(&self) -> Option<i64> {
        if self.hdr.play_time > 0 {
            let dur_100ns = self.hdr.play_time.saturating_sub((self.hdr.preroll as u64) * 10000);
            Some((dur_100ns / 10) as i64)
        } else {
            None
        }
    }

    fn metadata(&self) -> &[(String, String)] {
        &self.metadata
    }

    fn attached_pictures(&self) -> &[AttachedPicture] {
        &self.attached_pictures
    }

    fn chapters(&self) -> &[Chapter] {
        &self.chapters
    }

    fn seek_to(&mut self, _stream_index: u32, pts: i64) -> Result<i64> {
        if pts <= 0 {
            self.reset_packet_state();
            self.input.seek(SeekFrom::Start(self.data_offset))?;
            for st in &mut self.asf_streams {
                st.skip_to_key = true;
            }
            return Ok(0);
        }

        self.build_simple_index()?;
        if self.index.is_empty() {
            return Err(Error::unsupported("no simple index in ASF file"));
        }

        let mut idx = 0;
        for (i, entry) in self.index.iter().enumerate() {
            if entry.pts <= pts {
                idx = i;
            } else {
                break;
            }
        }

        let target_pos = self.index[idx].pos;
        let actual_pts = self.index[idx].pts;
        self.input.seek(SeekFrom::Start(target_pos))?;
        self.reset_packet_state();
        for st in &mut self.asf_streams {
            st.skip_to_key = true;
        }
        Ok(actual_pts)
    }

    fn next_packet(&mut self) -> Result<Packet> {
        if let Some(pkt) = self.pending_packets.pop_front() {
            return Ok(pkt);
        }

        if self.argo.is_some() {
            return self.argo_next_packet();
        }

        loop {
            // FFmpeg asf_parse_packet: refill when fewer than FRAME_HEADER_SIZE
            // bytes remain or no segments are pending.
            if self.packet_size_left < FRAME_HEADER_SIZE
                || (self.packet_segments < 1 && self.packet_time_start == 0)
            {
                if std::env::var_os("PEARTUBE_ASF_DEBUG").is_some() {
                    eprintln!(
                        "refill size_left={} segs={} time_start={} multi={}",
                        self.packet_size_left, self.packet_segments,
                        self.packet_time_start, self.packet_multi_size
                    );
                }
                // FFmpeg asserts size_left + padsize >= 0; a negative means a
                // desync — resync by skipping nothing (position already past).
                let skip = if self.packet_size_left >= 0 {
                    (self.packet_size_left as u64) + (self.packet_padsize as u64)
                } else {
                    0
                };
                if skip > 0 {
                    self.input.seek(SeekFrom::Current(skip as i64))?;
                }
                let cur = self.input.stream_position()?;
                if self.data_object_size != u64::MAX
                    && cur.saturating_sub(self.data_offset) >= self.data_object_size
                {
                    return Err(Error::Eof);
                }
                self.get_packet()?;
                self.packet_time_start = 0;
                continue;
            }

            // FFmpeg: asf->stream_index persists; multipacket continuations
            // reuse the stream chosen by their segment's frame header.
            let mut asf_stream_id = self.current_asf_stream_id;
            let mut packet_key_frame = false;
            let mut frag_offset = 0u32;
            let mut frag_size = 0u32;
            // FFmpeg: asf->packet_replic_size is context — it persists across
            // emitted packets so multipacket (replic==1) chains continue after
            // a completed sub-packet was returned. Refreshed by the frame
            // header below; the multipacket branch keeps it at 1.
            let mut replic_size = if self.packet_time_start != 0 {
                1
            } else {
                self.current_replic_size
            };

            if self.packet_time_start == 0 {
                // asf_read_frame_header
                let mut rsize = 1u32;
                let num = read_u8(&mut *self.input)?;
                self.packet_segments -= 1;
                packet_key_frame = (num & 0x80) != 0;
                asf_stream_id = (num & 0x7F) as usize;
                self.current_asf_stream_id = asf_stream_id;

                let d = self.packet_property;
                let _seq = match (d >> 4) & 3 {
                    3 => {
                        rsize += 4;
                        read_u32_le(&mut *self.input)?
                    }
                    2 => {
                        rsize += 2;
                        read_u16_le(&mut *self.input)? as u32
                    }
                    1 => {
                        rsize += 1;
                        read_u8(&mut *self.input)? as u32
                    }
                    _ => 0,
                };
                frag_offset = match (d >> 2) & 3 {
                    3 => {
                        rsize += 4;
                        read_u32_le(&mut *self.input)?
                    }
                    2 => {
                        rsize += 2;
                        read_u16_le(&mut *self.input)? as u32
                    }
                    1 => {
                        rsize += 1;
                        read_u8(&mut *self.input)? as u32
                    }
                    _ => 0,
                };
                replic_size = match d & 3 {
                    3 => {
                        rsize += 4;
                        read_u32_le(&mut *self.input)?
                    }
                    2 => {
                        rsize += 2;
                        read_u16_le(&mut *self.input)? as u32
                    }
                    1 => {
                        rsize += 1;
                        read_u8(&mut *self.input)? as u32
                    }
                    _ => 0,
                };
                self.current_replic_size = replic_size;
                if std::env::var_os("PEARTUBE_ASF_DEBUG").is_some() {
                    eprintln!(
                        "hdr sid={} key={} fo={} rs={} size_left={}",
                        asf_stream_id, packet_key_frame, frag_offset,
                        replic_size, self.packet_size_left
                    );
                }
                let mut packet_obj_size = 0u32;
                let mut packet_frag_timestamp: i64 = 0;
                let frag_size_read: Option<u32>;

                if rsize as i64 + replic_size as i64 > self.packet_size_left {
                    // FFmpeg: invalid replic size; abort this packet's segments
                    // and resync at the next packet header.
                    self.packet_time_start = 0;
                    self.packet_segments = 0;
                    continue;
                }

                if replic_size >= 8 {
                    let rep_start = self.input.stream_position()?;
                    let rep_end = rep_start + replic_size as u64;
                    packet_obj_size = read_u32_le(&mut *self.input)?;
                    if packet_obj_size >= (1 << 24) {
                        // FFmpeg: reset and resync at the next packet header
                        self.packet_time_start = 0;
                        self.packet_segments = 0;
                        continue;
                    }
                    packet_frag_timestamp = read_u32_le(&mut *self.input)? as i64;

                    if asf_stream_id < MAX_STREAMS {
                        for ext in &self.asf_streams[asf_stream_id].payload_extensions {
                            let mut sz = ext.size as u64;
                            if sz == 0xFFFF {
                                sz = read_u16_le(&mut *self.input)? as u64;
                            }
                            let pay_start = self.input.stream_position()?;
                            let pay_end = pay_start + sz;
                            if pay_end > rep_end {
                                break;
                            }
                            if ext.ext_type == 0x2A && sz >= 24 {
                                self.input.seek(SeekFrom::Current(8))?;
                                let ts0 = read_i64_le(&mut *self.input)?;
                                let _ts1 = read_i64_le(&mut *self.input)?;
                                if ts0 != -1 {
                                    packet_frag_timestamp = ts0 / 10000;
                                }
                            }
                            self.input.seek(SeekFrom::Start(pay_end))?;
                        }
                    }
                    self.input.seek(SeekFrom::Start(rep_end))?;
                    rsize += replic_size;
                } else if replic_size == 1 {
                    // multipacket: frag_offset carries the beginning timestamp
                    self.packet_time_start = frag_offset;
                    packet_frag_timestamp = self.packet_timestamp as i64;
                    self.packet_time_delta = read_u8(&mut *self.input)? as u32;
                    rsize += 1;
                } else if replic_size != 0 {
                    return Err(Error::invalid("unexpected packet_replic_size"));
                }

                if (self.packet_flags & 0x01) != 0 {
                    let fs = match (self.packet_segsizetype >> 6) & 3 {
                        3 => {
                            rsize += 4;
                            read_u32_le(&mut *self.input)?
                        }
                        2 => {
                            rsize += 2;
                            read_u16_le(&mut *self.input)? as u32
                        }
                        1 => {
                            rsize += 1;
                            read_u8(&mut *self.input)? as u32
                        }
                        _ => 0,
                    };
                    if rsize as i64 > self.packet_size_left {
                        // FFmpeg: abort this packet's segments and resync
                        self.packet_time_start = 0;
                        self.packet_segments = 0;
                        continue;
                    } else if fs as i64 > self.packet_size_left - rsize as i64 {
                        if fs as i64
                            > self.packet_size_left - rsize as i64 + self.packet_padsize as i64
                        {
                            // FFmpeg: "packet_frag_size is invalid" -> resync
                            self.packet_time_start = 0;
                            self.packet_segments = 0;
                            continue;
                        } else {
                            let diff =
                                fs as i64 - (self.packet_size_left - rsize as i64);
                            self.packet_size_left += diff;
                            self.packet_padsize -= diff as u32;
                        }
                    }
                    frag_size_read = Some(fs);
                } else {
                    frag_size_read = Some((self.packet_size_left - rsize as i64) as u32);
                }

                if replic_size == 1 {
                    self.packet_multi_size = frag_size_read.unwrap_or(0);
                    if self.packet_multi_size as i64 > self.packet_size_left {
                        return Err(Error::invalid("packet_multi_size exceeds packet"));
                    }
                }
                self.packet_size_left -= rsize as i64;

                if replic_size >= 8 && asf_stream_id < MAX_STREAMS {
                    // FFmpeg assigns packet_obj_size/timestamp only here;
                    // the multipacket (replic==1) branch sets them itself.
                    self.asf_streams[asf_stream_id].packet_obj_size = packet_obj_size;
                    self.asf_streams[asf_stream_id].pending_pts = packet_frag_timestamp;
                }
                frag_size = frag_size_read.unwrap_or(0);
            }

            if replic_size == 1 || self.packet_time_start != 0 {
                // asf_parse_packet multipacket (compressed payload) path
                if std::env::var_os("PEARTUBE_ASF_DEBUG").is_some() {
                    eprintln!(
                        "multi time_start={} multi_size={} size_left={}",
                        self.packet_time_start, self.packet_multi_size, self.packet_size_left
                    );
                }
                let pts = (self.packet_time_start as i64).saturating_sub(self.hdr.preroll as i64);
                self.packet_time_start = self.packet_time_start.wrapping_add(self.packet_time_delta);
                let sub_size = read_u8(&mut *self.input)? as u32;
                self.packet_size_left -= 1;
                self.packet_multi_size = self.packet_multi_size.saturating_sub(1);
                if self.packet_multi_size < sub_size {
                    self.packet_time_start = 0;
                    let skip = self.packet_multi_size;
                    self.input.seek(SeekFrom::Current(skip as i64))?;
                    self.packet_size_left -= skip as i64;
                    continue;
                }
                self.packet_multi_size -= sub_size;
                if self.packet_multi_size == 0 {
                    self.packet_time_start = 0;
                }
                frag_size = sub_size;
                frag_offset = 0;
                if asf_stream_id < MAX_STREAMS {
                    // multipacket payloads are whole objects: the allocation
                    // in the fragment step below must reuse this timestamp
                    let st = &mut self.asf_streams[asf_stream_id];
                    st.packet_obj_size = sub_size;
                    st.pending_pts = pts + self.hdr.preroll as i64;
                    st.pts = pts;
                    st.keyframe = true; // compressed payloads are audio (always key)
                }
            }

            let opt_idx = if asf_stream_id < MAX_STREAMS {
                self.asf_id_to_stream_idx[asf_stream_id]
            } else {
                None
            };
            let stream_idx = match opt_idx {
                Some(idx) => idx,
                None => {
                    // FFmpeg: unknown stream — skip the fragment bytes
                    self.input.seek(SeekFrom::Current(frag_size as i64))?;
                    self.packet_size_left -= frag_size as i64;
                    continue;
                }
            };

            let asf_st = &mut self.asf_streams[asf_stream_id];
            if std::env::var_os("PEARTUBE_ASF_DEBUG").is_some() {
                eprintln!(
                    "seg sid={} fo={} fs={} obj={} assembled={} pending_pts={}",
                    asf_stream_id, frag_offset, frag_size, asf_st.packet_obj_size,
                    asf_st.assembled_len, asf_st.pending_pts
                );
            }

            // FFmpeg: a fragment starting mid-packet with nothing assembled is
            // unreadable; skip its bytes.
            if asf_st.assembled_len == 0 && frag_offset != 0 {
                self.input.seek(SeekFrom::Current(frag_size as i64))?;
                self.packet_size_left -= frag_size as i64;
                continue;
            }

            let is_audio = self.streams[stream_idx].params.media_type == MediaType::Audio;
            let obj_size = asf_st.packet_obj_size as usize;

            // FFmpeg: allocate a fresh object when the size changed or the
            // fragment no longer fits; the timestamp is taken at ALLOCATION.
            if asf_st.assembled_packet.len() != obj_size
                || frag_offset as usize + frag_size as usize > obj_size
            {
                asf_st.assembled_packet = vec![0u8; obj_size];
                asf_st.assembled_len = 0;
                asf_st.pts = asf_st.pending_pts.saturating_sub(self.hdr.preroll as i64);
                asf_st.keyframe = is_audio || packet_key_frame;
            }

            // FFmpeg: "packet_size_left -= frag_size; if (< 0) continue;"
            self.packet_size_left -= frag_size as i64;
            if self.packet_size_left < 0 {
                continue;
            }

            // FFmpeg: fragment must lie inside the object; otherwise skip the
            // read (but not the bytes) and keep parsing.
            let offset = frag_offset as usize;
            if offset >= obj_size || frag_size as usize > obj_size - offset {
                continue;
            }

            // Read the fragment; avio_read returns a short count at EOF.
            let mut filled = 0usize;
            while filled < frag_size as usize {
                match self
                    .input
                    .read(&mut asf_st.assembled_packet[offset + filled..offset + frag_size as usize])
                {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(Error::Io(e)),
                }
            }
            if filled < frag_size as usize {
                if asf_st.ds_span > 1 {
                    // scrambling: zero-fill the gap, keep the full object
                    for b in &mut asf_st.assembled_packet[offset + filled..offset + frag_size as usize] {
                        *b = 0;
                    }
                    filled = frag_size as usize;
                } else if filled == 0 && offset == 0 {
                    return Err(Error::Eof);
                } else {
                    // FFmpeg av_shrink_packet: emit the partial packet
                    asf_st.assembled_packet.truncate(offset + filled);
                    asf_st.packet_obj_size = (offset + filled) as u32;
                }
            }
            asf_st.assembled_len += filled;

            if asf_st.assembled_len == asf_st.assembled_packet.len()
                && !asf_st.assembled_packet.is_empty()
            {
                let mut data = std::mem::take(&mut asf_st.assembled_packet);
                asf_st.assembled_len = 0;

                if asf_st.ds_span > 1 {
                    let span = asf_st.ds_span as usize;
                    let pkt_size = asf_st.ds_packet_size as usize;
                    let chunk_size = asf_st.ds_chunk_size as usize;
                    if data.len() == pkt_size * span && chunk_size > 0 {
                        let mut newdata = vec![0u8; data.len()];
                        let mut off = 0;
                        while off < data.len() {
                            let block = off / chunk_size;
                            let row = block / span;
                            let col = block % span;
                            let idx = row + col * pkt_size / chunk_size;
                            let src = idx * chunk_size;
                            if src + chunk_size <= data.len() && off + chunk_size <= newdata.len() {
                                newdata[off..off + chunk_size]
                                    .copy_from_slice(&data[src..src + chunk_size]);
                            }
                            off += chunk_size;
                        }
                        data = newdata;
                    }
                }

                if asf_st.skip_to_key {
                    if asf_st.keyframe {
                        asf_st.skip_to_key = false;
                    } else {
                        continue;
                    }
                }

                let time_base = TimeBase::new(1, 1000);
                let mut pkt = Packet::new(stream_idx as u32, time_base, data);
                pkt.pts = Some(asf_st.pts);
                pkt.dts = Some(asf_st.pts);
                pkt.flags.keyframe = asf_st.keyframe;
                return Ok(pkt);
            }
        }
    }
}

struct WaveFormatEx {
    format_tag: u16,
    channels: u16,
    samples_per_sec: u32,
    avg_bytes_per_sec: u32,
    #[allow(dead_code)]
    block_align: u16,
    bits_per_sample: u16,
    extradata: Vec<u8>,
}

struct BitmapInfoHeader {
    width: u32,
    height: u32,
    #[allow(dead_code)]
    planes: u16,
    bit_count: u16,
    compression: [u8; 4],
    #[allow(dead_code)]
    size_image: u32,
    #[allow(dead_code)]
    x_pels_per_meter: i32,
    #[allow(dead_code)]
    y_pels_per_meter: i32,
    #[allow(dead_code)]
    clr_used: u32,
    #[allow(dead_code)]
    clr_important: u32,
    extradata: Vec<u8>,
}

fn fallback_wave_format_codec_id(tag: u16) -> CodecId {
    match tag {
        0x0001 => CodecId::new("pcm_s16le"),
        0x0002 => CodecId::new("adpcm_ms"),
        0x0006 => CodecId::new("pcm_alaw"),
        0x0007 => CodecId::new("pcm_mulaw"),
        0x000A => CodecId::new("wmavoice"),
        0x0011 => CodecId::new("adpcm_ima_wav"),
        0x0055 => CodecId::new("mp3"),
        0x0160 => CodecId::new("wmav1"),
        0x0161 => CodecId::new("wmav2"),
        0x0162 => CodecId::new("wmapro"),
        0x0163 => CodecId::new("wmalossless"),
        0x2000 => CodecId::new("ac3"),
        _ => CodecId::new(format!("0x{:04x}", tag)),
    }
}

fn fallback_fourcc_codec_id(fcc: &[u8; 4]) -> CodecId {
    match fcc {
        b"WMV1" | b"wmv1" => CodecId::new("wmv1"),
        b"WMV2" | b"wmv2" => CodecId::new("wmv2"),
        b"WMV3" | b"wmv3" => CodecId::new("wmv3"),
        b"WVC1" | b"wvc1" | b"WMVA" | b"wmva" => CodecId::new("vc1"),
        b"MP43" | b"mp43" => CodecId::new("msmpeg4v3"),
        b"MP42" | b"mp42" => CodecId::new("msmpeg4v2"),
        b"MP41" | b"mp41" | b"MPG4" | b"mpg4" => CodecId::new("msmpeg4v1"),
        b"MSS1" | b"mss1" => CodecId::new("mss1"),
        b"MSS2" | b"mss2" => CodecId::new("mss2"),
        b"G2M2" | b"G2M3" | b"G2M4" | b"g2m2" | b"g2m3" | b"g2m4" => CodecId::new("g2m"),
        b"TDSC" | b"tdsc" => CodecId::new("tdsc"),
        b"H264" | b"h264" | b"X264" | b"x264" | b"AVC1" | b"avc1" => CodecId::new("h264"),
        b"MJPG" | b"mjpg" => CodecId::new("mjpeg"),
        b"DVR " => CodecId::new("mpeg2video"),
        _ => {
            if let Ok(s) = std::str::from_utf8(fcc) {
                CodecId::new(s.to_ascii_lowercase())
            } else {
                CodecId::new("unknown")
            }
        }
    }
}

fn rfc1766_to_iso639_2(lang: &str) -> Option<&'static str> {
    let tag = if let Some((primary, _)) = lang.split_once('-') {
        primary
    } else {
        lang
    };
    match tag.to_ascii_lowercase().as_str() {
        "en" => Some("eng"),
        "fr" => Some("fre"),
        "de" => Some("ger"),
        "es" => Some("spa"),
        "it" => Some("ita"),
        "ja" => Some("jpn"),
        "zh" => Some("chi"),
        "ru" => Some("rus"),
        "pt" => Some("por"),
        "nl" => Some("dut"),
        "pl" => Some("pol"),
        "ko" => Some("kor"),
        "ar" => Some("ara"),
        "sv" => Some("swe"),
        "da" => Some("dan"),
        "fi" => Some("fin"),
        "no" => Some("nor"),
        "el" => Some("gre"),
        "cs" => Some("cze"),
        "hu" => Some("hun"),
        "tr" => Some("tur"),
        _ => None,
    }
}

fn read_u8(r: &mut dyn Read) -> Result<u8> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b)?;
    Ok(b[0])
}

fn read_u16_le(r: &mut dyn Read) -> Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_le_bytes(b))
}

fn read_u32_le(r: &mut dyn Read) -> Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_i32_le(r: &mut dyn Read) -> Result<i32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(i32::from_le_bytes(b))
}

fn read_u64_le(r: &mut dyn Read) -> Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

fn read_i64_le(r: &mut dyn Read) -> Result<i64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(i64::from_le_bytes(b))
}
