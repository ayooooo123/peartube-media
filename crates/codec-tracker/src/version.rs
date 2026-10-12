//! OpenMPT version numbers and Schism Tracker dates, ported from
//! libopenmpt 0.8.9 `common/version.h` (`MPT_V`) and `soundlib/ITTools.h`.
//!
//! Copyright (c) 2004-2026, OpenMPT Project Developers and Contributors;
//! Copyright (c) 1997-2003, Olivier Lapicque. BSD-3-Clause (see LICENSE).

/// `MPT_V("a.b.c.d")`: each component is two hex digits.
pub const fn mpt_v(s: &str) -> u32 {
    let b = s.as_bytes();
    let mut parts = [0u32; 4];
    let mut part = 0;
    let mut i = 0;
    while i < b.len() && part < 4 {
        let c = b[i];
        if c == b'.' {
            part += 1;
        } else {
            let d = match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                b'A'..=b'F' => c - b'A' + 10,
                _ => 0,
            } as u32;
            parts[part] = parts[part] * 16 + d;
        }
        i += 1;
    }
    (parts[0] << 24) | (parts[1] << 16) | (parts[2] << 8) | parts[3]
}

/// `SchismVersionFromDate<y, m, d>::date`.
pub const fn schism_date(y: i32, m: i32, d: i32) -> i32 {
    let mm = (m + 9) % 12;
    let yy = y - mm / 10;
    yy * 365 + yy / 4 - yy / 100 + yy / 400 + (mm * 306 + 5) / 10 + (d - 1)
}

/// `SchismTrackerEpoch`.
pub const fn schism_epoch_date() -> i32 {
    schism_date(2009, 10, 31)
}
