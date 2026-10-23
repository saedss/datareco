//! On-disk timestamps to seconds since 1970-01-01 UTC.

/// Seconds between 1601-01-01 (the Windows FILETIME epoch) and 1970-01-01.
const FILETIME_UNIX_OFFSET: i64 = 11_644_473_600;

/// FAT date and time. `ten_ms` is the creation time's 10 ms count (0–199);
/// pass 0 where there is none. Invalid or unset dates give `None`.
pub(crate) fn fat(date: u16, time: u16, ten_ms: u8) -> Option<i64> {
    if date == 0 {
        return None;
    }
    let year = 1980 + i64::from(date >> 9);
    let month = i64::from((date >> 5) & 0x0F);
    let day = i64::from(date & 0x1F);
    let hour = i64::from(time >> 11);
    let minute = i64::from((time >> 5) & 0x3F);
    let second = i64::from(time & 0x1F) * 2 + i64::from(ten_ms) / 100;
    let valid = (1..=12).contains(&month)
        && (1..=31).contains(&day)
        && hour < 24
        && minute < 60
        && second < 62;
    valid.then(|| days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
}

/// An exFAT timestamp: a FAT date and time packed in 32 bits, plus a 10 ms
/// count and a UTC offset (in 15-minute steps, valid when bit 7 is set).
pub(crate) fn exfat(timestamp: u32, ten_ms: u8, utc_offset: u8) -> Option<i64> {
    let [t0, t1, d0, d1] = timestamp.to_le_bytes();
    let local = fat(
        u16::from_le_bytes([d0, d1]),
        u16::from_le_bytes([t0, t1]),
        ten_ms,
    )?;
    if utc_offset & 0x80 == 0 {
        return Some(local);
    }
    // The low 7 bits are a signed count of 15 minutes.
    let steps = i64::from(utc_offset & 0x7F);
    let steps = if steps >= 64 { steps - 128 } else { steps };
    Some(local - steps * 15 * 60)
}

/// Windows FILETIME: 100 ns intervals since 1601-01-01 UTC. 0 means unset.
pub(crate) fn filetime(value: u64) -> Option<i64> {
    if value == 0 {
        return None;
    }
    i64::try_from(value / 10_000_000)
        .ok()
        .map(|seconds| seconds - FILETIME_UNIX_OFFSET)
}

/// Days from 1970-01-01 to a date (proleptic Gregorian). Howard Hinnant's algorithm.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2024-01-15 10:30:00 UTC.
    const STAMP: i64 = 1_705_314_600;
    const DATE: u16 = (44 << 9) | (1 << 5) | 15;
    const TIME: u16 = (10 << 11) | (30 << 5);

    #[test]
    fn fat_dates() {
        assert_eq!(fat(DATE, TIME, 0), Some(STAMP));
        assert_eq!(fat(DATE, TIME, 150), Some(STAMP + 1));
        assert_eq!(fat(0, TIME, 0), None);
        assert_eq!(fat((44 << 9) | (13 << 5) | 1, 0, 0), None, "month 13");
        assert_eq!(
            fat((20 << 9) | (2 << 5) | 29, 0, 0),
            Some(951_782_400),
            "2000-02-29"
        );
    }

    #[test]
    fn exfat_utc_offsets() {
        let packed = (u32::from(DATE) << 16) | u32::from(TIME);
        assert_eq!(exfat(packed, 0, 0), Some(STAMP), "no offset recorded");
        assert_eq!(exfat(packed, 0, 0x80), Some(STAMP), "UTC");
        // Local time 10:30 at UTC+2 (8 steps) is 08:30 UTC.
        assert_eq!(exfat(packed, 0, 0x80 | 8), Some(STAMP - 7200));
        // UTC-5 (-20 steps, stored as 128 - 20 = 108).
        assert_eq!(exfat(packed, 0, 0x80 | 108), Some(STAMP + 5 * 3600));
    }

    #[test]
    fn filetimes() {
        assert_eq!(filetime(133_497_882_000_000_000), Some(STAMP));
        assert_eq!(filetime(0), None);
    }
}
