use oxideav_core::MediaType;

fn main() {
    let path = std::env::args().nth(1).unwrap();
    let fate_rel = if path.starts_with("aac/") { path.clone() } else { format!("aac/{path}") };
    let full = refcheck::fate(&fate_rel);
    let mut out = refcheck::decode(
        &full,
        &[
            oxideav_aac::__oxideav_entry,
            oxideav_mov::registry::register,
            oxideav_mp4::__oxideav_entry,
            oxideav_mpegts::__oxideav_entry,
        ],
        MediaType::Audio,
        0,
    );
    let ch = out.params.channels.unwrap_or(1).max(1) as usize;
    let audio = refcheck::interleaved_f32(&out);
    println!(
        "frames={} rate={:?} ch={:?} samples={}",
        out.frames.len(),
        out.params.sample_rate,
        out.params.channels,
        audio.len() / ch
    );
}
