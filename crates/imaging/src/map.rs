//! The map file: which parts of the source were copied, which failed and
//! which weren't tried yet. It is written in GNU ddrescue's mapfile format,
//! so either program can carry on a copy the other started.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::Path;

use serde::Serialize;

/// What is known about an area of the source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Area {
    /// Not read yet (`?`).
    Untried,
    /// A block that failed to read, whose edges haven't been read sector by
    /// sector yet (`*`).
    Untrimmed,
    /// The middle of a failed block, not read sector by sector yet (`/`).
    Unscraped,
    /// Sectors that failed to read on their own (`-`).
    Bad,
    /// Copied (`+`).
    Copied,
}

impl Area {
    pub fn symbol(self) -> char {
        match self {
            Area::Untried => '?',
            Area::Untrimmed => '*',
            Area::Unscraped => '/',
            Area::Bad => '-',
            Area::Copied => '+',
        }
    }

    pub fn from_symbol(symbol: char) -> Option<Area> {
        Some(match symbol {
            '?' => Area::Untried,
            '*' => Area::Untrimmed,
            '/' => Area::Unscraped,
            '-' => Area::Bad,
            '+' => Area::Copied,
            _ => return None,
        })
    }

    /// How much an area matters when many share one square of a picture of
    /// the map: bad sectors show over everything else.
    fn weight(self) -> u8 {
        match self {
            Area::Copied => 0,
            Area::Untried => 1,
            Area::Untrimmed | Area::Unscraped => 2,
            Area::Bad => 3,
        }
    }
}

/// What a copy is doing, in the order it does it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Phase {
    /// Reading blocks: forwards, then backwards, then whatever is left.
    Copying,
    /// Reading failed blocks sector by sector from their edges inwards.
    Trimming,
    /// Reading the rest of failed blocks sector by sector.
    Scraping,
    /// Reading bad sectors again.
    Retrying,
    /// Writing zeros over bad sectors on the target.
    Filling,
    Finished,
}

impl Phase {
    fn symbol(self) -> char {
        match self {
            Phase::Copying => '?',
            Phase::Trimming => '*',
            Phase::Scraping => '/',
            Phase::Retrying => '-',
            Phase::Filling => 'F',
            Phase::Finished => '+',
        }
    }

    fn from_symbol(symbol: char) -> Option<Phase> {
        Some(match symbol {
            '?' => Phase::Copying,
            '*' => Phase::Trimming,
            '/' => Phase::Scraping,
            '-' => Phase::Retrying,
            'F' => Phase::Filling,
            '+' => Phase::Finished,
            _ => return None,
        })
    }
}

/// How many bytes are in each state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    pub copied: u64,
    pub untried: u64,
    pub untrimmed: u64,
    pub unscraped: u64,
    pub bad: u64,
    /// Separate stretches of bad sectors.
    pub bad_areas: u64,
}

/// The state of every byte of the source, and where the copy is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Map {
    size: u64,
    /// Start → (end, area). The areas cover `0..size` without gaps, and
    /// neighbours always differ.
    areas: BTreeMap<u64, (u64, Area)>,
    /// Where the copy is in its current pass.
    pub position: u64,
    pub phase: Phase,
    /// The pass within the phase, from 1.
    pub pass: u32,
    /// What was copied, as a source id (`disk:1`), when DataReco made the map.
    pub source: Option<String>,
    /// Where it was copied to (`image:D:\disk1.img`, `disk:2`).
    pub target: Option<String>,
}

/// A map file that couldn't be read.
#[derive(Debug, thiserror::Error)]
pub enum MapError {
    #[error("{path}: {source}")]
    Io {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("line {line} of the map file: {message}")]
    Parse { line: usize, message: String },
}

impl Map {
    /// A map of a source `size` bytes long, none of it tried yet.
    pub fn new(size: u64) -> Map {
        let mut areas = BTreeMap::new();
        if size > 0 {
            areas.insert(0, (size, Area::Untried));
        }
        Map {
            size,
            areas,
            position: 0,
            phase: Phase::Copying,
            pass: 1,
            source: None,
            target: None,
        }
    }

    /// The size of the source the map is for.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Records what is known about `start..end` (clamped to the source).
    pub fn set(&mut self, start: u64, end: u64, area: Area) {
        let end = end.min(self.size);
        if start >= end {
            return;
        }
        self.split(start);
        self.split(end);
        let inside: Vec<u64> = self.areas.range(start..end).map(|(&k, _)| k).collect();
        for key in inside {
            self.areas.remove(&key);
        }
        let mut start = start;
        let mut end = end;
        if let Some((&before, &(before_end, before_area))) = self.areas.range(..start).next_back()
            && before_end == start
            && before_area == area
        {
            self.areas.remove(&before);
            start = before;
        }
        if let Some(&(after_end, after_area)) = self.areas.get(&end)
            && after_area == area
        {
            self.areas.remove(&end);
            end = after_end;
        }
        self.areas.insert(start, (end, area));
    }

    /// Splits the area holding `at` in two there.
    fn split(&mut self, at: u64) {
        if let Some((&start, &(end, area))) = self.areas.range(..=at).next_back()
            && start < at
            && at < end
        {
            self.areas.insert(start, (at, area));
            self.areas.insert(at, (end, area));
        }
    }

    /// The area holding byte `at`: (start, end, area).
    pub fn area_at(&self, at: u64) -> Option<(u64, u64, Area)> {
        self.areas
            .range(..=at)
            .next_back()
            .filter(|(_, (end, _))| at < *end)
            .map(|(&start, &(end, area))| (start, end, area))
    }

    /// The first stretch in one of `wanted` at or after `from`, starting no
    /// earlier than `from`.
    pub fn next(&self, from: u64, wanted: &[Area]) -> Option<(u64, u64)> {
        let first = self.area_at(from).map_or(from, |(start, _, _)| start);
        self.areas
            .range(first..)
            .find(|(_, (end, area))| *end > from && wanted.contains(area))
            .map(|(&start, &(end, _))| (start.max(from), end))
    }

    /// The last stretch in one of `wanted` before `before`, ending no later
    /// than `before`.
    pub fn previous(&self, before: u64, wanted: &[Area]) -> Option<(u64, u64)> {
        self.areas
            .range(..before)
            .rev()
            .find(|(_, (_, area))| wanted.contains(area))
            .map(|(&start, &(end, _))| (start, end.min(before)))
    }

    /// Every area in order: (start, end, area).
    pub fn areas(&self) -> impl Iterator<Item = (u64, u64, Area)> + '_ {
        self.areas
            .iter()
            .map(|(&start, &(end, area))| (start, end, area))
    }

    pub fn totals(&self) -> Totals {
        let mut totals = Totals::default();
        for (start, end, area) in self.areas() {
            let length = end - start;
            match area {
                Area::Copied => totals.copied += length,
                Area::Untried => totals.untried += length,
                Area::Untrimmed => totals.untrimmed += length,
                Area::Unscraped => totals.unscraped += length,
                Area::Bad => {
                    totals.bad += length;
                    totals.bad_areas += 1;
                }
            }
        }
        totals
    }

    /// A picture of the map in `count` squares, one symbol each (see
    /// [`Area::symbol`]): the most worrying state in each square.
    pub fn picture(&self, count: usize) -> String {
        if self.size == 0 || count == 0 {
            return String::new();
        }
        let count64 = count as u64;
        let square = |offset: u64| -> usize {
            let index = u128::from(offset) * u128::from(count64) / u128::from(self.size);
            usize::try_from(index).unwrap_or(count - 1).min(count - 1)
        };
        let mut squares: Vec<Option<Area>> = vec![None; count];
        for (start, end, area) in self.areas() {
            for slot in &mut squares[square(start)..=square(end - 1)] {
                if slot.is_none_or(|old| area.weight() > old.weight()) {
                    *slot = Some(area);
                }
            }
        }
        squares
            .into_iter()
            .map(|area| area.unwrap_or(Area::Untried).symbol())
            .collect()
    }

    /// The map in ddrescue's mapfile format.
    pub fn to_text(&self) -> String {
        let mut text = format!(
            "# Mapfile. Created by DataReco {}\n",
            env!("CARGO_PKG_VERSION")
        );
        if let Some(source) = &self.source {
            let _ = writeln!(text, "# DataReco source: {source}");
        }
        if let Some(target) = &self.target {
            let _ = writeln!(text, "# DataReco target: {target}");
        }
        text.push_str("# current_pos  current_status  current_pass\n");
        let _ = writeln!(
            text,
            "0x{:08X}     {}               {}",
            self.position,
            self.phase.symbol(),
            self.pass
        );
        text.push_str("#      pos        size  status\n");
        for (start, end, area) in self.areas() {
            let _ = writeln!(
                text,
                "0x{start:08X}  0x{:08X}  {}",
                end - start,
                area.symbol()
            );
        }
        text
    }

    /// Reads a map in ddrescue's mapfile format (made by DataReco or ddrescue).
    pub fn parse(text: &str) -> Result<Map, MapError> {
        let mut map = Map::new(0);
        let mut status_seen = false;
        for (index, line) in text.lines().enumerate() {
            let line_number = index + 1;
            let error = |message: &str| MapError::Parse {
                line: line_number,
                message: message.to_owned(),
            };
            let line = line.trim();
            if let Some(comment) = line.strip_prefix('#') {
                let comment = comment.trim();
                if let Some(source) = comment.strip_prefix("DataReco source:") {
                    map.source = Some(source.trim().to_owned());
                } else if let Some(target) = comment.strip_prefix("DataReco target:") {
                    map.target = Some(target.trim().to_owned());
                }
                continue;
            }
            if line.is_empty() {
                continue;
            }
            let fields: Vec<&str> = line.split_whitespace().collect();
            if !status_seen {
                status_seen = true;
                let (Some(position), Some(phase)) = (fields.first(), fields.get(1)) else {
                    return Err(error("expected where the copy is and what it's doing"));
                };
                map.position = number(position).ok_or_else(|| error("not a number"))?;
                map.phase = single(phase)
                    .and_then(Phase::from_symbol)
                    .ok_or_else(|| error("unknown copy status"))?;
                map.pass = match fields.get(2) {
                    Some(pass) => number(pass)
                        .and_then(|p| u32::try_from(p).ok())
                        .ok_or_else(|| error("not a pass number"))?,
                    None => 1,
                };
                continue;
            }
            let [start, length, status] = fields[..] else {
                return Err(error("expected a position, a size and a status"));
            };
            let start = number(start).ok_or_else(|| error("not a number"))?;
            let length = number(length).ok_or_else(|| error("not a number"))?;
            let area = single(status)
                .and_then(Area::from_symbol)
                .ok_or_else(|| error("unknown area status"))?;
            if start != map.size {
                return Err(error(if map.size == 0 {
                    "the map doesn't start at the start of the source"
                } else {
                    "areas must follow each other without gaps"
                }));
            }
            if length == 0 {
                return Err(error("an area can't be empty"));
            }
            let end = start
                .checked_add(length)
                .ok_or_else(|| error("the area ends past the largest possible size"))?;
            map.size = end;
            map.push(start, end, area);
        }
        if !status_seen {
            return Err(MapError::Parse {
                line: text.lines().count(),
                message: "this isn't a map file: it says nothing about a copy".to_owned(),
            });
        }
        map.pass = map.pass.max(1);
        map.position = map.position.min(map.size);
        Ok(map)
    }

    /// Adds an area at the end, merging it with the last one if they match.
    fn push(&mut self, start: u64, end: u64, area: Area) {
        if let Some(mut last) = self.areas.last_entry()
            && last.get().0 == start
            && last.get().1 == area
        {
            last.get_mut().0 = end;
            return;
        }
        self.areas.insert(start, (end, area));
    }

    pub fn load(path: &Path) -> Result<Map, MapError> {
        let text = fs::read_to_string(path).map_err(|source| MapError::Io {
            path: path.display().to_string(),
            source,
        })?;
        Map::parse(&text)
    }

    /// Saves the map, replacing the file only once the new one is complete,
    /// so a crash never leaves half a map.
    pub fn save(&self, path: &Path) -> Result<(), MapError> {
        let io_error = |source| MapError::Io {
            path: path.display().to_string(),
            source,
        };
        let mut temporary = path.as_os_str().to_owned();
        temporary.push(".new");
        let temporary = Path::new(&temporary);
        fs::write(temporary, self.to_text()).map_err(io_error)?;
        fs::rename(temporary, path).map_err(io_error)
    }
}

/// `0x1F`, or `31`.
fn number(text: &str) -> Option<u64> {
    match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => text.parse().ok(),
    }
}

fn single(text: &str) -> Option<char> {
    let mut chars = text.chars();
    let first = chars.next()?;
    chars.next().is_none().then_some(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn areas(map: &Map) -> Vec<(u64, u64, char)> {
        map.areas()
            .map(|(start, end, area)| (start, end, area.symbol()))
            .collect()
    }

    #[test]
    fn setting_areas_splits_and_merges_them() {
        let mut map = Map::new(1000);
        map.set(100, 200, Area::Copied);
        map.set(300, 400, Area::Copied);
        assert_eq!(
            areas(&map),
            [
                (0, 100, '?'),
                (100, 200, '+'),
                (200, 300, '?'),
                (300, 400, '+'),
                (400, 1000, '?')
            ]
        );
        map.set(200, 300, Area::Copied);
        assert_eq!(
            areas(&map),
            [(0, 100, '?'), (100, 400, '+'), (400, 1000, '?')]
        );
        map.set(150, 450, Area::Bad);
        assert_eq!(
            areas(&map),
            [
                (0, 100, '?'),
                (100, 150, '+'),
                (150, 450, '-'),
                (450, 1000, '?')
            ]
        );
        map.set(900, 2000, Area::Copied);
        assert_eq!(map.area_at(999), Some((900, 1000, Area::Copied)));
        map.set(0, 1000, Area::Copied);
        assert_eq!(areas(&map), [(0, 1000, '+')]);
        map.set(500, 500, Area::Bad);
        assert_eq!(areas(&map), [(0, 1000, '+')]);
    }

    #[test]
    fn finds_areas_forwards_and_backwards() {
        let mut map = Map::new(1000);
        map.set(0, 1000, Area::Copied);
        map.set(100, 200, Area::Bad);
        map.set(500, 600, Area::Bad);
        assert_eq!(map.next(0, &[Area::Bad]), Some((100, 200)));
        assert_eq!(map.next(150, &[Area::Bad]), Some((150, 200)));
        assert_eq!(map.next(200, &[Area::Bad]), Some((500, 600)));
        assert_eq!(map.next(600, &[Area::Bad]), None);
        assert_eq!(map.previous(1000, &[Area::Bad]), Some((500, 600)));
        assert_eq!(map.previous(550, &[Area::Bad]), Some((500, 550)));
        assert_eq!(map.previous(500, &[Area::Bad]), Some((100, 200)));
        assert_eq!(map.previous(100, &[Area::Bad]), None);
        let totals = map.totals();
        assert_eq!((totals.bad, totals.bad_areas, totals.copied), (200, 2, 800));
    }

    #[test]
    fn round_trips_through_text() {
        let mut map = Map::new(0x10_0000);
        map.set(0, 0x8000, Area::Copied);
        map.set(0x8000, 0x8200, Area::Bad);
        map.set(0x8200, 0x9000, Area::Unscraped);
        map.phase = Phase::Scraping;
        map.pass = 1;
        map.position = 0x8200;
        map.source = Some("disk:1".to_owned());
        map.target = Some(r"image:D:\disk 1.img".to_owned());
        let text = map.to_text();
        assert!(text.contains("0x00008000  0x00000200  -\n"), "{text}");
        assert_eq!(Map::parse(&text).unwrap(), map);
    }

    #[test]
    fn reads_ddrescue_maps() {
        let text = "# Mapfile. Created by GNU ddrescue version 1.27\n\
                    # Command line: ddrescue /dev/sdb disk.img disk.map\n\
                    # current_pos  current_status  current_pass\n\
                    0x00100000     *               1\n\
                    #      pos        size  status\n\
                    0x00000000  0x00080000  +\n\
                    0x00080000  0x00080000  +\n\
                    0x00100000  0x00010000  *\n\
                    0x00110000  0x000F0000  ?\n";
        let map = Map::parse(text).unwrap();
        assert_eq!(map.size(), 0x20_0000);
        assert_eq!(map.phase, Phase::Trimming);
        assert_eq!(map.position, 0x10_0000);
        assert_eq!(map.source, None);
        assert_eq!(
            areas(&map),
            [
                (0, 0x10_0000, '+'),
                (0x10_0000, 0x11_0000, '*'),
                (0x11_0000, 0x20_0000, '?')
            ]
        );
        // Old ddrescue versions don't write the pass.
        let old = Map::parse("0 ? \n0 512 ?\n").unwrap();
        assert_eq!((old.pass, old.size()), (1, 512));
    }

    #[test]
    fn refuses_broken_maps() {
        for (text, line) in [
            ("", 0),
            ("0x0 X 1\n0x0 0x200 +\n", 1),
            ("0x0 ? 1\n0x200 0x200 +\n", 2),
            ("0x0 ? 1\n0x0 0x200 +\n0x400 0x200 +\n", 3),
            ("0x0 ? 1\n0x0 0x0 +\n", 2),
            ("0x0 ? 1\n0x0 0x200 Q\n", 2),
            (
                "0x0 ? 1\n0x0 0xFFFFFFFFFFFFFFFF +\n0xFFFFFFFFFFFFFFFF 0x2 +\n",
                3,
            ),
            ("0x0 ? 1\n0x0 0x200\n", 2),
            ("zz ? 1\n", 1),
        ] {
            match Map::parse(text) {
                Err(MapError::Parse { line: found, .. }) => assert_eq!(found, line, "{text:?}"),
                other => panic!("{text:?} gave {other:?}"),
            }
        }
    }

    #[test]
    fn pictures_show_the_most_worrying_state() {
        let mut map = Map::new(1000);
        map.set(0, 1000, Area::Copied);
        map.set(10, 11, Area::Bad);
        map.set(600, 1000, Area::Untried);
        map.set(990, 1000, Area::Unscraped);
        assert_eq!(map.picture(10), "-+++++???/");
        assert_eq!(map.picture(0), "");
        assert_eq!(Map::new(3).picture(10).len(), 10);
    }

    #[test]
    fn saves_and_loads_files() {
        let path = std::env::temp_dir().join(format!("datareco-map-{}.map", std::process::id()));
        let mut map = Map::new(4096);
        map.set(0, 512, Area::Copied);
        map.save(&path).unwrap();
        map.set(512, 1024, Area::Bad);
        map.save(&path).unwrap();
        assert_eq!(Map::load(&path).unwrap(), map);
        std::fs::remove_file(path).unwrap();
    }
}
