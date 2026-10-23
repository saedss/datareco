//! `partitions` and `find-partitions`.

use std::io::Write;

use datareco_io::{Disk, SourceSpec};
use datareco_partitions::{
    FileSystem, FoundPartition, PartitionKind, PartitionTable, Scan, ScanMode, TableKind,
    read_table,
};

use crate::CliError;
use crate::format::format_size;

/// Bytes each search step examines before printing progress.
const SEARCH_STEP: u64 = 256 << 20;

pub fn show(source: &str) -> Result<(), CliError> {
    let disk = Disk::open(&SourceSpec::parse(source))?;
    let table = read_table(&disk)?;
    print_table(&table);
    Ok(())
}

fn file_system_text(fs: Option<&FileSystem>) -> String {
    match fs {
        None => "-".to_owned(),
        Some(fs) => match &fs.label {
            Some(label) => format!("{} \"{label}\"", fs.name),
            None => fs.name.clone(),
        },
    }
}

fn print_table(table: &PartitionTable) {
    let geometry = format!(
        "{} sectors of {} bytes",
        table.disk_sectors, table.sector_size
    );
    match table.kind {
        TableKind::Mbr => println!(
            "Partition table: MBR (disk signature {}), {geometry}",
            table.disk_id.as_deref().unwrap_or("?")
        ),
        TableKind::Gpt => println!(
            "Partition table: GPT (disk {}), {geometry}",
            table.disk_id.as_deref().unwrap_or("?")
        ),
        TableKind::None => println!("No partition table, {geometry}"),
    }
    if let Some(fs) = &table.whole_disk {
        println!("Whole disk: {}", file_system_text(Some(fs)));
    }
    if !table.partitions.is_empty() {
        println!();
        println!(
            "  {:>3}  {:<8}  {:>12}  {:>12}  {:>10}  {:<28}  File system",
            "#", "Kind", "Start LBA", "Sectors", "Size", "Type"
        );
        for p in &table.partitions {
            let kind = match p.kind {
                PartitionKind::Primary => "primary",
                PartitionKind::Extended => "extended",
                PartitionKind::Logical => "logical",
                PartitionKind::Gpt => "gpt",
            };
            let type_text = match &p.name {
                Some(name) => format!("{} \"{name}\"", p.type_name),
                None => p.type_name.clone(),
            };
            let bootable = if p.bootable { " (bootable)" } else { "" };
            println!(
                "  {:>3}  {kind:<8}  {:>12}  {:>12}  {:>10}  {type_text:<28}  {}{bootable}",
                p.number,
                p.start_lba,
                p.sectors,
                format_size(p.sectors * u64::from(table.sector_size)),
                file_system_text(p.file_system.as_ref())
            );
        }
    }
    if !table.problems.is_empty() {
        println!();
        println!("Problems:");
        for problem in &table.problems {
            let severity = format!("{:?}", problem.severity).to_lowercase();
            println!("  {severity}: {}", problem.message);
        }
    }
}

pub fn find(source: &str, full: bool) -> Result<(), CliError> {
    let disk = Disk::open(&SourceSpec::parse(source))?;
    let table = read_table(&disk)?;
    let mode = if full {
        ScanMode::Full
    } else {
        ScanMode::Quick
    };
    let mut scan = Scan::new(&disk, mode);
    loop {
        let progress = scan.step(&disk, SEARCH_STEP)?;
        eprint!(
            "\rSearching… {} of {} ({} clues)   ",
            format_size(progress.position),
            format_size(progress.total),
            progress.clues
        );
        let _ = std::io::stderr().flush();
        if progress.done {
            eprintln!();
            break;
        }
    }
    let found = scan.results(&disk, Some(&table))?;
    print_found(&found, disk.sector_size(), mode);
    Ok(())
}

fn print_found(found: &[FoundPartition], sector_size: u32, mode: ScanMode) {
    let mode_name = if mode == ScanMode::Full {
        "full"
    } else {
        "quick"
    };
    if found.is_empty() {
        println!("No partitions found ({mode_name} search).");
        if mode == ScanMode::Quick {
            println!(
                "A full search (--full) looks at every sector and finds more, but takes longer."
            );
        }
        return;
    }
    println!("Found {} partition(s) ({mode_name} search):", found.len());
    println!();
    println!(
        "  {:>12}  {:>12}  {:>10}  {:<24}  {:<10}  In table",
        "Start LBA", "Sectors", "Size", "File system", "Confidence"
    );
    for f in found {
        let confidence = format!("{:?}", f.confidence).to_lowercase();
        println!(
            "  {:>12}  {:>12}  {:>10}  {:<24}  {confidence:<10}  {}",
            f.start_lba,
            f.sectors,
            format_size(f.sectors * u64::from(sector_size)),
            file_system_text(f.file_system.as_ref()),
            if f.in_table { "yes" } else { "no" }
        );
        if let Some(type_name) = &f.type_name {
            let name = f
                .name
                .as_ref()
                .map(|n| format!(" \"{n}\""))
                .unwrap_or_default();
            println!("  {:>12}  type: {type_name}{name}", "");
        }
        for clue in &f.evidence {
            println!("  {:>12}  - {clue}", "");
        }
    }
}
