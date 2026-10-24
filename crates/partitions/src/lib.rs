//! Partition tables (MBR with extended/logical partitions, GPT) and the search
//! for lost partitions using boot sectors, superblocks and their backups.
//!
//! Nothing here writes to a disk. The [`edit`] module works out the sectors
//! a partition table change would write and keeps backups to undo it; the
//! caller writes them, to a target it opened after the person confirmed.
//! Disk data is untrusted: parsers use bounds-checked reads (`bytes`
//! module) and never panic on garbage.

mod boot;
mod bytes;
mod crc32;
pub mod edit;
mod gpt;
mod layout;
mod mbr;
mod scan;
mod table;
#[cfg(test)]
mod tests;

pub use boot::{FileSystem, FsKind, identify};
pub use crc32::crc32c;
pub use layout::{
    ALIGNMENT_BYTES, DiskMap, MBR_SECTOR_LIMIT, Segment, SegmentKind, disk_map, layout,
};
pub use scan::{Confidence, FoundPartition, Scan, ScanMode, ScanProgress, ScanState};
pub use table::{
    Partition, PartitionKind, PartitionTable, Problem, Severity, TableKind, UsableSectors,
    read_table,
};
