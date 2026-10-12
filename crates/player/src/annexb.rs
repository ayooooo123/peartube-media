//! AVCC/HVCC (length-prefixed) to Annex B conversion. Pure math, host
//! testable; the framing decision belongs to the caller (the Android video
//! sink reads it once from the stream extradata).

/// Converts one length-prefixed packet to Annex B start-code framing.
/// `nal_length_size` is the byte width of the length prefixes (1–4; 0 is
/// treated as 4). Does NOT sniff the input: a valid 4-byte NAL length of
/// 256–511 starts with `00 00 01`, which a start-code sniffer would
/// misread as Annex B.
pub fn convert_packet_to_annex_b(data: &[u8], nal_length_size: usize) -> Vec<u8> {
    let len_size = if nal_length_size == 0 {
        4
    } else {
        nal_length_size
    };
    let mut out = Vec::with_capacity(data.len() + 32);
    let mut offset = 0;

    while offset + len_size <= data.len() {
        let nal_len = match len_size {
            4 => u32::from_be_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ]) as usize,
            2 => u16::from_be_bytes([data[offset], data[offset + 1]]) as usize,
            1 => data[offset] as usize,
            3 => {
                let b0 = data[offset] as usize;
                let b1 = data[offset + 1] as usize;
                let b2 = data[offset + 2] as usize;
                (b0 << 16) | (b1 << 8) | b2
            }
            _ => 0,
        };
        offset += len_size;
        if offset + nal_len > data.len() {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&data[offset..]);
            break;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&data[offset..offset + nal_len]);
        offset += nal_len;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::convert_packet_to_annex_b;

    /// A 4-byte AVCC packet whose NAL length is 300 (0x012C): the length
    /// prefix itself starts with `00 00 01`, which a start-code sniffer
    /// would misread as Annex B. The converter must treat it as a length.
    #[test]
    fn nal_length_300_is_not_mistaken_for_annex_b() {
        // length prefix 00 00 01 2C = 300, then 300 bytes of NAL payload.
        let mut pkt = vec![0x00, 0x00, 0x01, 0x2C];
        pkt.extend(std::iter::repeat_n(0xABu8, 300));
        let out = convert_packet_to_annex_b(&pkt, 4);
        // The payload must be preserved verbatim behind a 4-byte start code,
        // with no 3-byte start code spliced in.
        assert_eq!(out.len(), 4 + 300);
        assert_eq!(&out[..4], &[0, 0, 0, 1]);
        assert_eq!(&out[4..], &pkt[4..]);
        assert_eq!(&out[4..7], &[0xAB, 0xAB, 0xAB]);
    }

    /// A NAL length of exactly 256 (0x0100) — also starts with 00 00.
    #[test]
    fn nal_length_256_round_trips() {
        let mut pkt = vec![0x00, 0x00, 0x01, 0x00];
        pkt.extend(std::iter::repeat_n(0x65u8, 256)); // 256-byte NAL
        let out = convert_packet_to_annex_b(&pkt, 4);
        assert_eq!(out.len(), 4 + 256);
        assert_eq!(&out[..4], &[0, 0, 0, 1]);
        assert_eq!(&out[4..], &pkt[4..]);
    }

    /// Multi-NAL packet with 2-byte lengths, incl. one > 255.
    #[test]
    fn multi_nal_two_byte_lengths() {
        let mut pkt = vec![0x01, 0x00]; // NAL 1: 256 bytes
        pkt.extend(std::iter::repeat_n(0x11u8, 256));
        pkt.extend_from_slice(&[0x00, 0x05]); // NAL 2: 5 bytes
        pkt.extend_from_slice(&[0x67, 0x64, 0x00, 0x1F, 0xAC]);
        let out = convert_packet_to_annex_b(&pkt, 2);
        assert_eq!(out.len(), 4 + 256 + 4 + 5);
        assert_eq!(&out[..4], &[0, 0, 0, 1]);
        assert_eq!(&out[4..260], &vec![0x11u8; 256][..]);
        assert_eq!(&out[260..264], &[0, 0, 0, 1]);
        assert_eq!(&out[264..], &[0x67, 0x64, 0x00, 0x1F, 0xAC]);
    }

    /// Truncated final NAL: the tail is emitted behind a start code (the
    /// old behavior — untrusted input must not panic or lose bytes).
    #[test]
    fn truncated_tail_is_emitted() {
        let pkt = [0x00, 0x00, 0x01, 0x04, 0xAA, 0xBB]; // claims 4, has 2
        let out = convert_packet_to_annex_b(&pkt, 4);
        assert_eq!(&out[..4], &[0, 0, 0, 1]);
        assert_eq!(&out[4..], &[0xAA, 0xBB]);
    }
}
