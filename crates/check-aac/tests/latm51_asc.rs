use oxideav_core::RuntimeContext;
use std::fs::File;
use std::io::Read;

#[test]
fn latm51_asc_dump() {
    let path = refcheck::fate("aac/latm_stereo_to_51.ts");
    let mut ctx = RuntimeContext::new();
    oxideav_aac::__oxideav_entry(&mut ctx);
    oxideav_mpegts::__oxideav_entry(&mut ctx);
    let mut head = vec![0u8; 256 * 1024];
    let _n = File::open(&path).and_then(|mut f| f.read(&mut head)).unwrap();
    let file2 = File::open(&path).unwrap();
    let mut demuxer = ctx.containers.open_demuxer("mpegts", Box::new(file2), &ctx.codecs).unwrap();
    let pkt = loop {
        match demuxer.next_packet() {
            Ok(p) => if p.stream_index == 0 && !p.data.is_empty() { break p; },
            Err(_) => panic!("no audio packet"),
        }
    };
    // Parse the LOAS element directly from the packet bytes.
    let mut walker = oxideav_aac::latm::AudioSyncStream::new(&pkt.data);
    if let Some(Ok(frame)) = walker.next() {
        for layer in &frame.element.config.layers {
            let asc = &layer.effective_asc;
            println!(
                "stream {}: aot={} chcfg={} pce={}",
                layer.stream_id,
                asc.aot,
                asc.channel_configuration,
                asc.ga_body.pce.as_ref().map(|p| format!(
                    "front={} side={} back={} lfe={}",
                    p.front_elements.len(), p.side_elements.len(),
                    p.back_elements.len(), p.lfe_element_tag_selects.len()
                )).unwrap_or_else(|| "none".into())
            );
        }
    }
}
