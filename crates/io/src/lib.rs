//! Disk and image-file access: listing disks, reading sectors, caching, and
//! handling bad sectors (retry, skip, log).
//!
//! This is the only crate allowed to use `unsafe`, for the OS calls that open
//! raw disks. Filled in by Phase 1 (see docs/ROADMAP.md).
