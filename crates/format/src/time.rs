//! The time, as FAT directory entries store it, and new serial numbers.

use std::time::{SystemTime, UNIX_EPOCH};

fn now() -> (u64, u32) {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (elapsed.as_secs(), elapsed.subsec_nanos())
}

/// A volume serial number, from the time as DOS made them: the date and
/// time words, each added to the other half of the clock.
pub(crate) fn serial() -> u32 {
    let (seconds, nanos) = now();
    let (date, time) = dos_date_time(seconds);
    let low = u32::from(date).wrapping_add(nanos & 0xFFFF);
    let high = u32::from(time).wrapping_add(nanos >> 16);
    (high << 16) | (low & 0xFFFF)
}

/// FAT's date and time words for now (UTC).
pub(crate) fn dos_now() -> (u16, u16) {
    dos_date_time(now().0)
}

/// Seconds since 1970 → (date, time) words: year since 1980, month and day;
/// hours, minutes and seconds halved. Dates before 1980 become 1980-01-01.
pub(crate) fn dos_date_time(seconds: u64) -> (u16, u16) {
    let days = i64::try_from(seconds / 86_400).unwrap_or(0);
    let rest = seconds % 86_400;
    let (year, month, day) = civil(days);
    if year < 1980 {
        return ((1 << 5) | 1, 0);
    }
    let word = |v: i64| u16::try_from(v).unwrap_or(0);
    let date = (word(year.min(2107) - 1980) << 9) | (word(month) << 5) | word(day);
    let packed = ((rest / 3600) << 11) | ((rest / 60 % 60) << 5) | (rest % 60 / 2);
    let time = u16::try_from(packed).unwrap_or(0);
    (date, time)
}

/// Days since 1970 → (year, month, day), Howard Hinnant's algorithm.
fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dos_dates() {
        // 2001-09-09 01:46:40.
        let (date, time) = dos_date_time(1_000_000_000);
        assert_eq!(date >> 9, 21);
        assert_eq!((date >> 5) & 0xF, 9);
        assert_eq!(date & 0x1F, 9);
        assert_eq!(time >> 11, 1);
        assert_eq!((time >> 5) & 0x3F, 46);
        assert_eq!(time & 0x1F, 20);
        assert_eq!(dos_date_time(0), (0x21, 0), "before 1980");
    }
}
