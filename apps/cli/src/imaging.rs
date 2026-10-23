//! `image` and `clone`: copying a disk to an image file or onto another disk,
//! in passes made for failing drives, with a map file to carry on later.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use datareco_imaging::{Copier, CopyOptions, CopyProgress, Map, Phase};
use datareco_io::{
    CopySource, DeviceList, DiskTarget, FileSource, ImageTarget, SectorSource, SectorTarget,
    SourceSpec, check_disk_target, check_outside_source, list_devices, same_path,
};

use crate::CliError;
use crate::format::format_size;

/// How long each step copies before showing progress.
const STEP: Duration = Duration::from_millis(500);
/// How often the map is saved.
const SAVE_EVERY: Duration = Duration::from_secs(5);

/// What both commands share.
pub struct CopyRequest<'a> {
    pub source: &'a str,
    pub map: Option<&'a Path>,
    pub retries: u32,
    pub skip: bool,
}

pub fn image(request: &CopyRequest, to: &Path) -> Result<(), CliError> {
    let spec = SourceSpec::parse(request.source);
    if let SourceSpec::Image(path) = &spec
        && same_path(path, to)
    {
        return Err(CliError::Message(
            "The image would overwrite the file being copied. Pick another file name.".to_owned(),
        ));
    }
    let map_path = request
        .map
        .map_or_else(|| default_map(to), Path::to_path_buf);
    if same_path(&map_path, to) {
        return Err(CliError::Message(
            "The map file and the image must be different files.".to_owned(),
        ));
    }
    let devices = list_devices().unwrap_or_default();
    refuse_on_source(&spec, &[to, &map_path], &devices)?;

    let mut source = FileSource::open(&spec)?;
    let size = source.size();
    let (map, mut target) = if map_path.exists() {
        let map = load_map(&map_path, size)?;
        if !to.exists() {
            return Err(CliError::Message(format!(
                "The map {} is for a copy into {}, which isn't there. To start again, delete \
                 the map.",
                map_path.display(),
                to.display()
            )));
        }
        (map, ImageTarget::resume(to, size)?)
    } else {
        if to.exists() {
            return Err(CliError::Message(format!(
                "{} already exists. To carry on a copy into it, give its map with --map; to \
                 start again, delete it first.",
                to.display()
            )));
        }
        let target = ImageTarget::create(to, size)?;
        let mut map = Map::new(size);
        map.source = Some(spec.id());
        map.target = Some(SourceSpec::Image(to.to_path_buf()).id());
        (map, target)
    };
    let options = CopyOptions {
        skip: request.skip,
        retries: request.retries,
        fill_bad: false,
        ..CopyOptions::default()
    };
    let progress = run(map, &map_path, &mut source, &mut target, options)?;
    println!();
    summary(&progress, &map_path);
    if progress.totals.bad > 0 {
        println!("They read as zeros in the image.");
    }
    Ok(())
}

pub fn clone(request: &CopyRequest, to: &str, erase: Option<&str>) -> Result<(), CliError> {
    let spec = SourceSpec::parse(request.source);
    let SourceSpec::PhysicalDisk(number) = SourceSpec::parse(to) else {
        return Err(CliError::Message(format!(
            "--to must be a disk, like disk2 (`datareco disks` lists them). To copy into an \
             image file, use `datareco image {} --to {to}`.",
            request.source
        )));
    };
    let map_path = request.map.map_or_else(
        || PathBuf::from(default_clone_map(&spec, number)),
        Path::to_path_buf,
    );
    let devices = list_devices()?;
    refuse_on_source(&spec, &[&map_path], &devices)?;

    let mut source = FileSource::open(&spec)?;
    let size = source.size();
    let mut keep: Vec<&Path> = vec![&map_path];
    if let SourceSpec::Image(path) = &spec {
        keep.push(path);
    }
    let copy_source = CopySource {
        spec: &spec,
        size,
        sector_size: source.sector_size(),
    };
    let check = check_disk_target(number, &copy_source, &keep, &devices)?;
    println!(
        "Copying {} ({}) onto {}: {}, {}.",
        spec.display_name(),
        format_size(size),
        check.name,
        check.model,
        format_size(check.size)
    );
    if check.volumes.is_empty() {
        println!("{} has no volumes Windows knows about.", check.name);
    } else {
        println!(
            "Its volumes will be closed and overwritten: {}.",
            check.volumes.join(", ")
        );
    }
    for warning in &check.warnings {
        println!("note: {warning}");
    }
    let Some(typed) = erase else {
        return Err(CliError::Message(format!(
            "Everything on {} will be erased. To go ahead, run the same command with --erase {}.",
            check.name, check.confirmation
        )));
    };
    if !typed.trim().eq_ignore_ascii_case(&check.confirmation) {
        return Err(CliError::Message(format!(
            "--erase must name the disk being written to: --erase {}.",
            check.confirmation
        )));
    }
    let map = if map_path.exists() {
        let map = load_map(&map_path, size)?;
        let target_id = SourceSpec::PhysicalDisk(number).id();
        if map.target.as_ref().is_some_and(|t| *t != target_id) {
            eprintln!(
                "note: this map was for a copy onto {}",
                map.target.as_deref().unwrap_or_default()
            );
        }
        map
    } else {
        let mut map = Map::new(size);
        map.source = Some(spec.id());
        map.target = Some(SourceSpec::PhysicalDisk(number).id());
        map
    };
    let mut target = DiskTarget::open(&check, typed)?;
    let options = CopyOptions {
        skip: request.skip,
        retries: request.retries,
        fill_bad: true,
        ..CopyOptions::default()
    };
    let progress = run(map, &map_path, &mut source, &mut target, options)?;
    drop(target);
    println!();
    summary(&progress, &map_path);
    if progress.totals.bad > 0 {
        println!("They were filled with zeros on {}.", check.name);
    }
    Ok(())
}

/// Copies until done, showing progress and saving the map as it goes.
fn run(
    map: Map,
    map_path: &Path,
    source: &mut dyn SectorSource,
    target: &mut dyn SectorTarget,
    options: CopyOptions,
) -> Result<CopyProgress, CliError> {
    let sector_size = source.sector_size();
    let mut copier =
        Copier::new(map, sector_size, options).map_err(|e| CliError::Message(e.to_string()))?;
    let start = copier.progress();
    if start.finished {
        println!("The copy in {} is already finished.", map_path.display());
        return Ok(start);
    }
    if start.phase == Phase::Retrying && start.totals.untried == 0 && start.pass == 1 {
        println!(
            "Trying the {} that couldn't be read again.",
            format_size(start.totals.bad)
        );
    } else if start.totals.copied > 0 || start.totals.bad > 0 {
        println!(
            "Carrying on the copy in {}: {} of {} copied so far.",
            map_path.display(),
            format_size(start.totals.copied),
            format_size(start.size)
        );
    }
    save_map(&copier, map_path, target)?;
    println!(
        "The map is {}. Stop any time with Ctrl+C; run the same command again to carry on.",
        map_path.display()
    );
    let started = Instant::now();
    let mut saved_at = Instant::now();
    let mut phase: Option<(Phase, u32)> = None;
    loop {
        let progress = match copier.step(source, target, STEP) {
            Ok(progress) => progress,
            Err(error) => {
                eprintln!();
                let _ = save_map(&copier, map_path, target);
                return Err(CliError::Message(error.to_string()));
            }
        };
        if phase != Some((progress.phase, progress.pass)) && !progress.finished {
            if phase.is_some() {
                eprintln!();
            }
            eprintln!("{}…", progress.description);
            phase = Some((progress.phase, progress.pass));
        }
        let rate = (progress.totals.copied - start.totals.copied)
            .checked_div(started.elapsed().as_secs())
            .map_or_else(String::new, |rate| format!(", {}/s", format_size(rate)));
        eprint!(
            "\r  {} of {} copied ({}%), {} unreadable{rate}        ",
            format_size(progress.totals.copied),
            format_size(progress.size),
            percent(progress.totals.copied, progress.size),
            format_size(progress.totals.bad)
        );
        if progress.finished || saved_at.elapsed() >= SAVE_EVERY {
            save_map(&copier, map_path, target)?;
            saved_at = Instant::now();
        }
        if progress.finished {
            eprintln!();
            return Ok(progress);
        }
    }
}

fn save_map(copier: &Copier, path: &Path, target: &mut dyn SectorTarget) -> Result<(), CliError> {
    // What the map says was copied must be stored first.
    target
        .flush()
        .map_err(|e| CliError::Message(format!("Writing to the target failed: {e}")))?;
    copier
        .map()
        .save(path)
        .map_err(|e| CliError::Message(format!("Saving the map failed: {e}")))
}

fn summary(progress: &CopyProgress, map_path: &Path) {
    let totals = progress.totals;
    if totals.bad == 0 {
        println!("Copied all {}.", format_size(progress.size));
        return;
    }
    println!(
        "Copied {} of {}. {} in {} couldn't be read; the map {} lists where.",
        format_size(totals.copied),
        format_size(progress.size),
        format_size(totals.bad),
        places(totals.bad_areas),
        map_path.display()
    );
}

fn load_map(path: &Path, size: u64) -> Result<Map, CliError> {
    let map = Map::load(path).map_err(|e| CliError::Message(e.to_string()))?;
    if map.size() != size {
        return Err(CliError::Message(format!(
            "The map {} is for a source of {}, but this one is {}. Is it the right map?",
            path.display(),
            exact_size(map.size()),
            exact_size(size)
        )));
    }
    Ok(map)
}

/// Refuses image and map files on the disk being copied.
fn refuse_on_source(
    spec: &SourceSpec,
    paths: &[&Path],
    devices: &DeviceList,
) -> Result<(), CliError> {
    for path in paths {
        check_outside_source(spec, path, devices)?;
    }
    Ok(())
}

/// `disk1.img` → `disk1.map`.
fn default_map(image: &Path) -> PathBuf {
    let map = image.with_extension("map");
    if map == image {
        let mut name = image.as_os_str().to_owned();
        name.push(".map");
        PathBuf::from(name)
    } else {
        map
    }
}

/// `disk1-to-disk2.map`, in the current folder.
fn default_clone_map(source: &SourceSpec, number: u32) -> String {
    let name = match source {
        SourceSpec::PhysicalDisk(n) => format!("disk{n}"),
        SourceSpec::Volume(path) => path.clone(),
        SourceSpec::Image(path) => path
            .file_stem()
            .map_or_else(|| "image".to_owned(), |s| s.to_string_lossy().into_owned()),
    };
    let name: String = name
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    format!("{name}-to-disk{number}.map")
}

/// `293.1 KiB (300100 bytes)`.
fn exact_size(bytes: u64) -> String {
    if bytes < 1024 {
        format_size(bytes)
    } else {
        format!("{} ({bytes} bytes)", format_size(bytes))
    }
}

fn places(count: u64) -> String {
    if count == 1 {
        "one place".to_owned()
    } else {
        format!("{count} places")
    }
}

fn percent(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "100".to_owned();
    }
    let tenths = u128::from(part) * 1000 / u128::from(whole);
    format!("{}.{}", tenths / 10, tenths % 10)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_map_names() {
        assert_eq!(default_map(Path::new("disk1.img")), Path::new("disk1.map"));
        assert_eq!(default_map(Path::new("disk1")), Path::new("disk1.map"));
        assert_eq!(default_map(Path::new("x.map")), Path::new("x.map.map"));
        assert_eq!(
            default_clone_map(&SourceSpec::PhysicalDisk(1), 2),
            "disk1-to-disk2.map"
        );
        assert_eq!(
            default_clone_map(&SourceSpec::Volume("E:".to_owned()), 2),
            "E-to-disk2.map"
        );
        assert_eq!(
            default_clone_map(&SourceSpec::Image(PathBuf::from("old disk.img")), 3),
            "olddisk-to-disk3.map"
        );
    }

    #[test]
    fn percentages() {
        assert_eq!(percent(0, 10), "0.0");
        assert_eq!(percent(1, 3), "33.3");
        assert_eq!(percent(10, 10), "100.0");
        assert_eq!(percent(0, 0), "100");
    }
}
