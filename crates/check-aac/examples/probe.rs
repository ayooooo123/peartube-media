use oxideav_core::{Frame, MediaType, SampleFormat};

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
    out.params.sample_format = Some(SampleFormat::F32);
    if let Some(Frame::Audio(a)) = out.frames.first() {
        let bytes = a.data[0].len();
        if let Some(ch) = (bytes / 4).checked_div(a.samples.max(1) as usize) {
            if (1..=8).contains(&ch) {
                out.params.channels = Some(ch as u16);
            }
        }
    }
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
