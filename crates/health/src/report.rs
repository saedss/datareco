//! What a drive's health data means, for people: a verdict, the findings
//! behind it, and the values worth showing.

use datareco_io::{HealthData, HealthPath};
use serde::Serialize;

use crate::{ata, nvme};

/// The overall verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// No signs of trouble.
    Good,
    /// Signs of wear or damage: copy what matters off it soon.
    Caution,
    /// Failing: copy it now, before anything else.
    Bad,
    /// No health data to judge by.
    Unknown,
}

/// How much a finding matters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Info,
    Caution,
    Bad,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub level: Level,
    pub message: String,
}

pub(crate) fn finding(level: Level, message: impl Into<String>) -> Finding {
    Finding {
        level,
        message: message.into(),
    }
}

/// An ATA S.M.A.R.T. attribute.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Attribute {
    pub id: u8,
    pub name: &'static str,
    /// The drive's own score, usually 100 or 200 when new and falling.
    pub current: u8,
    /// The lowest the score has been.
    pub worst: u8,
    /// The score at or below which the drive counts the attribute as failed.
    pub threshold: Option<u8>,
    /// The raw counter (six bytes; what it counts depends on the attribute).
    pub raw: u64,
    /// Pre-failure attribute: falling to its threshold means the drive is
    /// about to fail (the others track age and use).
    pub prefail: bool,
    pub state: AttributeState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AttributeState {
    Ok,
    /// A counter that should stay at zero isn't.
    Watch,
    /// Fell to its threshold once, and has come back up since.
    FailedBefore,
    /// At or below its threshold now.
    Failing,
}

/// An NVMe drive's SMART / Health Information log.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NvmeHealth {
    /// Critical warning bits (byte 0).
    pub critical_warning: u8,
    /// Composite temperature in °C.
    pub temperature: Option<i64>,
    /// Spare capacity left, in percent, and the level the drive warns below.
    pub available_spare: u8,
    pub spare_threshold: u8,
    /// Share of the rated life used, in percent (can pass 100).
    pub percentage_used: u8,
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub host_reads: u64,
    pub host_writes: u64,
    pub busy_minutes: u64,
    pub power_cycles: u64,
    pub power_on_hours: u64,
    pub unsafe_shutdowns: u64,
    /// Times the drive found data it couldn't correct.
    pub media_errors: u64,
    pub error_log_entries: u64,
    pub warning_temperature_minutes: u64,
    pub critical_temperature_minutes: u64,
}

/// What a drive's health data says.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthReport {
    pub verdict: Verdict,
    /// The verdict in a sentence, with what to do about it.
    pub summary: String,
    /// How the data was read: "ATA (S.M.A.R.T.)", "NVMe", …
    pub source: Option<String>,
    /// The reasons for the verdict, worst first.
    pub findings: Vec<Finding>,
    /// °C.
    pub temperature: Option<i64>,
    pub power_on_hours: Option<u64>,
    pub power_cycles: Option<u64>,
    /// NVMe: share of the rated life used, in percent.
    pub life_used: Option<u8>,
    pub bytes_written: Option<u64>,
    pub bytes_read: Option<u64>,
    /// ATA: whether the drive says it's solid-state, and its speed if it spins.
    pub solid_state: Option<bool>,
    pub rpm: Option<u32>,
    /// ATA attributes, in the drive's order.
    pub attributes: Vec<Attribute>,
    pub nvme: Option<NvmeHealth>,
}

impl HealthReport {
    pub(crate) fn empty(source: Option<String>) -> Self {
        HealthReport {
            verdict: Verdict::Unknown,
            summary: String::new(),
            source,
            findings: Vec::new(),
            temperature: None,
            power_on_hours: None,
            power_cycles: None,
            life_used: None,
            bytes_written: None,
            bytes_read: None,
            solid_state: None,
            rpm: None,
            attributes: Vec::new(),
            nvme: None,
        }
    }

    /// Sets the verdict and summary from the findings.
    pub(crate) fn conclude(mut self) -> Self {
        self.findings.sort_by_key(|f| std::cmp::Reverse(f.level));
        let worst = self.findings.iter().map(|f| f.level).max();
        (self.verdict, self.summary) = match worst {
            Some(Level::Bad) => (
                Verdict::Bad,
                "This drive is failing. Copy it to an image file or another disk now, before \
                 anything else, and recover from the copy: every read may be one of its last."
                    .to_owned(),
            ),
            Some(Level::Caution) => (
                Verdict::Caution,
                "This drive shows signs of wear or damage. Copy what matters off it soon; to \
                 recover files from it, copy it first and recover from the copy."
                    .to_owned(),
            ),
            _ => (Verdict::Good, "No signs of trouble.".to_owned()),
        };
        self
    }
}

/// Makes sense of a drive's health data.
pub fn assess(data: &HealthData) -> HealthReport {
    match data {
        HealthData::Ata {
            attributes,
            thresholds,
            identify,
            failure_predicted,
            via,
        } => ata::assess(
            attributes,
            thresholds.as_deref(),
            identify.as_deref(),
            *failure_predicted,
            *via,
        ),
        HealthData::Nvme { log } => nvme::assess(log),
        HealthData::Unavailable { reason } => {
            let mut report = HealthReport::empty(None);
            report.summary.clone_from(reason);
            report
        }
    }
}

/// How the data was read, for people.
pub(crate) fn path_name(via: HealthPath) -> &'static str {
    match via {
        HealthPath::Driver => "ATA S.M.A.R.T.",
        HealthPath::UsbAdapter => "ATA S.M.A.R.T., through the USB adapter",
        HealthPath::FailurePrediction => "Windows' failure prediction",
    }
}

/// "1 sector", "12 sectors".
pub(crate) fn count(n: u64, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}
