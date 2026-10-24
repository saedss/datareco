//! NVMe: the SMART / Health Information log page (log 02h, NVMe 1.4
//! section 5.14.1.2). Counters are little-endian, 16 bytes wide; data units
//! are thousands of 512-byte units.

use crate::report::{HealthReport, Level, NvmeHealth, count, finding};

const LOG_BYTES: usize = 512;

fn u128_at(log: &[u8], at: usize) -> u64 {
    let mut bytes = [0u8; 16];
    if let Some(slice) = log.get(at..at + 16) {
        bytes.copy_from_slice(slice);
    }
    u64::try_from(u128::from_le_bytes(bytes)).unwrap_or(u64::MAX)
}

fn u16_at(log: &[u8], at: usize) -> u16 {
    log.get(at..at + 2)
        .map_or(0, |b| u16::from_le_bytes([b[0], b[1]]))
}

fn u32_at(log: &[u8], at: usize) -> u32 {
    log.get(at..at + 4)
        .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Kelvin to °C; 0 means the drive doesn't report it.
fn celsius(kelvin: u16) -> Option<i64> {
    (kelvin != 0).then(|| i64::from(kelvin) - 273)
}

pub(crate) fn parse(log: &[u8]) -> NvmeHealth {
    let byte = |at: usize| log.get(at).copied().unwrap_or(0);
    let data_units = |at: usize| u128_at(log, at).saturating_mul(512_000);
    NvmeHealth {
        critical_warning: byte(0),
        temperature: celsius(u16_at(log, 1)),
        available_spare: byte(3),
        spare_threshold: byte(4),
        percentage_used: byte(5),
        bytes_read: data_units(32),
        bytes_written: data_units(48),
        host_reads: u128_at(log, 64),
        host_writes: u128_at(log, 80),
        busy_minutes: u128_at(log, 96),
        power_cycles: u128_at(log, 112),
        power_on_hours: u128_at(log, 128),
        unsafe_shutdowns: u128_at(log, 144),
        media_errors: u128_at(log, 160),
        error_log_entries: u128_at(log, 176),
        warning_temperature_minutes: u64::from(u32_at(log, 192)),
        critical_temperature_minutes: u64::from(u32_at(log, 196)),
    }
}

pub(crate) fn assess(log: &[u8]) -> HealthReport {
    let mut report = HealthReport::empty(Some("NVMe health log".to_owned()));
    if log.len() < LOG_BYTES {
        report.summary = "The drive's NVMe health log is too short to read.".to_owned();
        return report;
    }
    let health = parse(log);
    let warning = health.critical_warning;
    let bit = |n: u8| warning & (1 << n) != 0;
    let mut add = |level, message: String| report.findings.push(finding(level, message));

    if bit(0) {
        add(
            Level::Bad,
            format!(
                "Its spare space has run low ({}% left; it warns below {}%).",
                health.available_spare, health.spare_threshold
            ),
        );
    }
    if bit(2) {
        add(
            Level::Bad,
            "Its reliability has dropped because of media or internal errors.".to_owned(),
        );
    }
    if bit(3) {
        add(
            Level::Bad,
            "It has switched itself to read-only to protect the data on it.".to_owned(),
        );
    }
    if bit(1) {
        add(
            Level::Caution,
            "It reports a temperature warning: too hot, or too cold.".to_owned(),
        );
    }
    if bit(4) {
        add(
            Level::Caution,
            "Its volatile memory backup has failed.".to_owned(),
        );
    }
    if bit(5) {
        add(
            Level::Caution,
            "Its persistent memory region has become read-only.".to_owned(),
        );
    }
    if health.media_errors > 0 {
        add(
            Level::Caution,
            format!(
                "It found data it couldn't correct {} (media and data integrity errors).",
                count(health.media_errors, "time", "times")
            ),
        );
    }
    if health.percentage_used >= 100 {
        add(
            Level::Caution,
            format!(
                "It has used {}% of its rated life: it's past the writes it was made for.",
                health.percentage_used
            ),
        );
    }
    if let Some(t) = health.temperature
        && t >= 70
        && !bit(1)
    {
        add(Level::Caution, format!("It's running hot ({t} °C)."));
    }

    report.temperature = health.temperature;
    report.power_on_hours = Some(health.power_on_hours);
    report.power_cycles = Some(health.power_cycles);
    report.life_used = Some(health.percentage_used);
    report.bytes_read = Some(health.bytes_read);
    report.bytes_written = Some(health.bytes_written);
    report.solid_state = Some(true);
    report.nvme = Some(health);
    report.conclude()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::Verdict;

    fn log(set: &[(usize, &[u8])]) -> Vec<u8> {
        let mut log = vec![0u8; 512];
        for (at, bytes) in set {
            log[*at..*at + bytes.len()].copy_from_slice(bytes);
        }
        log
    }

    #[test]
    fn reads_a_healthy_log() {
        let log = log(&[
            (1, &313u16.to_le_bytes()),     // 40 °C
            (3, &[100, 10, 3]),             // spare, threshold, used
            (32, &2_000u64.to_le_bytes()),  // data units read
            (48, &1_000u64.to_le_bytes()),  // data units written
            (112, &55u64.to_le_bytes()),    // power cycles
            (128, &1_234u64.to_le_bytes()), // power-on hours
            (144, &7u64.to_le_bytes()),     // unsafe shutdowns
        ]);
        let report = assess(&log);
        assert_eq!(report.verdict, Verdict::Good, "{:#?}", report.findings);
        assert_eq!(report.temperature, Some(40));
        assert_eq!(report.life_used, Some(3));
        assert_eq!(report.bytes_written, Some(512_000_000));
        assert_eq!(report.bytes_read, Some(1_024_000_000));
        assert_eq!(report.power_on_hours, Some(1_234));
        assert_eq!(report.power_cycles, Some(55));
        assert_eq!(report.nvme.as_ref().unwrap().unsafe_shutdowns, 7);
    }

    #[test]
    fn critical_warnings_and_media_errors() {
        let report = assess(&log(&[(0, &[0b1001]), (3, &[5, 10, 40])]));
        assert_eq!(report.verdict, Verdict::Bad);
        assert!(report.findings[0].message.contains("5% left"));
        assert!(report.findings[1].message.contains("read-only"));

        let report = assess(&log(&[(160, &3u64.to_le_bytes()), (3, &[100, 10, 120])]));
        assert_eq!(report.verdict, Verdict::Caution);
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.message.contains("3 times"))
        );
        assert!(report.findings.iter().any(|f| f.message.contains("120%")));
    }

    #[test]
    fn short_logs_are_unknown() {
        let report = assess(&[0u8; 100]);
        assert_eq!(report.verdict, Verdict::Unknown);
        assert!(report.summary.contains("too short"));
    }

    #[test]
    fn huge_counters_saturate() {
        let report = assess(&log(&[(48, &[0xFF; 16])]));
        assert_eq!(report.bytes_written, Some(u64::MAX));
    }
}
