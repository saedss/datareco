//! Copying files and folders out of a volume to a folder on another disk.
//!
//! Each recovery writes, next to what it saves, a log of every item (saved,
//! incomplete or failed, and why) and a SHA-256 checksum list in
//! `sha256sum` format. Times are set as the file system recorded them.

use std::collections::HashSet;
use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use datareco_fs::{Entry, EntryId, FileSystem};
use datareco_io::{SourceSpec, plain_path};
use sha2::{Digest, Sha256};

/// Bytes copied per read.
const CHUNK: usize = 1 << 20;
/// Folders deeper than this aren't followed (damaged volumes can have loops).
const MAX_DEPTH: usize = 128;

#[derive(Debug, thiserror::Error)]
pub enum RecoverError {
    /// The destination can't be used (it's on the source disk, say).
    #[error("{0}")]
    Destination(String),
    #[error("Writing to the destination failed: {0}")]
    Io(#[from] io::Error),
    #[error("The recovery was stopped.")]
    Stopped,
}

impl RecoverError {
    pub fn kind(&self) -> &'static str {
        match self {
            RecoverError::Destination(_) => "destination",
            RecoverError::Io(_) => "io",
            RecoverError::Stopped => "stopped",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RecoverOptions {
    /// Also recover deleted files and folders found inside the folders
    /// being recovered. (Items picked directly are always recovered.)
    pub deleted: bool,
    /// Also save NTFS alternate data streams.
    pub streams: bool,
}

impl Default for RecoverOptions {
    fn default() -> Self {
        Self {
            deleted: true,
            streams: true,
        }
    }
}

/// How a recovery is going.
#[derive(Clone, Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoverProgress {
    pub files: u64,
    pub folders: u64,
    pub bytes: u64,
    /// Files saved only in part, because reading them failed partway.
    pub incomplete: u64,
    pub failed: u64,
    /// The item being saved, relative to the destination.
    pub current: String,
}

/// Whether to go on; the progress callback returns it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    Continue,
    Stop,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoverSummary {
    pub progress: RecoverProgress,
    pub destination: PathBuf,
    pub log: PathBuf,
    pub checksums: PathBuf,
}

/// Saves `items` (and everything in the folders among them) from `fs` into
/// `destination`, which is created if needed. `describe` names the source in
/// the log. Problems with single items are logged and counted, not fatal.
pub fn recover(
    fs: &dyn FileSystem,
    describe: &str,
    items: &[Entry],
    destination: &Path,
    options: RecoverOptions,
    on_progress: &mut dyn FnMut(&RecoverProgress) -> Control,
) -> Result<RecoverSummary, RecoverError> {
    fs::create_dir_all(destination)?;
    // The long, absolute form (dialogs sometimes hand over 8.3 short names).
    let full = fs::canonicalize(destination).map_or_else(|_| destination.to_path_buf(), plain_path);
    let destination = full.as_path();
    let stamp = timestamp(SystemTime::now(), true);
    let log_path = unique(destination, &format!("DataReco recovery {stamp}"), ".log");
    let sums_path = unique(
        destination,
        &format!("DataReco recovery {stamp} SHA-256"),
        ".txt",
    );
    let mut run = Run {
        fs,
        options,
        log: BufWriter::new(File::create(&log_path)?),
        sums: BufWriter::new(File::create(&sums_path)?),
        progress: RecoverProgress::default(),
        on_progress,
        visited: HashSet::new(),
        reserved: HashSet::from([log_path.clone(), sums_path.clone()]),
    };
    writeln!(
        run.log,
        "DataReco {} recovery log",
        env!("CARGO_PKG_VERSION")
    )?;
    writeln!(run.log, "Source: {describe}")?;
    writeln!(run.log, "Destination: {}", destination.display())?;
    writeln!(run.log, "Started: {}", timestamp(SystemTime::now(), false))?;
    writeln!(run.log)?;

    let result = items
        .iter()
        .try_for_each(|item| run.item(item, destination, Path::new(""), 0, true));
    let p = &run.progress;
    writeln!(run.log)?;
    writeln!(
        run.log,
        "{}: {} files and {} folders, {} bytes. {} incomplete, {} failed.",
        if result.is_err() {
            "Stopped"
        } else {
            "Finished"
        },
        p.files,
        p.folders,
        p.bytes,
        p.incomplete,
        p.failed
    )?;
    writeln!(run.log, "At: {}", timestamp(SystemTime::now(), false))?;
    run.log.flush()?;
    run.sums.flush()?;
    result?;
    Ok(RecoverSummary {
        progress: run.progress,
        destination: destination.to_owned(),
        log: log_path,
        checksums: sums_path,
    })
}

struct Run<'a> {
    fs: &'a dyn FileSystem,
    options: RecoverOptions,
    log: BufWriter<File>,
    sums: BufWriter<File>,
    progress: RecoverProgress,
    on_progress: &'a mut dyn FnMut(&RecoverProgress) -> Control,
    visited: HashSet<EntryId>,
    /// Paths taken by this run (the log, the checksums, what was saved).
    reserved: HashSet<PathBuf>,
}

impl Run<'_> {
    fn report(&mut self) -> Result<(), RecoverError> {
        match (self.on_progress)(&self.progress) {
            Control::Continue => Ok(()),
            Control::Stop => Err(RecoverError::Stopped),
        }
    }

    /// Saves one entry into `folder`; `relative` is `folder` relative to the
    /// destination, for the log.
    fn item(
        &mut self,
        entry: &Entry,
        folder: &Path,
        relative: &Path,
        depth: usize,
        picked: bool,
    ) -> Result<(), RecoverError> {
        if entry.deleted && !picked && !self.options.deleted {
            return Ok(());
        }
        let path = self.free_path(folder, &safe_name(&entry.name));
        let name = path.file_name().map(PathBuf::from).unwrap_or_default();
        let shown = relative.join(&name);
        self.progress.current = slashes(&shown);
        self.report()?;
        let deleted = if entry.deleted { " (deleted)" } else { "" };
        if entry.is_dir {
            if depth >= MAX_DEPTH || !self.visited.insert(entry.id.clone()) {
                return Ok(()); // a loop in a damaged volume
            }
            if let Err(error) = fs::create_dir(&path) {
                self.progress.failed += 1;
                writeln!(
                    self.log,
                    "FAILED      {}/{deleted}: {error}",
                    slashes(&shown)
                )?;
                return Ok(());
            }
            self.progress.folders += 1;
            match self.fs.list(&entry.id) {
                Ok(children) => {
                    writeln!(self.log, "folder      {}/{deleted}", slashes(&shown))?;
                    for child in &children {
                        self.item(child, &path, &shown, depth + 1, false)?;
                    }
                }
                Err(error) => {
                    self.progress.failed += 1;
                    writeln!(
                        self.log,
                        "FAILED      {}/{deleted}: its contents can't be listed: {error}",
                        slashes(&shown)
                    )?;
                }
            }
            set_times(&path, entry, true);
            return Ok(());
        }
        self.file(entry, None, &path, &shown, deleted)?;
        if self.options.streams {
            for stream in &entry.streams {
                let stream_path = self.stream_path(&path, &stream.name);
                let stream_shown = relative.join(stream_path.file_name().unwrap_or_default());
                self.file(
                    entry,
                    Some(&stream.name),
                    &stream_path,
                    &stream_shown,
                    deleted,
                )?;
            }
        }
        Ok(())
    }

    /// Copies a file's data (or one of its streams) to `path`, hashing it.
    fn file(
        &mut self,
        entry: &Entry,
        stream: Option<&str>,
        path: &Path,
        shown: &Path,
        deleted: &str,
    ) -> Result<(), RecoverError> {
        let mut out = match File::create(path) {
            Ok(file) => file,
            Err(error) => {
                self.progress.failed += 1;
                writeln!(self.log, "FAILED      {}{deleted}: {error}", slashes(shown))?;
                return Ok(());
            }
        };
        let mut hasher = Sha256::new();
        let mut buf = vec![0; CHUNK];
        let mut offset = 0u64;
        let mut problem = None;
        loop {
            match self.fs.read(&entry.id, stream, offset, &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    out.write_all(&buf[..n])?;
                    hasher.update(&buf[..n]);
                    offset += n as u64;
                    self.progress.bytes += n as u64;
                    if offset.is_multiple_of(16 * CHUNK as u64) {
                        self.report()?;
                    }
                }
                Err(error) => {
                    problem = Some(error.to_string());
                    break;
                }
            }
        }
        drop(out);
        if stream.is_none() {
            set_times(path, entry, false);
        }
        let hash: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        writeln!(self.sums, "{hash}  {}", slashes(shown))?;
        match problem {
            None => {
                self.progress.files += 1;
                writeln!(
                    self.log,
                    "ok          {}{deleted}, {offset} bytes",
                    slashes(shown)
                )?;
            }
            Some(error) if offset > 0 => {
                self.progress.incomplete += 1;
                writeln!(
                    self.log,
                    "INCOMPLETE  {}{deleted}: {offset} of its bytes saved, then: {error}",
                    slashes(shown)
                )?;
            }
            Some(error) => {
                self.progress.failed += 1;
                writeln!(self.log, "FAILED      {}{deleted}: {error}", slashes(shown))?;
            }
        }
        Ok(())
    }

    /// `name` in `folder`, or "name (2).ext" and so on if that's taken.
    fn free_path(&mut self, folder: &Path, name: &str) -> PathBuf {
        let (stem, extension) = match name.rsplit_once('.') {
            Some((stem, extension)) if !stem.is_empty() => (stem, format!(".{extension}")),
            _ => (name, String::new()),
        };
        let mut path = folder.join(name);
        let mut n = 2;
        while path.exists() || self.reserved.contains(&path) {
            path = folder.join(format!("{stem} ({n}){extension}"));
            n += 1;
        }
        self.reserved.insert(path.clone());
        path
    }

    /// Where to save a stream: as a stream again (`file:name`) where the
    /// destination can hold one, otherwise next to the file.
    fn stream_path(&mut self, file: &Path, stream: &str) -> PathBuf {
        let name = file.file_name().unwrap_or_default().to_string_lossy();
        let as_stream = file.with_file_name(format!("{name}:{}", safe_name(stream)));
        if cfg!(windows) && File::create(&as_stream).is_ok() {
            return as_stream;
        }
        let folder = file.parent().unwrap_or(Path::new(""));
        self.free_path(folder, &format!("{name} [{}]", safe_name(stream)))
    }
}

/// NTFS's own files ($MFT, $LogFile and the like): left out when a whole
/// volume is recovered (picked one by one, they're saved like any file).
pub fn is_system_file(entry: &Entry) -> bool {
    matches!(entry.id, EntryId::Record { number } if number < 16) && entry.name.starts_with('$')
}

/// A name Windows accepts: no reserved characters or device names, no
/// trailing dots or spaces.
pub(crate) fn safe_name(name: &str) -> String {
    let mut safe: String = name
        .chars()
        .map(|c| {
            if c < ' ' || "<>:\"/\\|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    while safe.ends_with(['.', ' ']) {
        safe.pop();
    }
    if safe.is_empty() {
        return "_".to_owned();
    }
    let stem = safe
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        safe.insert(0, '_');
    }
    safe
}

fn slashes(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn system_time(seconds: i64) -> Option<SystemTime> {
    if seconds >= 0 {
        UNIX_EPOCH.checked_add(Duration::from_secs(seconds.unsigned_abs()))
    } else {
        UNIX_EPOCH.checked_sub(Duration::from_secs(seconds.unsigned_abs()))
    }
}

/// Sets a saved file's or folder's times to the ones the volume recorded.
/// Failing to is no reason to fail the recovery.
fn set_times(path: &Path, entry: &Entry, folder: bool) {
    let mut times = FileTimes::new();
    if let Some(t) = entry.modified.and_then(system_time) {
        times = times.set_modified(t);
    }
    if let Some(t) = entry.accessed.and_then(system_time) {
        times = times.set_accessed(t);
    }
    #[cfg(windows)]
    if let Some(t) = entry.created.and_then(system_time) {
        use std::os::windows::fs::FileTimesExt;
        times = times.set_created(t);
    }
    let mut open = OpenOptions::new();
    open.write(true);
    #[cfg(windows)]
    if folder {
        use std::os::windows::fs::OpenOptionsExt;
        // Folders open only with backup semantics; writing attributes is enough.
        const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        open.access_mode(FILE_WRITE_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
    }
    #[cfg(not(windows))]
    if folder {
        open.write(false).read(true);
    }
    if let Ok(file) = open.open(path) {
        let _ = file.set_times(times);
    }
}

/// `stem` + `extension` in `folder`, numbered if that's taken.
fn unique(folder: &Path, stem: &str, extension: &str) -> PathBuf {
    let mut path = folder.join(format!("{stem}{extension}"));
    let mut n = 2;
    while path.exists() {
        path = folder.join(format!("{stem} ({n}){extension}"));
        n += 1;
    }
    path
}

/// `2026-10-23 15-30-12` (for file names) or `2026-10-23 15:30:12 UTC`.
fn timestamp(time: SystemTime, for_file_name: bool) -> String {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    let t = seconds.rem_euclid(86_400);
    let (h, m, s) = (t / 3600, t % 3600 / 60, t % 60);
    if for_file_name {
        format!("{year:04}-{month:02}-{day:02} {h:02}-{m:02}-{s:02}")
    } else {
        format!("{year:04}-{month:02}-{day:02} {h:02}:{m:02}:{s:02} UTC")
    }
}

/// Days since 1970-01-01 as (year, month, day). Howard Hinnant's algorithm.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

/// Refuses a destination on the disk being recovered from: saving there
/// could overwrite the very data being recovered. Image files can be
/// recovered anywhere.
pub fn check_destination(source: &SourceSpec, destination: &Path) -> Result<(), RecoverError> {
    if matches!(source, SourceSpec::Image(_)) {
        return Ok(());
    }
    let Ok(devices) = datareco_io::list_devices() else {
        return Ok(());
    };
    let source_disks = devices.disks_of_source(source);
    let destination_disks = devices.disks_of_path(destination);
    if source_disks.iter().any(|d| destination_disks.contains(d)) {
        return Err(RecoverError::Destination(format!(
            "{} is on the disk you're recovering from. Saving there could overwrite the files \
             you want back: pick a folder on another disk.",
            destination.display()
        )));
    }
    Ok(())
}
