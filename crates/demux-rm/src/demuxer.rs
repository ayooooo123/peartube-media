// Ported from FFmpeg libavformat/rmdec.c (seeking: rm_read_seek,
// rm_read_dts, rm_read_index over libavformat/seek.c), commit 2da55bf
// License: GNU Lesser General Public License (LGPL) version 2.1 or later

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Seek, SeekFrom};

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, CodecTag, Demuxer, Error, MediaType, Packet,
    ProbeContext, Rational, ReadSeek, Result, StreamInfo, TimeBase,
};

use crate::rm_tags::{
    codec_id_from_rm_tag, DEINT_ID_GENR, DEINT_ID_INT0, DEINT_ID_INT4, DEINT_ID_SIPR,
    DEINT_ID_VBRF, DEINT_ID_VBRS, RM_METADATA_KEYS,
};
use crate::rmsipr;
use crate::rv34::Rv34ParserState;
use demux_seek_core::{gen_search, Allowance, Index};

const RAW_PACKET_SIZE: usize = 1000;
const MAX_DIMENSION: u32 = 16384;
const MAX_AREA: u64 = 8192 * 8192;
const MAX_BUFFER_SIZE: usize = 64 * 1024 * 1024;

#[derive(Default)]
struct AudioStreamState {
    deint_id: u32,
    sub_packet_cnt: usize,
    sub_packet_h: usize,
    sub_packet_size: usize,
    coded_framesize: usize,
    audio_framesize: usize,
    block_align: usize,
    sample_rate: u32,
    audiotimestamp: Option<i64>,
    audio_buf: Vec<u8>,
    blocks_emitted: usize,
    partial: bool,
    /// A seek restarted the stream's parser (ff_read_frame_flush). The
    /// new sipr parser fetches no timestamp for its first frame (a cached
    /// packet's pos is -1, before its first frame offset), so that frame
    /// takes the landing dts, and no block after a seek is shifted back.
    restarted: bool,
}

#[derive(Default)]
struct VideoStreamState {
    rv34: Rv34ParserState,
    slices: usize,
    cur_slice: usize,
    curpic_num: i32,
    videobuf: Vec<u8>,
    videobufpos: usize,
    videobufsize: usize,
    pktpos: u64,
    timestamp: Option<i64>,
}

pub struct RmDemuxer {
    io: Box<dyn ReadSeek>,
    streams: Vec<StreamInfo>,
    metadata: Vec<(String, String)>,
    duration_micros: Option<i64>,
    old_format: bool,

    // Header & format state
    current_stream: u16,
    remaining_len: usize,

    // Queued packets
    packet_queue: VecDeque<Packet>,

    // Per-stream audio state (keyed by stream index)
    audio_states: HashMap<usize, AudioStreamState>,

    // Per-stream video state (keyed by stream index)
    video_states: HashMap<usize, VideoStreamState>,

    // Stream id (u32) -> stream index (usize)
    stream_id_to_index: HashMap<u32, usize>,

    // Seek index per stream: INDX entries and the key packets
    // rm_read_dts met (FFStream.index_entries).
    seek_index: Vec<Index>,

    // First data packet (si->data_offset).
    data_offset: u64,

    // Rolling state for rm_sync
    sync_state: u32,
    // Offset of the packet rm_sync found last.
    sync_pos: u64,

    // Last emitted pts per audio stream index (parser grid fill)
    last_audio_pts: HashMap<u32, i64>,
    // After a seek, the dts of the next packet without a timestamp per
    // audio stream (avpriv_update_cur_dts).
    cur_dts: HashMap<u32, i64>,

    /// What the seek under way may still read.
    allowance: Allowance,
}


/// FFmpeg's readfull(): read exactly `n` bytes into `dst`; on a short read
/// zero-fill the rest and report `false` (the caller keeps the data —
/// truncated trailing packets are surfaced with the corrupt flag).
fn read_full(io: &mut Box<dyn ReadSeek>, dst: &mut [u8]) -> bool {
    // Count what arrives: read_exact would consume a partial tail and then
    // leave its contents unspecified.
    let mut n = 0;
    while n < dst.len() {
        match io.read(&mut dst[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    dst[n..].fill(0);
    n == dst.len()
}

fn get_num<R: Read + ?Sized>(reader: &mut R, len: &mut usize) -> Result<usize> {
    if *len < 2 {
        return Err(Error::invalid("get_num: insufficient length"));
    }
    let mut b2 = [0u8; 2];
    reader.read_exact(&mut b2)?;
    *len -= 2;
    let n = (u16::from_be_bytes(b2) & 0x7FFF) as usize;
    if n >= 0x4000 {
        Ok(n - 0x4000)
    } else {
        if *len < 2 {
            return Err(Error::invalid("get_num: insufficient length for 32-bit num"));
        }
        reader.read_exact(&mut b2)?;
        *len -= 2;
        let n1 = u16::from_be_bytes(b2) as usize;
        Ok((n << 16) | n1)
    }
}

fn get_strl<R: Read + ?Sized>(reader: &mut R, len: usize) -> Result<String> {
    if len > 65536 {
        return Err(Error::invalid("string length too large"));
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    let s = String::from_utf8_lossy(&buf);
    Ok(s.trim_end_matches('\0').to_string())
}

fn get_str8<R: Read + ?Sized>(reader: &mut R) -> Result<String> {
    let mut len_b = [0u8; 1];
    reader.read_exact(&mut len_b)?;
    get_strl(reader, len_b[0] as usize)
}

fn read_metadata<R: Read + ?Sized>(
    reader: &mut R,
    wide: bool,
    metadata: &mut Vec<(String, String)>,
) -> Result<()> {
    for key in RM_METADATA_KEYS {
        let len = if wide {
            let mut b = [0u8; 2];
            reader.read_exact(&mut b)?;
            u16::from_be_bytes(b) as usize
        } else {
            let mut b = [0u8; 1];
            reader.read_exact(&mut b)?;
            b[0] as usize
        };
        if len > 0 {
            let val = get_strl(reader, len)?;
            if !val.is_empty() {
                metadata.push((key.to_string(), val));
            }
        }
    }
    Ok(())
}

/// `logical-fileinfo` property list from ff_rm_read_mdpr_codecdata: version,
/// stream/rule counts, then property_count (u32 size, u16 version, name,
/// typed value) records. Unrecognized records are skipped by their length.
fn read_metadata_properties<R: Read + Seek + ?Sized>(
    reader: &mut R,
    metadata: &mut Vec<(String, String)>,
) -> Result<()> {
    let mut b2 = [0u8; 2];
    let mut b4 = [0u8; 4];

    reader.read_exact(&mut b2)?; // version
    let stream_count = u16::from_be_bytes(b2) as usize;
    reader.seek(SeekFrom::Current((6 * stream_count) as i64))?;
    reader.read_exact(&mut b2)?;
    let rule_count = u16::from_be_bytes(b2) as usize;
    reader.seek(SeekFrom::Current((2 * rule_count) as i64))?;
    reader.read_exact(&mut b2)?;
    let property_count = u16::from_be_bytes(b2) as usize;
    for _ in 0..property_count {
        reader.seek(SeekFrom::Current(4))?;
        reader.read_exact(&mut b2)?; // version
        let name = get_str8(reader)?;
        reader.read_exact(&mut b4)?;
        let prop_type = u32::from_be_bytes(b4);
        if prop_type == 2 {
            reader.read_exact(&mut b2)?;
            let val_len = u16::from_be_bytes(b2) as usize;
            let val = get_strl(reader, val_len)?;
            metadata.push((name, val));
        } else {
            reader.read_exact(&mut b2)?;
            let skip_len = u16::from_be_bytes(b2) as i64;
            reader.seek(SeekFrom::Current(skip_len))?;
        }
    }
    Ok(())
}

fn read_audio_stream_info<R: Read + Seek + ?Sized>(
    reader: &mut R,
    read_all: bool,
    codecs: &dyn CodecResolver,
    metadata: &mut Vec<(String, String)>,
) -> Result<(CodecParameters, AudioStreamState)> {
    let mut b2 = [0u8; 2];
    let mut b4 = [0u8; 4];

    reader.read_exact(&mut b2)?;
    let version = u16::from_be_bytes(b2);

    if version == 3 {
        reader.read_exact(&mut b2)?;
        let header_size = u16::from_be_bytes(b2) as u64;
        let startpos = reader.stream_position()?;
        reader.seek(SeekFrom::Current(8))?;
        reader.read_exact(&mut b2)?;
        let bytes_per_minute = u16::from_be_bytes(b2) as u64;
        reader.seek(SeekFrom::Current(4))?;
        read_metadata(reader, false, metadata)?;

        let curpos = reader.stream_position()?;
        if startpos + header_size >= curpos + 2 {
            let mut b1 = [0u8; 1];
            reader.read_exact(&mut b1)?;
            let _ = get_str8(reader)?;
        }
        let curpos = reader.stream_position()?;
        if startpos + header_size > curpos {
            reader.seek(SeekFrom::Current((startpos + header_size - curpos) as i64))?;
        }

        let bit_rate = if bytes_per_minute > 0 {
            Some(8 * bytes_per_minute / 60)
        } else {
            None
        };

        let mut params = CodecParameters::audio(CodecId::new("ra_144"));
        params.sample_rate = Some(8000);
        params.channels = Some(1);
        params.bit_rate = bit_rate;
        params.tag = Some(CodecTag::fourcc(b"14_4"));

        let audio_state = AudioStreamState {
            deint_id: DEINT_ID_INT0,
            sample_rate: 8000,
            ..Default::default()
        };

        Ok((params, audio_state))
    } else {
        reader.seek(SeekFrom::Current(2))?; // unused
        reader.read_exact(&mut b4)?; // .ra4
        reader.read_exact(&mut b4)?; // data size
        reader.read_exact(&mut b2)?; // version2
        reader.read_exact(&mut b4)?; // header size

        reader.read_exact(&mut b2)?;
        let flavor = u16::from_be_bytes(b2) as usize;

        reader.read_exact(&mut b4)?;
        let coded_framesize = u32::from_be_bytes(b4) as usize;

        reader.seek(SeekFrom::Current(4))?; // ???
        reader.read_exact(&mut b4)?;
        let bytes_per_minute = u32::from_be_bytes(b4) as u64;

        reader.seek(SeekFrom::Current(4))?; // ???

        reader.read_exact(&mut b2)?;
        let sub_packet_h = u16::from_be_bytes(b2) as usize;

        reader.read_exact(&mut b2)?;
        let block_align = u16::from_be_bytes(b2) as usize;

        reader.read_exact(&mut b2)?;
        let sub_packet_size = u16::from_be_bytes(b2) as usize;

        reader.seek(SeekFrom::Current(2))?; // ???
        if version == 5 {
            reader.seek(SeekFrom::Current(6))?;
        }

        reader.read_exact(&mut b2)?;
        let sample_rate = u16::from_be_bytes(b2) as u32;

        reader.seek(SeekFrom::Current(4))?;

        reader.read_exact(&mut b2)?;
        let channels = u16::from_be_bytes(b2);

        let (deint_id, codec_tag_raw) = if version == 5 {
            reader.read_exact(&mut b4)?;
            let deint = u32::from_le_bytes(b4);
            let mut tag_b = [0u8; 4];
            reader.read_exact(&mut tag_b)?;
            (deint, tag_b)
        } else {
            let desc1 = get_str8(reader)?;
            let mut deint_bytes = [0u8; 4];
            let bytes1 = desc1.as_bytes();
            deint_bytes[..bytes1.len().min(4)].copy_from_slice(&bytes1[..bytes1.len().min(4)]);
            let deint = u32::from_le_bytes(deint_bytes);

            let desc2 = get_str8(reader)?;
            let mut tag_b = [0u8; 4];
            let bytes2 = desc2.as_bytes();
            tag_b[..bytes2.len().min(4)].copy_from_slice(&bytes2[..bytes2.len().min(4)]);
            (deint, tag_b)
        };

        let tag = CodecTag::fourcc(&codec_tag_raw);
        let codec_id = codecs
            .resolve_tag(&ProbeContext::new(&tag))
            .or_else(|| codec_id_from_rm_tag(&codec_tag_raw).map(|(id, _)| id))
            .unwrap_or_else(|| CodecId::new(String::from_utf8_lossy(&codec_tag_raw).to_ascii_lowercase()));

        let mut params = CodecParameters::audio(codec_id.clone());
        params.sample_rate = Some(sample_rate);
        params.channels = Some(channels);
        params.tag = Some(tag);
        if version == 4 && bytes_per_minute > 0 {
            params.bit_rate = Some(8 * bytes_per_minute / 60);
        }

        let mut effective_block_align = block_align;
        let mut audio_framesize = block_align;

        match codec_id.as_str() {
            "ra_288" => {
                audio_framesize = block_align;
                effective_block_align = coded_framesize;
            }
            "cook" | "atrac3" | "sipr" => {
                let codecdata_length = if read_all {
                    0
                } else {
                    reader.seek(SeekFrom::Current(3))?; // rb16, r8
                    if version == 5 {
                        reader.seek(SeekFrom::Current(1))?;
                    }
                    reader.read_exact(&mut b4)?;
                    u32::from_be_bytes(b4) as usize
                };
                audio_framesize = block_align;
                if codec_id.as_str() == "sipr" {
                    if flavor > 3 {
                        return Err(Error::invalid("bad SIPR flavor"));
                    }
                    effective_block_align = rmsipr::FF_SIPR_SUBPK_SIZE[flavor];
                } else {
                    if sub_packet_size > 0 {
                        effective_block_align = sub_packet_size;
                    }
                }
                if codecdata_length > 0 {
                    if codecdata_length > MAX_BUFFER_SIZE {
                        return Err(Error::invalid("extradata too large"));
                    }
                    let mut ed = vec![0u8; codecdata_length];
                    reader.read_exact(&mut ed)?;
                    params.extradata = ed;
                }
            }
            "aac" => {
                reader.seek(SeekFrom::Current(3))?; // rb16, r8
                if version == 5 {
                    reader.seek(SeekFrom::Current(1))?;
                }
                reader.read_exact(&mut b4)?;
                let codecdata_length = u32::from_be_bytes(b4) as usize;
                if codecdata_length >= 1 {
                    reader.seek(SeekFrom::Current(1))?; // skip 1 byte
                    let ed_len = codecdata_length - 1;
                    if ed_len > MAX_BUFFER_SIZE {
                        return Err(Error::invalid("extradata too large"));
                    }
                    let mut ed = vec![0u8; ed_len];
                    reader.read_exact(&mut ed)?;
                    params.extradata = ed;
                }
            }
            _ => {}
        }

        if read_all {
            reader.seek(SeekFrom::Current(3))?; // r8, r8, r8
            read_metadata(reader, false, metadata)?;
        }

        let audio_buf = if matches!(deint_id, DEINT_ID_INT4 | DEINT_ID_GENR | DEINT_ID_SIPR) {
            let buf_size = audio_framesize
                .checked_mul(sub_packet_h)
                .ok_or_else(|| Error::invalid("audio buffer size overflow"))?;
            if buf_size > MAX_BUFFER_SIZE {
                return Err(Error::invalid("audio buffer exceeds allocation limit"));
            }
            vec![0u8; buf_size]
        } else {
            Vec::new()
        };

        let audio_state = AudioStreamState {
            deint_id,
            sub_packet_cnt: 0,
            sub_packet_h,
            sub_packet_size,
            coded_framesize,
            audio_framesize,
            block_align: effective_block_align,
            sample_rate,
            audiotimestamp: None,
            audio_buf,
            blocks_emitted: 0,
            partial: false,
            restarted: false,
        };

        Ok((params, audio_state))
    }
}

pub fn open(
    mut input: Box<dyn ReadSeek>,
    codecs: &dyn CodecResolver,
) -> Result<Box<dyn Demuxer>> {
    let mut b4 = [0u8; 4];
    input.read_exact(&mut b4)?;

    let mut streams = Vec::new();
    let mut metadata = Vec::new();
    let mut duration_micros = None;
    let mut old_format = false;
    let mut audio_states = HashMap::new();
    let mut video_states = HashMap::new();
    let mut stream_id_to_index = HashMap::new();
    let mut seek_index: Vec<Index> = Vec::new();
    let mut data_offset = 0;

    if &b4 == b".ra\xfd" {
        old_format = true;
        let (params, audio_state) = read_audio_stream_info(&mut *input, true, codecs, &mut metadata)?;
        let stream_info = StreamInfo {
            index: 0,
            time_base: TimeBase::new(1, 90_000),
            duration: None,
            start_time: Some(0),
            params,
        };
        streams.push(stream_info);
        audio_states.insert(0, audio_state);
        stream_id_to_index.insert(0, 0);
    } else if &b4 == b".RMF" || &b4 == b".RMP" {
        let mut b2 = [0u8; 2];
        input.read_exact(&mut b4)?;
        let tag_size = u32::from_be_bytes(b4) as i64;
        if tag_size < 8 {
            return Err(Error::invalid("invalid RM header size"));
        }
        input.seek(SeekFrom::Current(tag_size - 8))?;

        let mut indx_off: Option<u64> = None;

        loop {
            let res = input.read_exact(&mut b4);
            if res.is_err() {
                break;
            }
            let chunk_tag = b4;
            input.read_exact(&mut b4)?;
            let chunk_size = u32::from_be_bytes(b4) as usize;
            input.read_exact(&mut b2)?;
            let chunk_ver = u16::from_be_bytes(b2);

            if chunk_size < 10 && &chunk_tag != b"DATA" {
                return Err(Error::invalid("chunk size too small"));
            }

            match &chunk_tag {
                b"PROP" => {
                    input.seek(SeekFrom::Current(16))?; // max_bit_rate, avg_bit_rate, max_packet_size, avg_packet_size
                    input.read_exact(&mut b4)?; // nb_packets (informational)
                    input.read_exact(&mut b4)?;
                    let dur_ms = u32::from_be_bytes(b4) as i64;
                    duration_micros = Some(dur_ms * 1000);
                    input.seek(SeekFrom::Current(4))?; // preroll

                    if chunk_ver == 0 {
                        input.read_exact(&mut b4)?;
                        let off = u32::from_be_bytes(b4);
                        if off > 0 {
                            indx_off = Some(off as u64);
                        }
                    } else {
                        let mut b8 = [0u8; 8];
                        input.read_exact(&mut b8)?;
                        let off = u64::from_be_bytes(b8);
                        if off > 0 {
                            indx_off = Some(off);
                        }
                    }

                    input.read_exact(&mut b4)?; // data offset
                    input.read_exact(&mut b2)?; // nb_streams
                    input.read_exact(&mut b2)?; // flags
                }
                b"CONT" => {
                    read_metadata(&mut *input, true, &mut metadata)?;
                }
                b"MDPR" => {
                    input.read_exact(&mut b2)?;
                    let stream_id = u16::from_be_bytes(b2);
                    input.seek(SeekFrom::Current(4))?; // max_bit_rate
                    input.read_exact(&mut b4)?;
                    let bit_rate = u32::from_be_bytes(b4) as u64;
                    input.seek(SeekFrom::Current(8))?; // max_packet_size, avg_packet_size
                    input.read_exact(&mut b4)?;
                    let start_time = u32::from_be_bytes(b4) as i64;
                    input.seek(SeekFrom::Current(4))?; // preroll
                    input.read_exact(&mut b4)?;
                    let dur = u32::from_be_bytes(b4) as i64;

                    let _desc = get_str8(&mut *input)?;
                    let mime = get_str8(&mut *input)?;

                    input.read_exact(&mut b4)?;
                    let codec_data_size = u32::from_be_bytes(b4) as usize;
                    let codec_pos = input.stream_position()?;

                    if mime == "logical-fileinfo" {
                        // ff_rm_read_mdpr_codecdata consumed the 4-byte v before this branch
                        // runs in FFmpeg; properties start right after it, and the object is
                        // skipped to its end either way.
                        let mut v_bytes = [0u8; 4];
                        input.read_exact(&mut v_bytes)?;
                        let parse_result = read_metadata_properties(&mut *input, &mut metadata);
                        if parse_result.is_err() || v_bytes != 0u32.to_be_bytes() {
                            // Malformed properties: keep the stream, drop the metadata.
                        }
                        input.seek(SeekFrom::Start(codec_pos + codec_data_size as u64))?;
                    } else if codec_data_size > 4 {
                        input.read_exact(&mut b4)?;
                        if &b4 == b"MLTI" {
                            input.read_exact(&mut b2)?;
                            let num_streams = u16::from_be_bytes(b2) as usize;
                            input.seek(SeekFrom::Current((2 * num_streams) as i64))?;
                            input.read_exact(&mut b2)?;
                            let num_mdpr = u16::from_be_bytes(b2) as usize;
                            for i in 0..num_mdpr {
                                input.read_exact(&mut b4)?;
                                let size2 = u32::from_be_bytes(b4) as usize;
                                let sub_codec_pos = input.stream_position()?;
                                let full_id = (stream_id as u32) + ((i as u32) << 16);

                                let (params, a_state, v_state) = read_mdpr_codecdata(
                                    &mut *input,
                                    size2,
                                    codecs,
                                    &mut metadata,
                                )?;

                                let idx = streams.len();
                                let mut p = params;
                                if p.bit_rate.is_none() && bit_rate > 0 {
                                    p.bit_rate = Some(bit_rate);
                                }
                                streams.push(StreamInfo {
                                    index: idx as u32,
                                    time_base: TimeBase::MILLIS,
                                    duration: if dur > 0 { Some(dur) } else { None },
                                    start_time: Some(start_time),
                                    params: p,
                                });
                                stream_id_to_index.insert(full_id, idx);
                                if let Some(a) = a_state {
                                    audio_states.insert(idx, a);
                                }
                                if let Some(v) = v_state {
                                    video_states.insert(idx, v);
                                }

                                let cur = input.stream_position()?;
                                if cur < sub_codec_pos + size2 as u64 {
                                    input.seek(SeekFrom::Start(sub_codec_pos + size2 as u64))?;
                                }
                            }
                        } else {
                            input.seek(SeekFrom::Current(-4))?;
                            let (params, a_state, v_state) = read_mdpr_codecdata(
                                &mut *input,
                                codec_data_size,
                                codecs,
                                &mut metadata,
                            )?;
                            let idx = streams.len();
                            let mut p = params;
                            if p.bit_rate.is_none() && bit_rate > 0 {
                                p.bit_rate = Some(bit_rate);
                            }
                            streams.push(StreamInfo {
                                index: idx as u32,
                                time_base: TimeBase::MILLIS,
                                duration: if dur > 0 { Some(dur) } else { None },
                                start_time: Some(start_time),
                                params: p,
                            });
                            stream_id_to_index.insert(stream_id as u32, idx);
                            if let Some(a) = a_state {
                                audio_states.insert(idx, a);
                            }
                            if let Some(v) = v_state {
                                video_states.insert(idx, v);
                            }
                        }
                    }

                    let cur = input.stream_position()?;
                    if cur < codec_pos + codec_data_size as u64 {
                        input.seek(SeekFrom::Start(codec_pos + codec_data_size as u64))?;
                    }
                }
                b"DATA" => {
                    // header_end: number of packets, 12 more bytes in
                    // version 2, the next data header's offset. Reading
                    // never follows that offset: a DATA tag met while
                    // reading is logged and scanned past (rmdec.c:745-751).
                    input.read_exact(&mut b4)?;
                    if chunk_ver == 2 {
                        input.seek(SeekFrom::Current(12))?;
                    }
                    input.read_exact(&mut b4)?;
                    break;
                }
                _ => {
                    input.seek(SeekFrom::Current((chunk_size - 10) as i64))?;
                }
            }
        }

        let data_start = input.stream_position()?;
        data_offset = data_start;
        seek_index = streams.iter().map(|_| Index::default()).collect();

        // If an index was announced and seekable, parse INDX chunks. Like
        // rm_read_header, a broken index only ends the index.
        if let Some(idx_pos) = indx_off {
            if input.seek(SeekFrom::Start(idx_pos)).is_ok() {
                let _ = read_index(&mut *input, &stream_id_to_index, &mut seek_index);
            }
            input.seek(SeekFrom::Start(data_start))?;
        }
    } else {
        return Err(Error::invalid("not a RealMedia file"));
    }

    let allowance = Allowance::default();
    Ok(Box::new(RmDemuxer {
        io: Box::new(allowance.meter(input)),
        streams,
        metadata,
        duration_micros,
        old_format,
        current_stream: 0,
        remaining_len: 0,
        packet_queue: VecDeque::new(),
        audio_states,
        video_states,
        stream_id_to_index,
        seek_index,
        data_offset,
        sync_state: 0xFFFFFFFF,
        sync_pos: 0,
        last_audio_pts: HashMap::new(),
        cur_dts: HashMap::new(),
        allowance,
    }))
}

fn read_mdpr_codecdata<R: Read + Seek + ?Sized>(
    reader: &mut R,
    codec_data_size: usize,
    codecs: &dyn CodecResolver,
    metadata: &mut Vec<(String, String)>,
) -> Result<(CodecParameters, Option<AudioStreamState>, Option<VideoStreamState>)> {
    let codec_pos = reader.stream_position()?;
    let mut b4 = [0u8; 4];
    let mut b2 = [0u8; 2];

    reader.read_exact(&mut b4)?;
    if &b4 == b".ra\xfd" {
        let (params, a_state) = read_audio_stream_info(reader, false, codecs, metadata)?;
        Ok((params, Some(a_state), None))
    } else if &b4 == b"LSD:" {
        reader.seek(SeekFrom::Start(codec_pos))?;
        if codec_data_size > MAX_BUFFER_SIZE {
            return Err(Error::invalid("RALF extradata too large"));
        }
        let mut ed = vec![0u8; codec_data_size];
        reader.read_exact(&mut ed)?;

        let mut params = CodecParameters::audio(CodecId::new("ralf"));
        params.tag = Some(CodecTag::fourcc(b"LSD:"));
        if ed.len() >= 18 {
            let ch = u16::from_be_bytes([ed[8], ed[9]]);
            let sr = u32::from_be_bytes([ed[12], ed[13], ed[14], ed[15]]);
            params.channels = Some(ch);
            params.sample_rate = Some(sr);
        }
        params.extradata = ed;
        Ok((params, Some(AudioStreamState::default()), None))
    } else {
        // Video: check for "VIDO"
        reader.read_exact(&mut b4)?;
        if &b4 != b"VIDO" {
            return Err(Error::invalid("unsupported RealMedia stream type"));
        }
        reader.read_exact(&mut b4)?;
        let tag_raw = b4;
        let tag = CodecTag::fourcc(&tag_raw);
        let codec_id = codecs
            .resolve_tag(&ProbeContext::new(&tag))
            .or_else(|| codec_id_from_rm_tag(&tag_raw).map(|(id, _)| id))
            .unwrap_or_else(|| CodecId::new(String::from_utf8_lossy(&tag_raw).to_ascii_lowercase()));

        let mut params = CodecParameters::video(codec_id);
        params.tag = Some(tag);

        reader.read_exact(&mut b2)?;
        let width = u16::from_be_bytes(b2) as u32;
        reader.read_exact(&mut b2)?;
        let height = u16::from_be_bytes(b2) as u32;

        if width > MAX_DIMENSION || height > MAX_DIMENSION || (width as u64 * height as u64) > MAX_AREA {
            return Err(Error::invalid("video dimensions exceed limits"));
        }
        params.width = Some(width);
        params.height = Some(height);

        reader.seek(SeekFrom::Current(6))?; // 2 bytes bits per sample, 4 bytes zero
        reader.read_exact(&mut b4)?;
        let fps = u32::from_be_bytes(b4);
        if fps > 0 {
            params.frame_rate = Some(Rational::new(fps as i64, 65536));
        }

        let cur = reader.stream_position()?;
        let consumed = (cur - codec_pos) as usize;
        if codec_data_size > consumed {
            let ed_size = codec_data_size - consumed;
            if ed_size > MAX_BUFFER_SIZE {
                return Err(Error::invalid("video extradata too large"));
            }
            let mut ed = vec![0u8; ed_size];
            reader.read_exact(&mut ed)?;
            params.extradata = ed;
        }

        let v_state = VideoStreamState {
            curpic_num: -1,
            ..Default::default()
        };

        Ok((params, None, Some(v_state)))
    }
}

/// rm_read_index: the INDX chunks from the reader's position, every entry
/// a key frame of its stream (av_add_index_entry). An error ends the
/// index; the entries read so far stay. A version-2 entry's 64-bit
/// position outside the file is not indexed: FFmpeg keeps it and its
/// search arithmetic overflows on it.
fn read_index<R: Read + Seek + ?Sized>(
    reader: &mut R,
    stream_id_to_index: &HashMap<u32, usize>,
    index: &mut [Index],
) -> Result<()> {
    let start = reader.stream_position()?;
    let file_size = reader.seek(SeekFrom::End(0))?;
    reader.seek(SeekFrom::Start(start))?;
    let mut b8 = [0u8; 8];
    loop {
        reader.read_exact(&mut b8[..4])?;
        if &b8[..4] != b"INDX" {
            return Ok(());
        }
        reader.read_exact(&mut b8[..4])?;
        let size = u32::from_be_bytes([b8[0], b8[1], b8[2], b8[3]]);
        if size < 20 {
            return Ok(());
        }
        reader.read_exact(&mut b8[..2])?;
        let ver = u16::from_be_bytes([b8[0], b8[1]]);
        if ver != 0 && ver != 2 {
            return Err(Error::invalid("rm: unknown index version"));
        }
        reader.read_exact(&mut b8[..4])?;
        let n_pkts = u32::from_be_bytes([b8[0], b8[1], b8[2], b8[3]]);
        reader.read_exact(&mut b8[..2])?;
        let str_id = u16::from_be_bytes([b8[0], b8[1]]);
        reader.read_exact(&mut b8[..4])?;
        let next_off = u64::from(u32::from_be_bytes([b8[0], b8[1], b8[2], b8[3]]));
        if ver == 2 {
            reader.seek(SeekFrom::Current(4))?;
        }
        let tell = reader.stream_position()?;
        // "Invalid stream index", or "Nr. of packets in packet index
        // exceeds filesize" (counted in 14-byte entries): skip the chunk.
        let stream = stream_id_to_index
            .get(&u32::from(str_id))
            .filter(|_| (file_size as i64 - tell as i64) / 14 >= i64::from(n_pkts));
        if let Some(&s) = stream {
            for _ in 0..n_pkts {
                reader.seek(SeekFrom::Current(2))?;
                reader.read_exact(&mut b8[..4])?;
                let pts = i64::from(u32::from_be_bytes([b8[0], b8[1], b8[2], b8[3]]));
                let pos = if ver == 0 {
                    reader.read_exact(&mut b8[..4])?;
                    i64::from(u32::from_be_bytes([b8[0], b8[1], b8[2], b8[3]]))
                } else {
                    reader.read_exact(&mut b8)?;
                    i64::from_be_bytes(b8)
                };
                reader.seek(SeekFrom::Current(4))?; // packet no.
                if u64::try_from(pos).is_ok_and(|pos| pos < file_size) {
                    index[s].add(pos, pts, 0, 0, true);
                }
            }
        }
        if next_off == 0 {
            return Ok(());
        }
        if reader.stream_position()? < next_off {
            reader.seek(SeekFrom::Start(next_off))?;
        }
    }
}

impl RmDemuxer {
    fn sync_next_packet(&mut self) -> Result<Option<(usize, usize, i64, u8)>> {
        if self.remaining_len > 0 {
            let num = self.current_stream;
            let len = self.remaining_len;
            let s_idx = match self.stream_id_to_index.get(&(num as u32)) {
                Some(&i) => i,
                None => return Ok(None),
            };
            // rmdec.c: a continuation carries AV_NOPTS_VALUE — the frame's pts
            // comes from the chunk that started it, except for type-3 frames
            // where the in-payload pos overwrites it.
            return Ok(Some((s_idx, len, i64::MIN, 0)));
        }

        let mut b1 = [0u8; 1];
        let mut b2 = [0u8; 2];
        let mut b4 = [0u8; 4];

        while {
            match self.io.read_exact(&mut b1) {
                Ok(()) => true,
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
                Err(e) => return Err(Error::from(e)),
            }
        } {
            self.sync_state = (self.sync_state << 8) | (b1[0] as u32);

            if self.sync_state == u32::from_be_bytes(*b"INDX") {
                // Skip index chunk
                self.io.read_exact(&mut b4)?;
                let len = u32::from_be_bytes(b4) as usize;
                self.io.read_exact(&mut b2)?;
                let ver = u16::from_be_bytes(b2);
                self.io.read_exact(&mut b4)?;
                let n_pkts = u32::from_be_bytes(b4) as usize;
                let expected_len = if ver == 0 {
                    20 + n_pkts as i64 * 14
                } else {
                    24 + n_pkts as i64 * 18
                };
                // Some files don't add index entries to the chunk size (rmdec.c).
                let real_len = if len == 20 && expected_len <= i64::from(i32::MAX) {
                    expected_len as usize
                } else {
                    len
                };
                if real_len >= 14 {
                    self.io
                        .seek(SeekFrom::Current((real_len - 14) as i64))?;
                }
                self.sync_state = 0xFFFFFFFF;
                continue;
            }

            // A DATA tag here is "in middle of chunk": FFmpeg logs it and
            // scans on (rmdec.c:745-751), as for any other state above 0xFFFF.
            if self.sync_state > 0xFFFF || self.sync_state <= 12 {
                continue;
            }

            let len = (self.sync_state - 12) as usize;
            self.sync_state = 0xFFFFFFFF;
            // rm_sync's *pos: the packet starts at its version field.
            self.sync_pos = self.io.stream_position()?.saturating_sub(4);

            // The trailing bytes of a truncated file can look like a packet
            // header; a header cut off by EOF ends the stream (rmdec.c's
            // avio_feof check stops the scan the same way).
            let read_hdr = |io: &mut Box<dyn ReadSeek>, buf: &mut [u8]| -> Result<bool> {
                match io.read_exact(buf) {
                    Ok(()) => Ok(true),
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
                    Err(e) => Err(Error::from(e)),
                }
            };
            if !read_hdr(&mut self.io, &mut b2)? {
                return Ok(None);
            }
            let num = u16::from_be_bytes(b2);
            if !read_hdr(&mut self.io, &mut b4)? {
                return Ok(None);
            }
            let timestamp = u32::from_be_bytes(b4) as i64;
            if !read_hdr(&mut self.io, &mut b1)? {
                return Ok(None);
            }
            let mlti_byte = b1[0];
            let mlti_id = (mlti_byte.wrapping_shr(1) as i32 - 1).max(0) as u32 * 0x10000;
            if !read_hdr(&mut self.io, &mut b1)? {
                return Ok(None);
            }
            let flags = b1[0];

            let full_id = mlti_id + (num as u32);
            let s_idx = match self.stream_id_to_index.get(&full_id) {
                Some(&i) => i,
                None => {
                    self.allowance.spend(1, 0)?;
                    self.io.seek(SeekFrom::Current(len as i64))?;
                    continue;
                }
            };

            if std::env::var("RM_DEBUG").is_ok() {
                eprintln!("chunk stream={s_idx} len={len} ts={timestamp} flags={flags:#x}");
            }
            return Ok(Some((s_idx, len, timestamp, flags)));
        }

        Ok(None)
    }

    fn assemble_video_frame(
        &mut self,
        stream_idx: usize,
        mut len: usize,
        mut timestamp: i64,
        flags: u8,
    ) -> Result<Option<Packet>> {
        let vst = self.video_states.get_mut(&stream_idx).unwrap();
        let mut b1 = [0u8; 1];
        self.io.read_exact(&mut b1)?;
        len -= 1;
        let hdr = b1[0];
        let frame_type = hdr >> 6;

        let mut seq = 0;
        let mut len2 = 0;
        let mut pos = 0;
        let mut pic_num = 0;

        if frame_type != 3 {
            self.io.read_exact(&mut b1)?;
            len -= 1;
            seq = b1[0] as usize;
        }

        if frame_type != 1 {
            len2 = get_num(&mut *self.io, &mut len)?;
            pos = get_num(&mut *self.io, &mut len)?;
            self.io.read_exact(&mut b1)?;
            len -= 1;
            pic_num = b1[0] as i32;
        }

        self.remaining_len = len;

        if (frame_type & 1) != 0 {
            // Frame, not slice
            let subframe_len = if frame_type == 3 {
                timestamp = pos as i64;
                len2
            } else {
                len
            };

            if self.remaining_len < subframe_len {
                return Err(Error::invalid("insufficient remaining len for subframe"));
            }
            self.remaining_len -= subframe_len;

            if subframe_len > MAX_BUFFER_SIZE {
                return Err(Error::invalid("video frame exceeds allocation limit"));
            }
            let mut data = vec![0u8; subframe_len + 9];
            data[0] = 0;
            data[1..5].copy_from_slice(&1u32.to_le_bytes());
            data[5..9].copy_from_slice(&0u32.to_le_bytes());
            // ffio_read_size in rmdec.c: a short read fails the packet, and
            // rm_read_packet propagates the error instead of emitting it.
            if self.io.read_exact(&mut data[9..9 + subframe_len]).is_err() {
                return Err(Error::Eof);
            }

            // rv34 parser pts correction (FFmpeg attaches the parser to
            // rv30/rv40 streams; see rv34.rs).
            let codec_id = self.streams[stream_idx].params.codec_id.as_str().to_owned();
            let pts = vst
                .rv34
                .correct_pts(&codec_id, Some(timestamp), &data);
            let mut pkt = Packet::new(stream_idx as u32, TimeBase::MILLIS, data);
            if let Some(pts) = pts {
                pkt = pkt.with_pts(pts);
            }
            pkt = pkt.with_keyframe((flags & 2) != 0);
            return Ok(Some(pkt));
        }

        // Single slice
        if (seq & 0x7F) == 1 || vst.curpic_num != pic_num {
            vst.slices = (((hdr & 0x3F) as usize) << 1) + 1;
            vst.videobufsize = len2 + 8 * vst.slices + 1;
            if vst.videobufsize > MAX_BUFFER_SIZE {
                return Err(Error::invalid("assembled video frame exceeds allocation limit"));
            }
            vst.videobuf = vec![0u8; vst.videobufsize];
            vst.videobufpos = 8 * vst.slices + 1;
            vst.cur_slice = 0;
            vst.curpic_num = pic_num;
            vst.pktpos = self.io.stream_position()?;
            vst.timestamp = Some(timestamp);
        }

        let slice_len = if frame_type == 2 {
            len.min(pos)
        } else {
            len
        };

        vst.cur_slice += 1;
        if vst.cur_slice > vst.slices {
            self.io.seek(SeekFrom::Current(slice_len as i64))?;
            self.remaining_len = self.remaining_len.saturating_sub(slice_len);
            return Ok(None);
        }

        let slice_hdr_offset = 8 * vst.cur_slice - 7;
        if slice_hdr_offset + 8 <= vst.videobuf.len() {
            vst.videobuf[slice_hdr_offset..slice_hdr_offset + 4].copy_from_slice(&1u32.to_le_bytes());
            let data_offset_in_body = (vst.videobufpos - 8 * vst.slices - 1) as u32;
            vst.videobuf[slice_hdr_offset + 4..slice_hdr_offset + 8]
                .copy_from_slice(&data_offset_in_body.to_le_bytes());
        }

        if vst.videobufpos + slice_len > vst.videobufsize {
            return Err(Error::invalid("outside videobufsize"));
        }
        // FFmpeg reads slices with ffio_read_size and drops the frame on a
        // short read (ff_rm_parse_packet returns the error); zero-padding a
        // truncated trailing slice would emit a frame FFmpeg never does.
        if self
            .io
            .read_exact(&mut vst.videobuf[vst.videobufpos..vst.videobufpos + slice_len])
            .is_err()
        {
            vst.videobufpos = 0;
            vst.slices = 0;
            return Err(Error::Eof);
        }
        vst.videobufpos += slice_len;
        self.remaining_len = self.remaining_len.saturating_sub(slice_len);

        if frame_type == 2 || vst.videobufpos == vst.videobufsize {
            vst.videobuf[0] = (vst.cur_slice - 1) as u8;
            if vst.slices != vst.cur_slice {
                let actual_header_end = 1 + 8 * vst.cur_slice;
                let old_header_end = 1 + 8 * vst.slices;
                let body_len = vst.videobufpos.saturating_sub(old_header_end);
                vst.videobuf.copy_within(old_header_end..old_header_end + body_len, actual_header_end);
                vst.videobuf.truncate(actual_header_end + body_len);
            } else {
                vst.videobuf.truncate(vst.videobufpos);
            }

            let data = std::mem::take(&mut vst.videobuf);
            // rmdec.c: rm_assemble_video_frame sets pkt->pts = AV_NOPTS_VALUE,
            // then ff_rm_parse_packet falls through and overwrites it with the
            // current chunk's timestamp (the completing slice's, or `pos` for
            // type-3 frames). Slices arriving later in one DATA chunk still
            // complete the frame immediately, matching FFmpeg's packet order.
            // rv34 parser pts correction (FFmpeg attaches the parser to
            // rv30/rv40 streams; see rv34.rs).
            let codec_id = self.streams[stream_idx].params.codec_id.as_str().to_owned();
            let pts = vst
                .rv34
                .correct_pts(&codec_id, Some(timestamp), &data);
            let mut pkt = Packet::new(stream_idx as u32, TimeBase::MILLIS, data);
            if let Some(pts) = pts {
                pkt = pkt.with_pts(pts);
            }
            pkt = pkt.with_keyframe((flags & 2) != 0);
            vst.slices = 0;
            return Ok(Some(pkt));
        }

        Ok(None)
    }

    fn parse_audio_packet(
        &mut self,
        stream_idx: usize,
        len: usize,
        timestamp: i64,
        flags: u8,
    ) -> Result<Option<Packet>> {
        let ast = self.audio_states.get_mut(&stream_idx).unwrap();
        let time_base = self.streams[stream_idx].time_base;

        if ast.deint_id == DEINT_ID_GENR
            || ast.deint_id == DEINT_ID_INT4
            || ast.deint_id == DEINT_ID_SIPR
        {
            let sps = ast.sub_packet_size;
            let cfs = ast.coded_framesize;
            let h = ast.sub_packet_h;
            let w = ast.audio_framesize;
            let block_align = ast.block_align;

            if (flags & 2) != 0 {
                ast.sub_packet_cnt = 0;
                ast.partial = false;
            }
            if ast.sub_packet_cnt == 0 {
                ast.audiotimestamp = Some(timestamp);
                ast.partial = false;
            }
            let y = ast.sub_packet_cnt;

            match ast.deint_id {
                DEINT_ID_INT4 => {
                    let steps = if w > 0 && cfs > 0 { h / 2 } else { 0 };
                    for x in 0..steps {
                        let offset = x * 2 * w + y * cfs;
                        if offset + cfs <= ast.audio_buf.len() {
                            let ok = read_full(&mut self.io, &mut ast.audio_buf[offset..offset + cfs]);
                            ast.partial |= !ok;
                        }
                    }
                }
                DEINT_ID_GENR => {
                    let steps = if sps > 0 { w / sps } else { 0 };
                    for x in 0..steps {
                        let offset = sps * (h * x + ((h + 1) / 2) * (y & 1) + (y >> 1));
                        if offset + sps <= ast.audio_buf.len() {
                            let ok = read_full(&mut self.io, &mut ast.audio_buf[offset..offset + sps]);
                            ast.partial |= !ok;
                        }
                    }
                }
                DEINT_ID_SIPR => {
                    let offset = y * w;
                    if offset + w <= ast.audio_buf.len() {
                        let ok = read_full(&mut self.io, &mut ast.audio_buf[offset..offset + w]);
                        ast.partial |= !ok;
                    }
                }
                _ => {}
            }

            ast.sub_packet_cnt += 1;
            if ast.sub_packet_cnt < h {
                return Ok(None);
            }

            // Block is complete!
            if ast.deint_id == DEINT_ID_SIPR {
                rmsipr::rm_reorder_sipr_data(&mut ast.audio_buf, h, w);
            }

            ast.sub_packet_cnt = 0;
            if block_align == 0 {
                return Err(Error::invalid("zero block_align in audio interleaver"));
            }

            let num_pkts = (h * w) / block_align;
            // Frame duration in samples, from av_get_audio_frame_duration:
            // int4 (ra_288) 160, genr (cook) 1024, sipr per block_align
            // (20→160, 19→144, 29→288, 37→480). Rescaled into the stream's
            // time base so .rm (1/1000) and .ra (1/90000) both match ffprobe.
            let pkt_samples: i64 = match ast.deint_id {
                DEINT_ID_INT4 => 160,
                DEINT_ID_GENR => 1024,
                DEINT_ID_SIPR => match block_align {
                    20 => 160,
                    19 => 144,
                    29 => 288,
                    37 => 480,
                    _ => 160,
                },
                _ => 0,
            };
            let pkt_duration = if pkt_samples > 0 && ast.sample_rate > 0 {
                TimeBase::from_rate(ast.sample_rate).rescale(pkt_samples, time_base)
            } else {
                0
            };
            // ff_rm_retrieve_cache keeps the audiotimestamp as-is; the
            // negative first pts on sipr 8k5/5k0 files comes from the file's
            // chunk timestamps themselves.
            let initial_delay: i64 = 0;

            let base_ts = ast.audiotimestamp.unwrap_or(timestamp) + initial_delay;
            ast.blocks_emitted += 1;

            if ast.deint_id == DEINT_ID_SIPR {
                // FFmpeg attaches the sipr parser (AVSTREAM_PARSE_FULL_RAW,
                // rmdec.c) to sipr audio: each frame's pts is the chunk
                // timestamp only when the frame starts a DATA chunk
                // (ff_fetch_timestamp's window rule); frames inside a block
                // carry no fresh ts and land on the duration grid
                // (compute_pkt_fields: pts = prev + duration). The first
                // block is shifted back by the decoder primer (32 samples
                // for the 19-byte flavor, 48 for 37-byte, verified against
                // ffprobe on the FATE sipr files); later blocks whose chunk
                // ts disagrees with the carried grid re-anchor on it.
                let primer: i64 = match block_align {
                    19 => 32,
                    37 => 48,
                    _ => 0,
                };
                for i in 0..num_pkts {
                    let start = i * block_align;
                    let end = start + block_align;
                    if end > ast.audio_buf.len() {
                        continue;
                    }
                    let slice = &ast.audio_buf[start..end];
                    let pkt = Packet::new(stream_idx as u32, time_base, slice.to_vec());
                    let pts = if i == 0 {
                        if std::mem::take(&mut ast.restarted) {
                            None
                        } else if ast.blocks_emitted == 1 {
                            Some(base_ts - TimeBase::from_rate(ast.sample_rate).rescale(primer, time_base))
                        } else {
                            Some(base_ts)
                        }
                    } else {
                        None
                    };
                    let mut pkt = pkt
                        .with_duration(pkt_duration)
                        // compute_pkt_fields flags every frame of an
                        // intra-only codec (all RealMedia audio) key.
                        .with_keyframe(true)
                        .with_corrupt(ast.partial);
                    pkt.pts = pts;
                    self.packet_queue.push_back(pkt);
                }
            } else {
                for i in 0..num_pkts {
                    let start = i * block_align;
                    let end = start + block_align;
                    if end <= ast.audio_buf.len() {
                        let slice = &ast.audio_buf[start..end];
                        let pkt_pts = base_ts + (i as i64) * pkt_duration;
                        let mut pkt = Packet::new(stream_idx as u32, time_base, slice.to_vec());
                        pkt = pkt.with_pts(pkt_pts);
                        pkt = pkt.with_duration(pkt_duration);
                        pkt = pkt.with_keyframe(true);
                        pkt = pkt.with_corrupt(ast.partial);
                        self.packet_queue.push_back(pkt);
                    }
                }
            }

            // Pop the front frame; a pts-less frame takes compute_pkt_fields'
            // dts (see fill_pts).
            let mut pkt = self.packet_queue.pop_front().ok_or(Error::Eof)?;
            self.fill_pts(&mut pkt);
            Ok(Some(pkt))
        } else if ast.deint_id == DEINT_ID_VBRF || ast.deint_id == DEINT_ID_VBRS {
            let mut b2 = [0u8; 2];
            self.io.read_exact(&mut b2)?;
            let sub_pkts = ((u16::from_be_bytes(b2) & 0xF0) >> 4) as usize;
            if sub_pkts > 0 {
                let mut lengths = [0usize; 16];
                for i in 0..sub_pkts.min(16) {
                    self.io.read_exact(&mut b2)?;
                    lengths[i] = u16::from_be_bytes(b2) as usize;
                }
                for i in 0..sub_pkts.min(16) {
                    let sub_len = lengths[i];
                    if sub_len > MAX_BUFFER_SIZE {
                        return Err(Error::invalid("VBR audio subpacket too large"));
                    }
                    let mut buf = vec![0u8; sub_len];
                    self.io.read_exact(&mut buf)?;
                    let mut pkt = Packet::new(stream_idx as u32, time_base, buf).with_keyframe(true);
                    if i == 0 {
                        pkt = pkt.with_pts(timestamp);
                    }
                    self.packet_queue.push_back(pkt);
                }
                Ok(self.packet_queue.pop_front())
            } else {
                Ok(None)
            }
        } else {
            // DEINT_ID_INT0 or unknown: direct packet
            if len > MAX_BUFFER_SIZE {
                return Err(Error::invalid("audio packet too large"));
            }
            let mut buf = vec![0u8; len];
            let whole = read_full(&mut self.io, &mut buf);
            if self.streams[stream_idx].params.codec_id.as_str() == "ac3" {
                for chunk in buf.chunks_exact_mut(2) {
                    chunk.swap(0, 1);
                }
            }
            let mut pkt = Packet::new(stream_idx as u32, time_base, buf);
            pkt = pkt.with_pts(timestamp);
            pkt = pkt.with_keyframe(true);
            pkt = pkt.with_corrupt(!whole);
            Ok(Some(pkt))
        }
    }

    /// compute_pkt_fields for an audio frame without its own timestamp:
    /// the first after a seek takes the landing dts (avpriv_update_cur_dts),
    /// the others continue the duration grid from the previous frame.
    fn fill_pts(&mut self, pkt: &mut Packet) {
        let landed = self.cur_dts.remove(&pkt.stream_index);
        if pkt.pts.is_none() {
            pkt.pts = landed.or_else(|| {
                let prev = self.last_audio_pts.get(&pkt.stream_index)?;
                Some(prev.saturating_add(pkt.duration.unwrap_or(0)))
            });
        }
        if let Some(pts) = pkt.pts {
            self.last_audio_pts.insert(pkt.stream_index, pts);
        }
    }

    /// rm_read_dts: from `*ppos`, the timestamp of the first key packet of
    /// `stream` (flag 2 and, for video, slice sequence 1), `*ppos` moved to
    /// that packet; `None` (AV_NOPTS_VALUE) when the scan ends first. Every
    /// key packet met is indexed for its stream.
    fn read_dts(&mut self, stream: usize, ppos: &mut i64) -> Result<Option<i64>> {
        if self.old_format {
            return Ok(None);
        }
        let Ok(pos) = u64::try_from(*ppos) else { return Ok(None) };
        if self.io.seek(SeekFrom::Start(pos)).is_err() {
            return Ok(None);
        }
        self.remaining_len = 0;
        // rm_sync starts every call from a fresh state.
        self.sync_state = 0xFFFF_FFFF;
        loop {
            let (s, len, dts, flags) = match self.sync_next_packet() {
                Ok(Some(found)) => found,
                Err(e) if demux_seek_core::is_exhausted(&e) => return Err(e),
                _ => return Ok(None),
            };
            self.allowance.spend(1, 0)?;
            let at = self.sync_pos as i64;
            let mut len = len as i64;
            let mut seq = 1;
            if self.streams[s].params.media_type == MediaType::Video {
                let mut b = [0u8; 1];
                if self.io.read_exact(&mut b).is_err() {
                    return Ok(None);
                }
                len -= 1;
                if b[0] & 0x40 == 0 {
                    if self.io.read_exact(&mut b).is_err() {
                        return Ok(None);
                    }
                    len -= 1;
                    seq = b[0];
                }
            }
            if flags & 2 != 0 && seq & 0x7F == 1 {
                self.seek_index[s].add(at, dts, 0, 0, true);
                if s == stream {
                    *ppos = at;
                    return Ok(Some(dts));
                }
            }
            if self.io.seek(SeekFrom::Current(len)).is_err() {
                return Ok(None);
            }
        }
    }
}

impl Demuxer for RmDemuxer {
    fn format_name(&self) -> &str {
        "rm"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn metadata(&self) -> &[(String, String)] {
        &self.metadata
    }

    fn duration_micros(&self) -> Option<i64> {
        self.duration_micros
    }

    fn next_packet(&mut self) -> Result<Packet> {
        loop {
            if let Some(mut pkt) = self.packet_queue.pop_front() {
                self.fill_pts(&mut pkt);
                return Ok(pkt);
            }

            if self.old_format {
            let ast = match self.audio_states.get_mut(&0) {
                Some(a) => a,
                None => return Err(Error::Eof),
            };

            let len = if ast.audio_framesize == 0 {
                RAW_PACKET_SIZE
            } else {
                ast.coded_framesize * ast.sub_packet_h / 2
            };

            let cur_pos = self.io.stream_position()?;
            let file_end = self.io.seek(SeekFrom::End(0))?;
            self.io.seek(SeekFrom::Start(cur_pos))?;
            if cur_pos >= file_end {
                return Err(Error::Eof);
            }

            let read_len = len.min((file_end - cur_pos) as usize);
            if read_len == 0 {
                return Err(Error::Eof);
            }

            let tb = self.streams[0].time_base;
            if self.audio_states.get(&0).is_some_and(|a| a.deint_id == DEINT_ID_INT4) {
                // rm_read_packet: the old .ra format has one interleaved
                // block per RAW chunk; only the first chunk of each block
                // group is flagged KEY (seq), so pass 2 once and 0 after —
                // the int4 interlever needs the counter to advance.
                let first_sub = self.audio_states.get(&0).is_some_and(|a| a.sub_packet_cnt == 0);
                let res = self.parse_audio_packet(0, read_len, 0, u8::from(first_sub))?;
                if let Some(mut pkt) = res {
                    // 28_8 frames are 160 samples; pts/duration in the
                    // 1/90000 old-format time base, like compute_pkt_fields
                    // (frame_size 160 @ 8 kHz → 20 ms → 1800 ticks).
                    let sample_rate = self.audio_states.get(&0).map_or(8000, |a| a.sample_rate);
                    let blocks = self.audio_states.get(&0).map_or(0, |a| a.blocks_emitted);
                    let dur_ticks = tb.rescale(160, TimeBase::from_rate(sample_rate));
                    let pts = (blocks.saturating_sub(1) as i64)
                        .checked_mul(dur_ticks)
                        .unwrap_or(0)
                        + pkt.pts.unwrap_or(0);
                    pkt.pts = Some(pts);
                    pkt.duration = Some(dur_ticks);
                    return Ok(pkt);
                }
                // Block still incomplete: rm_read_packet loops for the next
                // RAW chunk (`if (res) continue;`) instead of emitting a
                // raw packet on top of the bytes the interleaver consumed.
                // Iterative: the caller (next_packet loop) re-enters.
                continue;
            }

            let mut buf = vec![0u8; read_len];
            self.io.read_exact(&mut buf)?;
            let mut pkt = Packet::new(0, tb, buf);
            pkt = pkt.with_pts(0);
            pkt = pkt.with_keyframe(true);
            return Ok(pkt);
            }

        let (stream_idx, len, timestamp, flags) = match self.sync_next_packet()? {
            Some(p) => p,
            None => return Err(Error::Eof),
        };

        let media_type = self.streams[stream_idx].params.media_type;
        match media_type {
            MediaType::Video => {
                self.current_stream = self
                    .stream_id_to_index
                    .iter()
                    .find(|(_, i)| **i == stream_idx)
                    .map_or(0, |(k, _)| *k as u16);
                if let Some(pkt) = self.assemble_video_frame(stream_idx, len, timestamp, flags)? {
                    return Ok(pkt);
                }
                // Incomplete frame: loop for the next chunk.
                continue;
            }
            MediaType::Audio => {
                if let Some(pkt) = self.parse_audio_packet(stream_idx, len, timestamp, flags)? {
                    // The stream's next frame no longer takes the seek's dts.
                    self.cur_dts.remove(&pkt.stream_index);
                    return Ok(pkt);
                }
                continue;
            }
            _ => {
                if len > MAX_BUFFER_SIZE {
                    return Err(Error::invalid("data packet exceeds limits"));
                }
                let mut buf = vec![0u8; len];
                // rmdec.c: av_get_packet on a truncated final packet fails
                // with AVERROR_INVALIDDATA, ending the stream.
                if self.io.read_exact(&mut buf).is_err() {
                    return Err(Error::Eof);
                }
                let mut pkt = Packet::new(stream_idx as u32, self.streams[stream_idx].time_base, buf);
                pkt = pkt.with_pts(timestamp);
                // compute_pkt_fields flags every data packet key.
                pkt = pkt.with_keyframe(true);
                return Ok(pkt);
            }
        }
        }
    }

    /// rm_read_seek: ff_seek_frame_binary over rm_read_dts, bounded by the
    /// INDX entries and the key packets earlier searches met. Lands on the
    /// last key packet of `stream_index` at or before `pts`. The search
    /// reads within the seek's allowance; a seek that fails, its
    /// reposition to the landing included, leaves reading where it was.
    fn seek_to(&mut self, stream_index: u32, pts: i64) -> Result<i64> {
        if self.old_format {
            // rm_read_dts returns AV_NOPTS_VALUE for RealAudio (.ra) files,
            // so FFmpeg's search, and its seek, fail.
            return Err(Error::unsupported("rm: RealAudio (.ra) files have no timestamps to seek by"));
        }
        let stream = stream_index as usize;
        if stream >= self.streams.len() {
            return Err(Error::invalid("rm: no such stream to seek"));
        }
        let resume = (self.io.stream_position()?, self.remaining_len, self.sync_state, self.sync_pos);
        let bounds = self.seek_index[stream].bounds(pts);
        let file_size = self.io.seek(SeekFrom::End(0))? as i64;
        let data_offset = self.data_offset as i64;
        self.allowance.start();
        let found = gen_search(pts, bounds, data_offset, file_size, &mut |pos, _| self.read_dts(stream, pos));
        let landed = match self.allowance.finish(found) {
            Ok(Some((pos, ts))) if pos >= 0 => self.io.seek(SeekFrom::Start(pos as u64)).map(|_| Some(ts)).map_err(Error::from),
            other => other.map(|_| None),
        };
        let ts = match landed {
            Ok(Some(ts)) => ts,
            failed => {
                (self.remaining_len, self.sync_state, self.sync_pos) = (resume.1, resume.2, resume.3);
                self.io.seek(SeekFrom::Start(resume.0))?;
                return Err(failed.err().unwrap_or_else(|| Error::invalid("rm: no key frame to seek to")));
            }
        };

        // ff_read_frame_flush and rm->audio_pkt_cnt = 0: queued frames and
        // the parsers go. FFmpeg keeps the audio deinterleaver and the
        // video slice assembly across the seek; they restart here, so the
        // first frames after it do not depend on what was read before.
        self.packet_queue.clear();
        self.remaining_len = 0;
        self.sync_state = 0xFFFF_FFFF;
        self.last_audio_pts.clear();
        for ast in self.audio_states.values_mut() {
            ast.sub_packet_cnt = 0;
            ast.audiotimestamp = None;
            ast.restarted = true;
        }
        for vst in self.video_states.values_mut() {
            vst.slices = 0;
            vst.cur_slice = 0;
            vst.videobufpos = 0;
            vst.videobuf.clear();
            vst.rv34 = Rv34ParserState::default();
        }
        // avpriv_update_cur_dts: every stream (all in 1/1000) continues
        // from the landing time.
        self.cur_dts = self.audio_states.keys().map(|&s| (s as u32, ts)).collect();
        Ok(ts)
    }
}
