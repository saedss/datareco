//! `datareco` command-line tool. Uses the same engine crates as the desktop app.

mod files;
mod format;
mod health;
mod imaging;
mod manage;
mod migrate;
mod partitions;
mod raw;
mod relocate;
mod scan;
mod structures;
mod table;

use std::fmt;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use datareco_io::{DeviceList, Disk, Error, SourceSpec, VolumeInfo, is_elevated, list_devices};

use crate::files::VolumeChoice;
use crate::format::{format_size, hexdump};

/// Everything a command can fail with.
#[derive(Debug)]
enum CliError {
    /// Opening or reading a source failed.
    Disk(Error),
    /// Anything else, already worded for people.
    Message(String),
}

impl From<Error> for CliError {
    fn from(error: Error) -> Self {
        CliError::Disk(error)
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::Disk(error) => error.fmt(f),
            CliError::Message(message) => f.write_str(message),
        }
    }
}

/// `0x1BE` or `446`.
fn parse_offset(text: &str) -> Result<u64, String> {
    datareco_templates::parse_integer(text)
        .and_then(|n| u64::try_from(n).ok())
        .ok_or_else(|| format!("\"{text}\" is not an offset (use decimal or 0x hex)"))
}

/// Which volume of the source to use. Without any of these, the only one
/// DataReco can read.
#[derive(Args)]
struct VolumeArgs {
    /// Which partition, by its number in `datareco partitions`.
    #[arg(long, short, conflicts_with_all = ["offset", "scan", "raw"])]
    partition: Option<u32>,
    /// Where the volume starts, as a byte offset (decimal or 0x hex).
    #[arg(long, value_parser = parse_offset, conflicts_with_all = ["scan", "raw"])]
    offset: Option<u64>,
    /// A scan saved with `datareco scan --save`, to use a volume it found.
    #[arg(long, requires = "volume", conflicts_with = "raw")]
    scan: Option<PathBuf>,
    /// That volume's number in the scan's list.
    #[arg(long, requires = "scan")]
    volume: Option<usize>,
    /// A raw scan saved with `datareco raw --save`: the files it found by
    /// their contents, in a folder per kind (JPG, PDF, …).
    #[arg(long)]
    raw: Option<PathBuf>,
}

impl VolumeArgs {
    fn choice(self) -> VolumeChoice {
        VolumeChoice {
            partition: self.partition,
            offset: self.offset,
            scan: self.scan.zip(self.volume),
            raw: self.raw,
        }
    }
}

/// DataReco: disk editor and data recovery tool. Never writes to disks.
/// How `image` and `clone` copy.
#[derive(Args)]
struct CopyArgs {
    /// The map file recording what was copied (default: next to the image,
    /// or disk1-to-disk2.map in this folder). If it exists, the copy carries on.
    #[arg(long)]
    map: Option<PathBuf>,
    /// Passes over the sectors that couldn't be read, at the end.
    #[arg(long, default_value_t = 1)]
    retries: u32,
    /// Don't skip ahead after read errors: read everything in order. Skipping
    /// gets the most off a failing drive first.
    #[arg(long)]
    no_skip: bool,
}

impl CopyArgs {
    fn request<'a>(&'a self, source: &'a str) -> imaging::CopyRequest<'a> {
        imaging::CopyRequest {
            source,
            map: self.map.as_deref(),
            retries: self.retries,
            skip: !self.no_skip,
        }
    }
}

#[derive(Parser)]
#[command(name = "datareco", version, about, arg_required_else_help = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List physical disks and volumes.
    Disks {
        /// Print JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Show sectors of a disk, volume or image file as hex.
    Read {
        /// What to read: a disk (disk0), a volume (C:) or an image file path.
        source: String,
        /// First sector to show.
        #[arg(long, short, default_value_t = 0)]
        sector: u64,
        /// How many sectors to show.
        #[arg(long, short = 'n', default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=4096))]
        count: u32,
    },
    /// List the structure templates `decode` can use.
    Templates {
        /// Also load the template files in this folder.
        #[arg(long)]
        templates_dir: Option<PathBuf>,
    },
    /// Decode a structure (boot sector, partition table, MFT record, …).
    Decode {
        /// What to read: a disk (disk0), a volume (C:) or an image file path.
        source: String,
        /// Template id from `datareco templates`, or "auto" to recognize it.
        template: String,
        /// Where the structure starts, as a sector number.
        #[arg(long, short, conflicts_with = "offset")]
        sector: Option<u64>,
        /// Where the structure starts, as a byte offset (decimal or 0x hex).
        #[arg(long, value_parser = parse_offset)]
        offset: Option<u64>,
        /// Also load the template files in this folder.
        #[arg(long)]
        templates_dir: Option<PathBuf>,
    },
    /// Show the partition table, with file systems and problems.
    Partitions {
        /// A disk (disk0) or image file path.
        source: String,
    },
    /// Map a disk: its partitions, free space, drive letters and free space on
    /// each volume.
    Map {
        /// A disk (disk0), volume (C:) or image file path.
        source: String,
    },
    /// Check a drive's health from its S.M.A.R.T. data (SATA, NVMe, and
    /// USB adapters that pass it on).
    Health {
        /// A disk (disk0), or a volume on it (C:).
        source: String,
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Read every sector once and time it: finds slow and unreadable areas.
    /// On a failing drive, copy it first (`image`): every read wears it.
    Surface {
        /// A disk (disk0), volume (C:) or image file path.
        source: String,
        /// Only test this partition, by its number in `datareco partitions`.
        #[arg(long, short)]
        partition: Option<u32>,
    },
    /// Measure how fast a disk reads (about 12 seconds; nothing is written).
    Speed {
        /// A disk (disk0) or volume (C:).
        source: String,
    },
    /// Change a disk's partition table: create, delete, hide, convert MBR↔GPT,
    /// restore a lost partition, undo. Shows the change first; writes only
    /// with --apply and the disk's name, after backing up the old table.
    Table {
        /// A disk (disk1) or image file path.
        source: String,
        #[command(subcommand)]
        action: table::TableAction,
        /// Write the changes, typing the disk's name (disk1) or the image's file name.
        #[arg(long, global = true)]
        apply: Option<String>,
        /// Where backups of old tables go (default: %LOCALAPPDATA%\DataReco\table-backups).
        #[arg(long, global = true)]
        backup_dir: Option<PathBuf>,
    },
    /// Format a partition (or a disk or image with no partition table): FAT32
    /// (any size) and exFAT by DataReco, NTFS through Windows. Shows what it
    /// would do; formats only with --apply and the disk's name.
    Format {
        /// A disk (disk1) or image file path.
        source: String,
        /// The partition, by its number in `datareco partitions`.
        #[arg(long, short)]
        partition: Option<u32>,
        /// fat32, exfat or ntfs.
        #[arg(long = "fs")]
        file_system: String,
        /// The volume label.
        #[arg(long)]
        label: Option<String>,
        /// Cluster size, e.g. 32K (default: the usual one for its size).
        #[arg(long)]
        cluster: Option<String>,
        /// NTFS: read and check every sector (slow). Without it, a quick format.
        #[arg(long)]
        full: bool,
        /// Write it, typing the disk's name (disk1) or the image's file name.
        #[arg(long)]
        apply: Option<String>,
    },
    /// Write over a partition, or a whole disk, so nothing on it can be
    /// recovered. Shows what it would do; wipes only with --apply.
    Wipe {
        /// A disk (disk1) or image file path.
        source: String,
        /// Only this partition (default: the whole disk).
        #[arg(long, short)]
        partition: Option<u32>,
        /// Random bytes instead of zeros.
        #[arg(long)]
        random: bool,
        /// Write it, typing the disk's name (disk1) or the image's file name.
        #[arg(long)]
        apply: Option<String>,
    },
    /// Change a volume's label (without one, remove it).
    Label {
        /// A volume (E:), or a disk with -p.
        source: String,
        #[arg(long, short)]
        partition: Option<u32>,
        label: Option<String>,
    },
    /// Change a volume's drive letter, or take it away with "none". Shows the
    /// change; makes it only with --apply and the volume named again.
    Letter {
        /// A volume (E:), or a disk with -p.
        source: String,
        #[arg(long, short)]
        partition: Option<u32>,
        letter: String,
        /// Change it, naming the volume again as given (E:, or disk1 with -p).
        #[arg(long)]
        apply: Option<String>,
    },
    /// Move a partition to another place on its disk, block by block. A journal
    /// on another disk lets a move stopped by a power cut or Ctrl+C carry on
    /// (--resume). Shows the move; moves only with --apply and the disk's name.
    Move {
        /// A disk (disk1) or image file path.
        source: String,
        /// The partition, by its number in `datareco partitions`.
        #[arg(long, short)]
        partition: Option<u32>,
        /// Its new first sector.
        #[arg(long, conflicts_with = "by")]
        to_sector: Option<u64>,
        /// How far: +100MiB (towards the end), -1GiB (towards the start), +2048s.
        #[arg(long, allow_hyphen_values = true)]
        by: Option<String>,
        /// Carry on this disk's unfinished move.
        #[arg(long, conflicts_with_all = ["partition", "to_sector", "by"])]
        resume: bool,
        /// Where the journal goes (default: %LOCALAPPDATA%\DataReco\moves).
        #[arg(long)]
        journal_dir: Option<PathBuf>,
        /// Where the old partition table's backup goes.
        #[arg(long)]
        backup_dir: Option<PathBuf>,
        /// Write it, typing the disk's name (disk1) or the image's file name.
        #[arg(long)]
        apply: Option<String>,
    },
    /// Move a partition to the nearest 1 MiB boundary, as SSDs and drives
    /// with 4 KiB sectors work best with.
    Align {
        /// A disk (disk1) or image file path.
        source: String,
        #[arg(long, short)]
        partition: u32,
        #[arg(long)]
        journal_dir: Option<PathBuf>,
        #[arg(long)]
        backup_dir: Option<PathBuf>,
        #[arg(long)]
        apply: Option<String>,
    },
    /// Copy a partition into free space on another disk (its used space
    /// only), and add it to that disk's partition table. Shows the copy;
    /// copies only with --apply and the target's name.
    ClonePartition {
        /// A disk (disk1) or image file path.
        source: String,
        #[arg(long, short)]
        partition: u32,
        /// The disk (disk2) or image file to copy it onto.
        #[arg(long)]
        to: String,
        /// Where on it the copy starts (default: the first free space that fits).
        #[arg(long)]
        at_sector: Option<u64>,
        /// Where the target's old partition table's backup goes.
        #[arg(long)]
        backup_dir: Option<PathBuf>,
        /// Copy it, typing the target's name (disk2) or the image's file name.
        #[arg(long)]
        apply: Option<String>,
    },
    /// Copy every partition of a disk onto another disk, used space only, so
    /// a smaller disk will do if they fit. From the disk Windows runs from,
    /// the copy is one Windows starts from. Everything on the target is
    /// erased. Shows the copy; copies only with --apply and the target's name.
    Migrate {
        /// A disk (disk0) or image file path.
        source: String,
        /// The disk (disk2) or image file to copy it onto.
        #[arg(long)]
        to: String,
        /// Keep the disk's IDs (the default for the disk Windows runs from):
        /// Windows then keeps one of the two disks offline while both are
        /// connected.
        #[arg(long, conflicts_with = "new_ids")]
        keep_ids: bool,
        /// Give the copy new disk IDs, so both can be used side by side.
        #[arg(long)]
        new_ids: bool,
        /// Leave the target's spare room right after this partition, so
        /// Resize can grow it there later.
        #[arg(long)]
        space_after: Option<u32>,
        /// Copy it, typing the target's name (disk2) or the image's file name.
        #[arg(long)]
        apply: Option<String>,
    },
    /// Shrink or grow an NTFS partition through Windows (C: too, while it
    /// runs). Growing needs free space right after it.
    Resize {
        /// A disk (disk1).
        source: String,
        #[arg(long, short)]
        partition: u32,
        /// The new size (50GiB, 500MiB), or min or max.
        #[arg(long)]
        size: String,
        /// Do it, typing the disk's name (disk1).
        #[arg(long)]
        apply: Option<String>,
    },
    /// Search for lost partitions (after a wiped or damaged partition table).
    FindPartitions {
        /// A disk (disk0) or image file path.
        source: String,
        /// Look at every sector instead of only where partitions usually start.
        #[arg(long)]
        full: bool,
    },
    /// List a folder of a FAT, exFAT, NTFS, ext2/3/4, HFS+, APFS or btrfs volume, deleted files
    /// included (APFS: each volume of the container is a folder).
    Ls {
        /// A disk (disk0), volume (C:) or image file path.
        source: String,
        /// The folder, e.g. "docs/photos" (default: the top folder).
        #[arg(default_value = "")]
        path: String,
        #[command(flatten)]
        volume: VolumeArgs,
        /// Show only deleted files and folders.
        #[arg(long)]
        deleted: bool,
        /// Also list what's in the folders inside, and so on.
        #[arg(long, short)]
        recursive: bool,
    },
    /// Print a file from a FAT, exFAT, NTFS, ext2/3/4, HFS+, APFS or btrfs volume. Redirect it to a file on
    /// another disk to save it (in Windows PowerShell 5, `>` mangles binary
    /// data: use cmd or PowerShell 7).
    Cat {
        /// A disk (disk0), volume (C:) or image file path.
        source: String,
        /// The file, e.g. "docs/notes.txt".
        path: String,
        #[command(flatten)]
        volume: VolumeArgs,
        /// An NTFS alternate data stream of the file instead of its contents.
        #[arg(long)]
        stream: Option<String>,
        /// When a deleted file and a live one have the same name, take the deleted one.
        #[arg(long)]
        deleted: bool,
    },
    /// Look for volumes, including lost and damaged ones (after a format, a
    /// wiped partition table or a damaged boot sector), and list them best first.
    Scan {
        /// A disk (disk0), volume (C:) or image file path.
        source: String,
        /// Look at every sector: finds volumes whose boot sector, partition
        /// entry or FAT is gone. Slow on big disks.
        #[arg(long)]
        full: bool,
        /// Save the scan to this file as it goes, to use its volumes later
        /// (`ls`, `cat` and `recover` take `--scan FILE --volume N`) or to
        /// carry on after stopping it.
        #[arg(long)]
        save: Option<PathBuf>,
        /// Carry on with a scan saved with --save (or show what it found).
        #[arg(long, conflicts_with = "full")]
        resume: Option<PathBuf>,
    },
    /// Find files by their contents (JPEG, PDF, ZIP, MP4, …) when no file
    /// system says where they are, for example after a format.
    Raw {
        /// A disk (disk0), volume (C:) or image file path.
        source: String,
        /// Only look inside this partition, by its number in `datareco partitions`.
        #[arg(long, short, conflicts_with = "offset")]
        partition: Option<u32>,
        /// Only look from this byte offset on (decimal or 0x hex).
        #[arg(long, value_parser = parse_offset)]
        offset: Option<u64>,
        /// Use this signature list instead of the built-in one
        /// (`datareco signatures --write FILE` starts one).
        #[arg(long)]
        signatures: Option<PathBuf>,
        /// Save the scan to this file as it goes; `ls`, `cat` and `recover`
        /// take `--raw FILE` to use what it found.
        #[arg(long)]
        save: Option<PathBuf>,
        /// Carry on with a raw scan saved with --save.
        #[arg(long, conflicts_with_all = ["partition", "offset"])]
        resume: Option<PathBuf>,
        /// Save every file found into this folder (on another disk).
        #[arg(long, short = 't')]
        to: Option<PathBuf>,
    },
    /// List the file signatures raw recovery looks for, or write them to a
    /// file to change them.
    Signatures {
        /// Show this signature list instead of the built-in one.
        #[arg(long, conflicts_with = "write")]
        file: Option<PathBuf>,
        /// Write the built-in list to this new file, to edit.
        #[arg(long)]
        write: Option<PathBuf>,
    },
    /// Save files and folders from a volume to a folder on another disk,
    /// with their times, a log and SHA-256 checksums.
    Recover {
        /// A disk (disk0), volume (C:) or image file path.
        source: String,
        /// What to save, e.g. "Documents" or "Photos/beach.jpg" (default:
        /// everything on the volume).
        paths: Vec<String>,
        /// The folder to save into, on another disk.
        #[arg(long, short = 't')]
        to: PathBuf,
        #[command(flatten)]
        volume: VolumeArgs,
        /// Leave out deleted files found inside the folders being saved.
        #[arg(long)]
        no_deleted: bool,
        /// Leave out NTFS alternate data streams.
        #[arg(long)]
        no_streams: bool,
        /// When a deleted file and a live one have the same name, take the deleted one.
        #[arg(long)]
        deleted: bool,
    },
    /// Copy a disk, volume or image file into a new image file, in passes
    /// made for failing drives. A map file records what was copied, so the
    /// copy can stop and carry on: run the same command again.
    Image {
        /// What to copy: a disk (disk1), a volume (E:) or an image file path.
        source: String,
        /// The image file to write (on another disk).
        #[arg(long, short = 't')]
        to: PathBuf,
        #[command(flatten)]
        copy: CopyArgs,
    },
    /// Copy a disk, volume or image file onto another disk, erasing it. A map
    /// file records what was copied, so the copy can stop and carry on.
    Clone {
        /// What to copy: a disk (disk1), a volume (E:) or an image file path.
        source: String,
        /// The disk to copy onto, e.g. disk2. Everything on it is erased.
        #[arg(long, short = 't')]
        to: String,
        /// The disk being erased again, to confirm (e.g. --erase disk2).
        #[arg(long)]
        erase: Option<String>,
        #[command(flatten)]
        copy: CopyArgs,
    },
    /// Search a disk, volume or image file for text or bytes.
    Find {
        /// What to search: a disk (disk0), a volume (C:) or an image file path.
        source: String,
        /// Text to find (ASCII).
        #[arg(long, required_unless_present = "hex", conflicts_with = "hex")]
        text: Option<String>,
        /// Bytes to find, in hex, e.g. "55 AA".
        #[arg(long)]
        hex: Option<String>,
        /// Treat upper and lower case ASCII letters as the same.
        #[arg(long, short)]
        ignore_case: bool,
        /// Byte offset to start from (decimal or 0x hex).
        #[arg(long, value_parser = parse_offset, default_value = "0")]
        from: u64,
        /// Stop after this many matches.
        #[arg(long, short = 'n', default_value_t = 1)]
        count: u32,
    },
}

/// Stack for the thread commands run on. clap's parser for this many
/// commands needs more than Windows' default 1 MiB in debug builds.
const STACK: usize = 16 << 20;

fn main() -> ExitCode {
    std::thread::Builder::new()
        .name("datareco".into())
        .stack_size(STACK)
        .spawn(run)
        .map_or(ExitCode::FAILURE, |thread| {
            thread.join().unwrap_or(ExitCode::FAILURE)
        })
}

fn run() -> ExitCode {
    let result = match Cli::parse().command {
        Command::Disks { json } => disks(json),
        Command::Read {
            source,
            sector,
            count,
        } => read(&source, sector, count),
        Command::Templates { templates_dir } => structures::list(templates_dir.as_deref()),
        Command::Decode {
            source,
            template,
            sector,
            offset,
            templates_dir,
        } => structures::decode(&source, &template, sector, offset, templates_dir.as_deref()),
        Command::Partitions { source } => partitions::show(&source),
        Command::FindPartitions { source, full } => partitions::find(&source, full),
        Command::Map { source } => health::map(&source),
        Command::Move {
            source,
            partition,
            to_sector,
            by,
            resume,
            journal_dir,
            backup_dir,
            apply,
        } => relocate::move_partition(&relocate::MoveRequest {
            source: &source,
            partition,
            to_sector,
            by: by.as_deref(),
            resume,
            journal_dir: journal_dir.as_deref(),
            backup_dir: backup_dir.as_deref(),
            apply: apply.as_deref(),
        }),
        Command::Align {
            source,
            partition,
            journal_dir,
            backup_dir,
            apply,
        } => relocate::align(
            &source,
            partition,
            (journal_dir.as_deref(), backup_dir.as_deref()),
            apply.as_deref(),
        ),
        Command::Resize {
            source,
            partition,
            size,
            apply,
        } => relocate::resize(&source, partition, &size, apply.as_deref()),
        Command::ClonePartition {
            source,
            partition,
            to,
            at_sector,
            backup_dir,
            apply,
        } => migrate::clone_partition(&migrate::PartitionRequest {
            source: &source,
            partition,
            to: &to,
            at_sector,
            backup_dir: backup_dir.as_deref(),
            apply: apply.as_deref(),
        }),
        Command::Migrate {
            source,
            to,
            keep_ids,
            new_ids,
            space_after,
            apply,
        } => migrate::migrate(&migrate::MigrateRequest {
            source: &source,
            to: &to,
            keep_ids: if keep_ids {
                Some(true)
            } else if new_ids {
                Some(false)
            } else {
                None
            },
            space_after,
            apply: apply.as_deref(),
        }),
        Command::Format {
            source,
            partition,
            file_system,
            label,
            cluster,
            full,
            apply,
        } => manage::format(&manage::FormatRequest {
            source: &source,
            partition,
            file_system: &file_system,
            label: label.as_deref(),
            cluster: cluster.as_deref(),
            full,
            apply: apply.as_deref(),
        }),
        Command::Wipe {
            source,
            partition,
            random,
            apply,
        } => manage::wipe(&source, partition, random, apply.as_deref()),
        Command::Label {
            source,
            partition,
            label,
        } => manage::label(&source, partition, label.as_deref()),
        Command::Letter {
            source,
            partition,
            letter,
            apply,
        } => manage::letter(&source, partition, &letter, apply.as_deref()),
        Command::Table {
            source,
            action,
            apply,
            backup_dir,
        } => table::table(table::TableRequest {
            source: &source,
            action,
            apply: apply.as_deref(),
            backup_dir: backup_dir.as_deref(),
        }),
        Command::Health { source, json } => health::health(&source, json),
        Command::Surface { source, partition } => health::surface(&source, partition),
        Command::Speed { source } => health::speed(&source),
        Command::Ls {
            source,
            path,
            volume,
            deleted,
            recursive,
        } => files::ls(&source, &path, &volume.choice(), deleted, recursive),
        Command::Cat {
            source,
            path,
            volume,
            stream,
            deleted,
        } => files::cat(&source, &path, &volume.choice(), stream.as_deref(), deleted),
        Command::Scan {
            source,
            full,
            save,
            resume,
        } => scan::scan(&source, full, save.as_deref(), resume.as_deref()),
        Command::Raw {
            source,
            partition,
            offset,
            signatures,
            save,
            resume,
            to,
        } => raw::raw(&raw::RawRequest {
            source: &source,
            partition,
            offset,
            signatures: signatures.as_deref(),
            save: save.as_deref(),
            resume: resume.as_deref(),
            to: to.as_deref(),
        }),
        Command::Signatures { file, write } => raw::signatures(file.as_deref(), write.as_deref()),
        Command::Recover {
            source,
            paths,
            to,
            volume,
            no_deleted,
            no_streams,
            deleted,
        } => scan::recover(&scan::RecoverRequest {
            source: &source,
            paths: &paths,
            to: &to,
            choice: &volume.choice(),
            options: datareco_scan::RecoverOptions {
                deleted: !no_deleted,
                streams: !no_streams,
            },
            prefer_deleted: deleted,
        }),
        Command::Image { source, to, copy } => imaging::image(&copy.request(&source), &to),
        Command::Clone {
            source,
            to,
            erase,
            copy,
        } => imaging::clone(&copy.request(&source), &to, erase.as_deref()),
        Command::Find {
            source,
            text,
            hex,
            ignore_case,
            from,
            count,
        } => structures::find(
            &source,
            text.as_deref(),
            hex.as_deref(),
            ignore_case,
            from,
            count,
        ),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            if matches!(error, CliError::Disk(Error::AccessDenied { .. })) {
                eprintln!("hint: open a terminal with \"Run as administrator\" and try again");
            }
            ExitCode::FAILURE
        }
    }
}

fn disks(json: bool) -> Result<(), CliError> {
    let devices = list_devices()?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&devices).expect("device lists serialize")
        );
        return Ok(());
    }
    print_devices(&devices);
    println!();
    if is_elevated() {
        println!("Running as administrator.");
    } else {
        println!(
            "Not running as administrator: reading disks and volumes needs an elevated terminal."
        );
    }
    Ok(())
}

fn print_devices(devices: &DeviceList) {
    println!("Physical disks");
    if devices.disks.is_empty() {
        println!("  (none found)");
    }
    for disk in &devices.disks {
        let removable = if disk.removable { " (removable)" } else { "" };
        println!(
            "  {:<11} {:>10}  {:<6} {}{removable}",
            format!("disk{}", disk.number),
            format_size(disk.size),
            disk.bus,
            disk.model
        );
    }
    println!();
    println!("Volumes");
    if devices.volumes.is_empty() {
        println!("  (none found)");
    }
    for volume in &devices.volumes {
        let size = volume.size.map_or_else(|| "?".to_owned(), format_size);
        let location = volume
            .extents
            .first()
            .map(|extent| format!("disk{} at {}", extent.disk, format_size(extent.offset)))
            .unwrap_or_else(|| "-".to_owned());
        // Volumes without a drive letter are read by their volume path.
        let (name, path) = match drive_letter(volume) {
            Some(letter) => (letter, String::new()),
            None => ("(no letter)".to_owned(), volume.path.clone()),
        };
        println!(
            "  {name:<11} {size:>10}  {:<6} {:<16} {location:<20} {path}",
            volume.file_system.as_deref().unwrap_or("-"),
            volume.label.as_deref().unwrap_or("-"),
        );
    }
}

/// The volume's drive letter (`C:`), if it has one.
fn drive_letter(volume: &VolumeInfo) -> Option<String> {
    volume
        .mount_points
        .iter()
        .find(|mount| mount.len() == 3 && mount.ends_with(":\\"))
        .map(|mount| mount.trim_end_matches('\\').to_owned())
}

fn read(source: &str, sector: u64, count: u32) -> Result<(), CliError> {
    let spec = SourceSpec::parse(source);
    let disk = Disk::open(&spec)?;
    println!(
        "{}: {} ({} bytes), {} sectors of {} bytes",
        disk.name(),
        format_size(disk.size()),
        disk.size(),
        disk.sector_count(),
        disk.sector_size()
    );
    let (bytes, outcome) = disk.read_sectors(sector, count)?;
    let sector_size = disk.sector_size() as usize;
    for (i, chunk) in bytes.chunks(sector_size).enumerate() {
        let lba = sector + i as u64;
        println!();
        println!("Sector {lba}");
        print!("{}", hexdump(lba * u64::from(disk.sector_size()), chunk));
    }
    for lba in &outcome.unreadable {
        eprintln!("warning: sector {lba} could not be read; its bytes are shown as 00");
    }
    Ok(())
}
