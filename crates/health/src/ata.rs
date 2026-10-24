//! ATA S.M.A.R.T.: the attribute and threshold pages, and IDENTIFY DEVICE.
//!
//! Both S.M.A.R.T. pages hold 30 entries of 12 bytes from byte 2; byte 511
//! makes the page's bytes add up to zero. Attribute entries are: id, flags
//! (two bytes), current value, worst value, a six-byte raw counter and a
//! reserved byte. Threshold entries are: id and threshold. What each
//! attribute counts isn't standard; the names below are the common ones.

use std::collections::HashMap;

use datareco_io::HealthPath;

use crate::report::{Attribute, AttributeState, HealthReport, Level, count, finding, path_name};

const ENTRIES: usize = 30;
const ENTRY_BYTES: usize = 12;
const PAGE: usize = 512;

pub(crate) fn assess(
    page: &[u8],
    thresholds: Option<&[u8]>,
    identify: Option<&[u8]>,
    failure_predicted: Option<bool>,
    via: HealthPath,
) -> HealthReport {
    let mut report = HealthReport::empty(Some(path_name(via).to_owned()));
    if !checksum_ok(page) {
        report.findings.push(finding(
            Level::Info,
            "The drive's attribute page doesn't add up (its checksum is wrong), so some values \
             may be wrong.",
        ));
    }
    let limits = thresholds.filter(|t| checksum_ok(t)).map(parse_thresholds);
    if limits.is_none() && via != HealthPath::FailurePrediction {
        report.findings.push(finding(
            Level::Info,
            "The drive didn't give its attribute thresholds, so only its counters were checked.",
        ));
    }
    if via == HealthPath::FailurePrediction {
        report.findings.push(finding(
            Level::Info,
            "Read through Windows' failure prediction, which gives no attribute thresholds: \
             only the counters were checked.",
        ));
    }
    report.attributes = parse_attributes(page, limits.as_ref());
    if failure_predicted == Some(true) {
        report.findings.push(finding(
            Level::Bad,
            "The drive itself predicts that it will fail (Windows' failure prediction).",
        ));
    }
    if let Some(identify) = identify.filter(|page| page.len() >= PAGE) {
        let rotation = word(identify, 217);
        match rotation {
            1 => report.solid_state = Some(true),
            0x0401..=0xFFFE => {
                report.solid_state = Some(false);
                report.rpm = Some(u32::from(rotation));
            }
            _ => {}
        }
    }

    for attribute in &report.attributes {
        let raw = attribute.raw;
        // Sector counts sit in the low four bytes; some drives put other
        // things in the upper two.
        let low = raw & 0xFFFF_FFFF;
        match attribute.id {
            9 => report.power_on_hours = Some(low),
            12 => report.power_cycles = Some(low),
            194 | 190 if report.temperature.is_none() => {
                let celsius = raw & 0xFF;
                if (1..=120).contains(&celsius) {
                    report.temperature = i64::try_from(celsius).ok();
                }
            }
            _ => {}
        }
    }

    let mut findings = Vec::new();
    for a in &report.attributes {
        let low = a.raw & 0xFFFF_FFFF;
        let threshold = a.threshold.unwrap_or(0);
        match a.state {
            AttributeState::Failing if a.prefail => findings.push(finding(
                Level::Bad,
                format!(
                    "{} ({}) has fallen to the drive's own failure threshold ({} ≤ {threshold}): \
                     the drive expects to fail soon.",
                    a.name, a.id, a.current
                ),
            )),
            AttributeState::Failing => findings.push(finding(
                Level::Caution,
                format!(
                    "{} ({}) has fallen to its threshold ({} ≤ {threshold}): the drive is past \
                     what it was made for.",
                    a.name, a.id, a.current
                ),
            )),
            AttributeState::FailedBefore => findings.push(finding(
                Level::Caution,
                format!(
                    "{} ({}) fell to its failure threshold once (lowest {}, threshold \
                     {threshold}).",
                    a.name, a.id, a.worst
                ),
            )),
            _ => {}
        }
        if low == 0 {
            continue;
        }
        let sectors = |what: &str| count(low, "sector", "sectors") + what;
        match a.id {
            5 => findings.push(finding(
                Level::Caution,
                sectors(" couldn't be read reliably and were replaced with spares (reallocated)."),
            )),
            197 => findings.push(finding(
                Level::Caution,
                sectors(
                    " couldn't be read and are waiting to be replaced (pending): what they \
                     held may already be lost.",
                ),
            )),
            198 => findings.push(finding(
                Level::Caution,
                sectors(" failed the drive's own scan and can't be read (uncorrectable)."),
            )),
            187 => findings.push(finding(
                Level::Caution,
                count(low, "read error", "read errors")
                    + " couldn't be corrected by the drive (reported uncorrectable).",
            )),
            10 => findings.push(finding(
                Level::Caution,
                format!(
                    "The drive needed {} to spin up (spin retries).",
                    count(low, "retry", "retries")
                ),
            )),
            199 => findings.push(finding(
                Level::Info,
                count(low, "transfer error", "transfer errors")
                    + " between the drive and the computer (UltraDMA CRC): usually a loose or \
                       worn cable, not the drive.",
            )),
            _ => {}
        }
    }
    if let Some(celsius) = report.temperature
        && celsius >= 60
    {
        findings.push(finding(
            Level::Caution,
            format!("The drive is running hot ({celsius} °C)."),
        ));
    }
    report.findings.extend(findings);
    report.conclude()
}

fn checksum_ok(page: &[u8]) -> bool {
    page.len() == PAGE && page.iter().fold(0u8, |sum, &b| sum.wrapping_add(b)) == 0
}

/// A little-endian 16-bit word of an IDENTIFY page.
fn word(page: &[u8], index: usize) -> u16 {
    let at = index * 2;
    page.get(at..at + 2)
        .map_or(0, |b| u16::from_le_bytes([b[0], b[1]]))
}

fn entries(page: &[u8]) -> impl Iterator<Item = &[u8]> {
    (0..ENTRIES).filter_map(move |i| {
        let at = 2 + i * ENTRY_BYTES;
        page.get(at..at + ENTRY_BYTES).filter(|entry| entry[0] != 0)
    })
}

fn parse_thresholds(page: &[u8]) -> HashMap<u8, u8> {
    entries(page).map(|e| (e[0], e[1])).collect()
}

fn parse_attributes(page: &[u8], thresholds: Option<&HashMap<u8, u8>>) -> Vec<Attribute> {
    entries(page)
        .map(|e| {
            let id = e[0];
            let flags = u16::from_le_bytes([e[1], e[2]]);
            let (current, worst) = (e[3], e[4]);
            let mut raw = [0u8; 8];
            raw[..6].copy_from_slice(&e[5..11]);
            let raw = u64::from_le_bytes(raw);
            // 0 means "always passes"; 0xFE and 0xFF aren't real thresholds.
            let threshold = thresholds
                .and_then(|t| t.get(&id).copied())
                .filter(|&t| (1..=0xFD).contains(&t));
            let valid = |value: u8| (1..=0xFD).contains(&value);
            let state = match threshold {
                Some(t) if valid(current) && current <= t => AttributeState::Failing,
                Some(t) if valid(worst) && worst <= t => AttributeState::FailedBefore,
                _ if WATCHED.contains(&id) && raw & 0xFFFF_FFFF != 0 => AttributeState::Watch,
                _ => AttributeState::Ok,
            };
            Attribute {
                id,
                name: attribute_name(id),
                current,
                worst,
                threshold,
                raw,
                prefail: flags & 1 != 0,
                state,
            }
        })
        .collect()
}

/// Counters that should stay at zero on a healthy drive.
const WATCHED: [u8; 6] = [5, 10, 187, 197, 198, 199];

/// Common names for S.M.A.R.T. attributes. Drive makers don't all agree;
/// where they differ, the name says "usually".
pub(crate) fn attribute_name(id: u8) -> &'static str {
    match id {
        1 => "Read error rate",
        2 => "Throughput performance",
        3 => "Spin-up time",
        4 => "Start/stop count",
        5 => "Reallocated sectors",
        7 => "Seek error rate",
        8 => "Seek time performance",
        9 => "Power-on hours",
        10 => "Spin retries",
        11 => "Calibration retries",
        12 => "Power cycles",
        13 => "Soft read error rate",
        22 => "Helium level",
        170 => "Reserved blocks available",
        171 => "Program failures",
        172 => "Erase failures",
        173 => "Wear leveling count",
        174 => "Unexpected power losses",
        175 => "Power loss protection failures",
        177 => "Wear range delta",
        179 => "Used reserved blocks",
        180 => "Unused reserved blocks",
        181 => "Program failures",
        182 => "Erase failures",
        183 => "SATA downshifts (usually)",
        184 => "End-to-end errors",
        187 => "Reported uncorrectable errors",
        188 => "Command timeouts",
        189 => "High fly writes",
        190 => "Airflow temperature",
        191 => "G-sense errors",
        192 => "Power-off retracts",
        193 => "Load/unload cycles",
        194 => "Temperature",
        195 => "Hardware ECC recovered",
        196 => "Reallocation events",
        197 => "Pending sectors",
        198 => "Uncorrectable sectors",
        199 => "UltraDMA CRC errors",
        200 => "Write error rate (usually)",
        201 => "Soft read errors",
        202 => "Data address mark errors (usually)",
        220 => "Disk shift",
        222 => "Loaded hours",
        223 => "Load/unload retries",
        224 => "Load friction",
        225 => "Load/unload cycles",
        226 => "Load-in time",
        230 => "Head amplitude (usually)",
        231 => "Life left (SSDs, usually)",
        232 => "Endurance remaining (usually)",
        233 => "Media wearout (usually)",
        234 => "Erase count (usually)",
        235 => "Good blocks (usually)",
        240 => "Head flying hours",
        241 => "Total written",
        242 => "Total read",
        249 => "NAND writes (usually)",
        250 => "Read error retries",
        254 => "Free fall events",
        _ => "Vendor specific",
    }
}

#[cfg(test)]
pub(crate) mod samples {
    /// A S.M.A.R.T. attribute page: (id, flags, current, worst, raw) per entry.
    pub fn attributes(entries: &[(u8, u16, u8, u8, u64)]) -> Vec<u8> {
        let mut page = vec![0u8; 512];
        page[0] = 0x10;
        for (i, &(id, flags, current, worst, raw)) in entries.iter().enumerate() {
            let at = 2 + i * 12;
            page[at] = id;
            page[at + 1..at + 3].copy_from_slice(&flags.to_le_bytes());
            page[at + 3] = current;
            page[at + 4] = worst;
            page[at + 5..at + 11].copy_from_slice(&raw.to_le_bytes()[..6]);
        }
        seal(page)
    }

    /// A threshold page: (id, threshold) per entry.
    pub fn thresholds(entries: &[(u8, u8)]) -> Vec<u8> {
        let mut page = vec![0u8; 512];
        page[0] = 0x10;
        for (i, &(id, threshold)) in entries.iter().enumerate() {
            page[2 + i * 12] = id;
            page[3 + i * 12] = threshold;
        }
        seal(page)
    }

    /// Sets the checksum byte so the page adds up to zero.
    pub fn seal(mut page: Vec<u8>) -> Vec<u8> {
        let sum = page[..511].iter().fold(0u8, |s, &b| s.wrapping_add(b));
        page[511] = sum.wrapping_neg();
        page
    }
}

#[cfg(test)]
mod tests {
    use super::samples::{attributes, seal, thresholds};
    use super::*;
    use crate::report::Verdict;

    fn healthy() -> (Vec<u8>, Vec<u8>) {
        (
            attributes(&[
                (1, 0x000F, 200, 200, 0),
                (5, 0x0033, 200, 200, 0),
                (9, 0x0032, 90, 90, 8_760),
                (12, 0x0032, 100, 100, 1_234),
                (194, 0x0022, 112, 100, 0x0028_0012_0026), // 38 °C, min/max in the upper bytes
                (197, 0x0032, 200, 200, 0),
            ]),
            thresholds(&[(1, 51), (5, 140), (9, 0), (12, 0), (194, 0), (197, 0)]),
        )
    }

    #[test]
    fn a_healthy_drive_is_good() {
        let (page, limits) = healthy();
        let mut identify = vec![0u8; 512];
        identify[434..436].copy_from_slice(&7200u16.to_le_bytes());
        let report = assess(
            &page,
            Some(&limits),
            Some(&identify),
            Some(false),
            HealthPath::Driver,
        );
        assert_eq!(report.verdict, Verdict::Good, "{:#?}", report.findings);
        assert!(report.findings.is_empty(), "{:#?}", report.findings);
        assert_eq!(report.power_on_hours, Some(8_760));
        assert_eq!(report.power_cycles, Some(1_234));
        assert_eq!(report.temperature, Some(38));
        assert_eq!((report.solid_state, report.rpm), (Some(false), Some(7200)));
        assert_eq!(report.attributes.len(), 6);
        assert_eq!(report.attributes[1].name, "Reallocated sectors");
        assert_eq!(report.attributes[1].threshold, Some(140));
        assert!(report.attributes[1].prefail);
        assert_eq!(
            report.attributes[2].threshold, None,
            "0 means always passing"
        );
    }

    #[test]
    fn pending_and_reallocated_sectors_call_for_caution() {
        let page = attributes(&[
            (5, 0x0033, 199, 199, 24),
            (197, 0x0032, 200, 200, 3),
            (199, 0x003E, 200, 200, 7),
        ]);
        let limits = thresholds(&[(5, 140), (197, 0), (199, 0)]);
        let report = assess(&page, Some(&limits), None, None, HealthPath::Driver);
        assert_eq!(report.verdict, Verdict::Caution);
        let messages: Vec<_> = report.findings.iter().map(|f| f.message.as_str()).collect();
        assert!(
            messages[0].starts_with("24 sectors couldn't be read reliably"),
            "{messages:?}"
        );
        assert!(messages[1].starts_with("3 sectors couldn't be read and are waiting"));
        assert!(
            messages[2].contains("cable"),
            "CRC errors are only information"
        );
        assert_eq!(report.findings[2].level, Level::Info);
        assert_eq!(report.attributes[0].state, AttributeState::Watch);
    }

    #[test]
    fn an_attribute_at_its_threshold_means_failing() {
        let page = attributes(&[(5, 0x0033, 100, 100, 2_000), (9, 0x0032, 0, 0, 70_000)]);
        let limits = thresholds(&[(5, 140), (9, 0)]);
        let report = assess(&page, Some(&limits), None, None, HealthPath::Driver);
        assert_eq!(report.verdict, Verdict::Bad);
        assert_eq!(report.attributes[0].state, AttributeState::Failing);
        assert!(report.findings[0].message.contains("expects to fail"));
        assert!(report.summary.contains("Copy it"));

        // An age attribute at its threshold is a caution, not a failure.
        let page = attributes(&[(193, 0x0032, 1, 1, 600_000)]);
        let limits = thresholds(&[(193, 1)]);
        let report = assess(&page, Some(&limits), None, None, HealthPath::Driver);
        assert_eq!(report.verdict, Verdict::Caution);

        // Fell once and came back.
        let page = attributes(&[(3, 0x0027, 180, 20, 0)]);
        let limits = thresholds(&[(3, 21)]);
        let report = assess(&page, Some(&limits), None, None, HealthPath::Driver);
        assert_eq!(report.attributes[0].state, AttributeState::FailedBefore);
        assert_eq!(report.verdict, Verdict::Caution);
    }

    #[test]
    fn failure_prediction_alone_is_enough_to_fail() {
        let page = attributes(&[(9, 0x0032, 99, 99, 100)]);
        let report = assess(&page, None, None, Some(true), HealthPath::FailurePrediction);
        assert_eq!(report.verdict, Verdict::Bad);
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.message.contains("predicts"))
        );
        assert!(
            report
                .source
                .as_deref()
                .unwrap()
                .contains("failure prediction")
        );
    }

    #[test]
    fn solid_state_drives_say_so_in_identify() {
        let (page, limits) = healthy();
        let mut identify = vec![0u8; 512];
        identify[434] = 1;
        let report = assess(
            &page,
            Some(&limits),
            Some(&identify),
            None,
            HealthPath::Driver,
        );
        assert_eq!((report.solid_state, report.rpm), (Some(true), None));
    }

    #[test]
    fn garbled_pages_never_panic() {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        for len in [0, 1, 13, 100, 511, 512, 513] {
            for _ in 0..50 {
                let page: Vec<u8> = (0..len)
                    .map(|_| {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        state.to_le_bytes()[0]
                    })
                    .collect();
                let report = assess(&page, Some(&page), Some(&page), None, HealthPath::Driver);
                assert_ne!(report.verdict, Verdict::Unknown);
            }
        }
        let bad_sum = {
            let mut page = seal(vec![0u8; 512]);
            page[100] = 1;
            page
        };
        let report = assess(&bad_sum, None, None, None, HealthPath::Driver);
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.message.contains("checksum"))
        );
    }
}
