//! Drive health: what a drive's S.M.A.R.T. data says (ATA and NVMe), a
//! surface test that reads and times every sector, and a read speed test.
//!
//! Everything here only reads. The S.M.A.R.T. pages come from
//! [`datareco_io::read_health_data`] and are untrusted: parsers never panic
//! on short or garbled pages.

mod ata;
mod nvme;
mod report;
mod speed;
mod surface;

pub use report::{
    Attribute, AttributeState, Finding, HealthReport, Level, NvmeHealth, Verdict, assess,
};
pub use speed::{RandomResult, SequentialResult, SpeedStatus, SpeedTest};
pub use surface::{
    BadRange, BlockSpeed, SURFACE_BLOCK_BYTES, SpeedCounts, SurfaceProgress, SurfaceTest, classify,
};
