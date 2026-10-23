//! `datareco` command-line tool. Uses the same engine crates as the desktop app.

mod format;
mod structures;

use std::fmt;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use datareco_io::{DeviceList, Disk, Error, SourceSpec, VolumeInfo, is_elevated, list_devices};

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

/// DataReco: disk editor and data recovery tool. Never writes to disks.
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

fn main() -> ExitCode {
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
