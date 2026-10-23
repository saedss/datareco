//! Reading file data stored in clusters (FAT and exFAT).

use std::sync::Arc;

use crate::error::FsError;
use crate::volume::Volume;
use crate::{EntryId, Extent};

/// What FAT and exFAT have in common for reading data.
pub(crate) trait ClusterMap {
    fn volume(&self) -> &Volume;
    fn cluster_size(&self) -> u64;
    /// Where a data cluster starts in the volume; `None` for numbers that
    /// aren't data clusters.
    fn cluster_offset(&self, cluster: u32) -> Option<u64>;
    /// A cluster chain from the FAT, starting with `start`.
    fn chain(&self, start: u32) -> Result<Arc<Vec<u32>>, FsError>;
}

/// The clusters holding a file: from the FAT, or a run that's assumed to be
/// contiguous.
pub(crate) enum Clusters {
    Chain(Arc<Vec<u32>>),
    Run { first: u32, count: u64 },
}

impl Clusters {
    pub fn of<M: ClusterMap + ?Sized>(
        map: &M,
        cluster: u32,
        length: u64,
        chain: bool,
    ) -> Result<Self, FsError> {
        if chain {
            Ok(Clusters::Chain(map.chain(cluster)?))
        } else {
            Ok(Clusters::Run {
                first: cluster,
                count: length.div_ceil(map.cluster_size()),
            })
        }
    }

    pub fn len(&self) -> u64 {
        match self {
            Clusters::Chain(chain) => chain.len() as u64,
            Clusters::Run { count, .. } => *count,
        }
    }

    pub fn get(&self, index: u64) -> Option<u32> {
        match self {
            Clusters::Chain(chain) => chain.get(usize::try_from(index).ok()?).copied(),
            Clusters::Run { first, count } => (index < *count)
                .then(|| u32::try_from(u64::from(*first) + index).ok())
                .flatten(),
        }
    }
}

fn file_parts(id: &EntryId) -> Result<(u32, u64, bool), FsError> {
    match id {
        EntryId::Clusters {
            cluster,
            length,
            chain,
        } => Ok((*cluster, *length, *chain)),
        _ => Err(FsError::NotFound("That entry".to_owned())),
    }
}

/// Reads file data at `offset`; returns the bytes read (0 at the end).
pub(crate) fn read<M: ClusterMap + ?Sized>(
    map: &M,
    id: &EntryId,
    offset: u64,
    buf: &mut [u8],
) -> Result<usize, FsError> {
    let (cluster, length, chain) = file_parts(id)?;
    if offset >= length || buf.is_empty() {
        return Ok(0);
    }
    let clusters = Clusters::of(map, cluster, length, chain)?;
    let cluster_size = map.cluster_size();
    let wanted = usize::try_from((length - offset).min(buf.len() as u64)).unwrap_or(buf.len());
    let mut done = 0;
    while done < wanted {
        let position = offset + done as u64;
        let index = position / cluster_size;
        let within = position % cluster_size;
        let start = clusters
            .get(index)
            .and_then(|c| map.cluster_offset(c))
            .ok_or_else(|| {
                FsError::damaged(
                    "The file's clusters end before the file does (its cluster chain is damaged).",
                )
            })?;
        let n = usize::try_from(cluster_size - within)
            .unwrap_or(usize::MAX)
            .min(wanted - done);
        map.volume()
            .read(start + within, &mut buf[done..done + n])?;
        done += n;
    }
    Ok(done)
}

/// Where a file's data is, with neighboring clusters joined into one extent.
pub(crate) fn extents<M: ClusterMap + ?Sized>(
    map: &M,
    id: &EntryId,
) -> Result<Vec<Extent>, FsError> {
    let (cluster, length, chain) = file_parts(id)?;
    let clusters = Clusters::of(map, cluster, length, chain)?;
    let cluster_size = map.cluster_size();
    let mut extents: Vec<Extent> = Vec::new();
    let mut remaining = length;
    for index in 0..clusters.len() {
        if remaining == 0 {
            break;
        }
        let Some(start) = clusters.get(index).and_then(|c| map.cluster_offset(c)) else {
            break;
        };
        let take = remaining.min(cluster_size);
        let offset = map.volume().absolute(start);
        match extents.last_mut() {
            Some(last) if last.offset + last.length == offset => last.length += take,
            _ => extents.push(Extent {
                offset,
                length: take,
                sparse: false,
            }),
        }
        remaining -= take;
    }
    Ok(extents)
}

/// How many of a deleted file's (guessed) clusters are in use now, out of how many.
pub(crate) fn reused<M: ClusterMap + ?Sized>(
    map: &M,
    cluster: u32,
    length: u64,
    in_use: impl Fn(u32) -> bool,
) -> (u64, u64) {
    let count = length.div_ceil(map.cluster_size()).min(1 << 20);
    let used = (0..count)
        .map_while(|i| u32::try_from(u64::from(cluster) + i).ok())
        .take_while(|&c| map.cluster_offset(c).is_some())
        .filter(|&c| in_use(c))
        .count() as u64;
    (used, count)
}

/// The note shown for a deleted FAT or exFAT file.
pub(crate) fn deleted_note(guessed: bool, used: u64, total: u64) -> String {
    let mut note = if guessed {
        "Deleted. Its clusters are taken to follow one another from the first one, which is right unless \
         the file was fragmented."
            .to_owned()
    } else {
        "Deleted.".to_owned()
    };
    if used > 0 {
        note.push_str(&format!(
            " {used} of its {total} clusters are now used by other files, so those parts are probably overwritten."
        ));
    }
    note
}
