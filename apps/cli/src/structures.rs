//! `templates`, `decode` and `find`: looking inside disks.

use std::path::Path;

use datareco_io::{Disk, SearchProgress, SourceSpec};
use datareco_templates::{Node, TemplateSet, parse_hex};

use crate::CliError;

/// How much each search step reads before checking again.
const SEARCH_STEP: u64 = 64 << 20;

fn load_templates(dir: Option<&Path>) -> TemplateSet {
    let mut set = TemplateSet::builtin();
    if let Some(dir) = dir {
        set.add_dir(dir);
    }
    for problem in set.problems() {
        eprintln!("warning: {problem}");
    }
    set
}

pub fn list(dir: Option<&Path>) -> Result<(), CliError> {
    let set = load_templates(dir);
    let mut group = None;
    for template in set.list() {
        if group.as_ref() != Some(&template.group) {
            println!(
                "{}",
                if template.group.is_empty() {
                    "Other"
                } else {
                    &template.group
                }
            );
            group = Some(template.group.clone());
        }
        println!("  {:<22} {}", template.id, template.name);
    }
    Ok(())
}

pub fn decode(
    source: &str,
    template: &str,
    sector: Option<u64>,
    offset: Option<u64>,
    dir: Option<&Path>,
) -> Result<(), CliError> {
    let set = load_templates(dir);
    let disk = Disk::open(&SourceSpec::parse(source))?;
    let offset = match (sector, offset) {
        (Some(sector), _) => sector
            .checked_mul(u64::from(disk.sector_size()))
            .ok_or_else(|| CliError::Message("sector number too large".to_owned()))?,
        (None, Some(offset)) => offset,
        (None, None) => 0,
    };
    if offset >= disk.size() {
        return Err(CliError::Message(format!(
            "offset {offset} is past the end of {} ({} bytes)",
            disk.name(),
            disk.size()
        )));
    }

    let id = if template == "auto" {
        let sample = read(&disk, offset, 4096)?;
        set.detect(&sample)
            .first()
            .map(|t| t.id.clone())
            .ok_or_else(|| {
                CliError::Message(format!(
                    "no template recognizes the bytes at offset {offset}"
                ))
            })?
    } else {
        template.to_owned()
    };
    let extent = set
        .get(&id)
        .ok_or_else(|| {
            CliError::Message(format!(
                "there is no template named \"{id}\" (see `datareco templates`)"
            ))
        })?
        .extent;
    let data = read(&disk, offset, extent)?;
    let node = set
        .decode(&id, &data, offset)
        .map_err(|e| CliError::Message(e.to_string()))?;

    let sector_size = u64::from(disk.sector_size());
    println!(
        "{} at offset 0x{offset:x} (sector {}{}), {} bytes",
        node.name,
        offset / sector_size,
        if offset % sector_size == 0 {
            ""
        } else {
            ", partway"
        },
        node.size
    );
    print_children(&node, 0);
    Ok(())
}

/// Reads up to `len` bytes at `offset`, fewer at the end of the disk.
fn read(disk: &Disk, offset: u64, len: u64) -> Result<Vec<u8>, CliError> {
    let len = len.min(disk.size().saturating_sub(offset));
    let mut data = vec![
        0;
        usize::try_from(len)
            .map_err(|_| CliError::Message("too much to read".to_owned()))?
    ];
    let outcome = disk.read_at(offset, &mut data)?;
    for lba in outcome.unreadable {
        eprintln!("warning: sector {lba} could not be read; its bytes decode as 00");
    }
    Ok(data)
}

fn print_children(node: &Node, depth: usize) {
    for child in &node.children {
        let marker = if child.problem { "! " } else { "" };
        let value = if child.value.is_empty() {
            String::new()
        } else {
            format!(": {}", child.value)
        };
        let note = child
            .note
            .as_ref()
            .map(|n| format!("  ({n})"))
            .unwrap_or_default();
        println!(
            "{:>14}  {}{marker}{}{value}{note}",
            format!("0x{:x}", child.offset),
            "  ".repeat(depth),
            child.name
        );
        print_children(child, depth + 1);
    }
}

pub fn find(
    source: &str,
    text: Option<&str>,
    hex: Option<&str>,
    ignore_case: bool,
    from: u64,
    count: u32,
) -> Result<(), CliError> {
    let pattern = match (text, hex) {
        (Some(text), _) => text.as_bytes().to_vec(),
        (None, Some(hex)) => parse_hex(hex).map_err(CliError::Message)?,
        (None, None) => return Err(CliError::Message("give --text or --hex".to_owned())),
    };
    if pattern.is_empty() {
        return Err(CliError::Message("nothing to search for".to_owned()));
    }
    let disk = Disk::open(&SourceSpec::parse(source))?;
    let sector_size = u64::from(disk.sector_size());
    let mut position = from;
    let mut found = 0;
    while found < count {
        match disk.find(&pattern, position, SEARCH_STEP, ignore_case)? {
            SearchProgress::Found { offset } => {
                println!(
                    "0x{offset:x}  (byte {offset}, sector {} + {})",
                    offset / sector_size,
                    offset % sector_size
                );
                found += 1;
                position = offset + 1;
            }
            SearchProgress::Continue { next } => position = next,
            SearchProgress::NotFound => break,
        }
    }
    if found == 0 {
        println!("Not found.");
    }
    Ok(())
}
