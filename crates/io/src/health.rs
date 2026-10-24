//! Raw health data from a drive: the S.M.A.R.T. pages of ATA (SATA) drives
//! and the health log of NVMe drives, as the drive returns them. The
//! `datareco-health` crate makes sense of them.
//!
//! Only commands that read are sent: on ATA drives SMART READ DATA, SMART
//! READ THRESHOLDS and IDENTIFY DEVICE, and on NVMe drives GET LOG PAGE for
//! the health log. Nothing changes a drive's settings, not even turning
//! S.M.A.R.T. on when a drive has it off.

use serde::Serialize;

use crate::error::Result;

/// Bytes in each ATA S.M.A.R.T. page, IDENTIFY page and NVMe health log.
pub const HEALTH_PAGE: usize = 512;

/// What a drive told about its health.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HealthData {
    /// An ATA drive's S.M.A.R.T. pages, each [`HEALTH_PAGE`] bytes.
    Ata {
        attributes: Vec<u8>,
        /// Missing when the drive or the way to it doesn't give them.
        thresholds: Option<Vec<u8>>,
        /// The IDENTIFY DEVICE page, for whether S.M.A.R.T. is on and the
        /// rotation rate.
        identify: Option<Vec<u8>>,
        /// Whether Windows' failure prediction says the drive will fail.
        failure_predicted: Option<bool>,
        via: HealthPath,
    },
    /// An NVMe drive's SMART / Health Information log page (log 02h).
    Nvme { log: Vec<u8> },
    /// Neither the drive nor what it's plugged into gave any. Why, for people.
    Unavailable { reason: String },
}

/// How ATA health data was read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum HealthPath {
    /// Straight from the drive, through the disk driver.
    Driver,
    /// Through a USB adapter that passes ATA commands on (SCSI/ATA Translation).
    UsbAdapter,
    /// Windows' failure prediction query: the attributes, but no thresholds.
    FailurePrediction,
}

/// Reads physical disk `number`'s health data. Needs administrator rights.
pub fn read_health_data(number: u32) -> Result<HealthData> {
    crate::platform::read_health_data(number)
}
