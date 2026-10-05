fn main() {
    let ctx = codecs::context();
    let path = std::env::args().nth(1).unwrap();
    let head = std::fs::read(&path).unwrap();
    let buf = &head[..head.len().min(256 * 1024)];
    let probe = oxideav_core::ProbeData { buf, ext: Some("mkv") };
    let cands = ctx.containers.probe_candidates(&probe);
    println!("candidates: {:?}", cands.iter().map(|c| (c.name, c.score)).collect::<Vec<_>>());
    let mut d = ctx
        .containers
        .open_demuxer("matroska", Box::new(std::fs::File::open(&path).unwrap()), &ctx.codecs)
        .unwrap();
    for s in d.streams() {
        println!(
            "stream {}: {:?} codec={} tb={}/{} rate={:?} ch={:?} fmt={:?} pix={:?} {}x{}",
            s.index, s.params.media_type, s.params.codec_id.as_str(),
            s.time_base.num(), s.time_base.den(),
            s.params.sample_rate, s.params.channels, s.params.sample_format,
            s.params.pixel_format, s.params.width.unwrap_or(0), s.params.height.unwrap_or(0)
        );
    }
    use oxideav_core::Demuxer;
    let mut packets = 0u64;
    let mut dec_a = None;
    let mut dec_v = None;
    for s in d.streams().to_vec() {
        if let Ok(dec) = ctx.codecs.first_decoder(&s.params) {
            if s.params.media_type == oxideav_core::MediaType::Audio { dec_a = Some((s.index, dec)); }
            if s.params.media_type == oxideav_core::MediaType::Video { dec_v = Some((s.index, dec)); }
        } else {
            println!("NO DECODER for stream {} codec {}", s.index, s.params.codec_id.as_str());
        }
    }
    let mut aframes = 0u64;
    let mut a_samples = 0u64;
    let mut vframes = 0u64;
    loop {
        match d.next_packet() {
            Ok(p) => {
                packets += 1;
                if let Some((idx, dec)) = dec_a.as_mut() {
                    if p.stream_index == *idx {
                        let _ = dec.send_packet(&p);
                        while let Ok(f) = dec.receive_frame() {
                            aframes += 1;
                            if let oxideav_core::Frame::Audio(a) = f {
                                a_samples += a.samples as u64;
                                if aframes == 1 { println!("first aframe: samples={} planes={} len0={} pts={:?}", a.samples, a.data.len(), a.data.first().map(|v| v.len()), a.pts); }
                            }
                        }
                    }
                }
                if let Some((idx, dec)) = dec_v.as_mut() {
                    if p.stream_index == *idx {
                        let _ = dec.send_packet(&p);
                        while let Ok(f) = dec.receive_frame() {
                            if let oxideav_core::Frame::Video(v) = f {
                                vframes += 1;
                                if vframes == 1 {
                                    let planes = v.image_planes();
                                    println!("first vframe: planes={} strides={:?} lens={:?}", planes.len(), planes.iter().map(|p| p.stride).collect::<Vec<_>>(), planes.iter().map(|p| p.data.len()).collect::<Vec<_>>());
                                }
                            }
                        }
                    }
                }
            }
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => { println!("demux err: {e}"); break; }
        }
    }
    println!("packets={packets} aframes={aframes} asamples={a_samples} vframes={vframes}");
}
