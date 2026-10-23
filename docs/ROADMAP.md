# DataReco roadmap

DataReco is a disk editor and data recovery tool with the same scope as DMDE.
It is built from public file-system specifications; no DMDE code, name, UI or
templates are copied.

Each phase is its own branch and pull request, merged only when its tests pass.
Phases 0–6 are the first goal: a working Windows recovery tool for common hard
drives, USB sticks and SD cards (NTFS, FAT, exFAT).

## Ground rules

- **Read-only by default.** Nothing is written to a source disk unless the user turns
  on write mode and confirms.
- **Never recover onto the source.** Recovered files can't be saved to the disk being
  recovered, because that would overwrite the data being recovered. Neither can
  image and map files when copying.
- **Copies only write where the user said.** A new image file, or a disk the user
  confirmed by typing its name; never the source disk or the disk Windows runs from.
- **Damaged disks are untrusted input.** Parsers must never panic or crash on corrupted
  data, and are fuzz-tested. Use `get()` instead of indexing, and checked arithmetic
  on any number read from disk.
- **Test against known images**, never real disks: see [testdata/README.md](../testdata/README.md).

## Stack

- Rust engine (`crates/`), shared by the command-line tool (`apps/cli`) and the desktop app.
- Desktop app: Tauri 2 + React + TypeScript (`apps/gui`).
- CI on Windows (GitHub Actions). One Linux job builds the test disk images;
  Linux and macOS app builds start in Phase 10.

## Phases

| # | Phase | Delivers | Done when | Status |
|---|---|---|---|---|
| 0 | Foundation | Workspace, CLI and desktop shells, CI, test-image generator | CI is green and an empty app window opens | Done |
| 1 | Reading disks | List physical disks, volumes and image files (.img/.dd); cached sector reads; retry/skip/log on bad sectors; admin rights on Windows | The app lists your disks and shows the first sector of any of them | Done |
| 2 | Disk editor (view only) | Hex and text view over the whole disk; go to sector/offset; search hex/text; bookmarks; structure templates (MBR, GPT, FAT/exFAT/NTFS boot sectors, MFT record, directory entries); user-editable templates | Templates decode every structure in the test images correctly || Done |
| 3 | Partitions | MBR (incl. extended/logical) and GPT, with errors flagged; lost-partition search from boot sectors, superblocks and their backups | `mbr-wiped` and `gpt-header-damaged` partitions are found || Done |
| 4 | NTFS, FAT, exFAT | Browse folders and read files, including deleted ones. NTFS: compressed and sparse files, alternate data streams, orphaned files. FAT/exFAT: deleted entries, long names, guessing where a deleted file's data was | Every live file in the test images reads back with the right hash; deleted ones are listed | Done |
| 5 | Scanning and recovery | Quick scan; full scan that rebuilds lost volumes ranked by quality; pause/resume and saved scans; preview; recover files and folders with timestamps, a log and checksums | Files from `ntfs-reformatted` and `fat32-no-fat` are recovered | Done (`ntfs-reformatted` by raw recovery, see the note) |
| 6 | Raw recovery | Recovery by file signature: JPEG, PNG, GIF, PDF, Office/ZIP, MP4/MOV, MP3, RAR/7z and more, with an editable signature list | Raw recovery finds the files in `fat32-no-fat` | Done |
| 7 | Imaging and cloning | Disk to image, disk to disk, image to disk; failing-drive mode with a fast first pass, retry passes, reverse reading and a map file to resume | A copy with simulated bad sectors completes and resumes | Done (writing to real disks untested, see the note) |
| 8 | More file systems | ext2/3/4 and HFS+, then APFS, ReFS and btrfs (one PR each) | Each reads its test image | Done for ext2/3/4, HFS+, APFS and btrfs (APFS: see the note); ReFS deferred (see the note) |
| 9 | Advanced tools | RAID constructor (0/1/4/5/6, JBOD, auto-detect); cluster map; write mode with undo for the disk editor and partition restore; E01, VHD and VMDK images | — | |
| 10 | Linux and macOS | Builds, root/admin handling, portable packaging, code signing | — | |

Note on Phase 5 and `ntfs-reformatted`: the full scan finds the old NTFS volume
(from its backup boot sector) and opens it, but very likely with no files in it. On
that 48 MiB test volume, mkntfs puts the MFT 16 KiB in, inside the area the new
FAT32 tables overwrite, so its file records are gone. Its files come back by their
contents with raw recovery (Phase 6), which the image tests check. On real disks the
MFT sits far from the start, and a full scan recovers files after such a format,
names and folders included (tested on volumes built in memory).

Note on Phase 7: copies with simulated bad and flaky sectors, stopped and carried
on through every pass, are tested in memory, and copies into image files are
tested end to end. Writing onto a real disk (locking and dismounting its volumes,
writing straight to the drive) needs administrator rights and a spare disk, so it
hasn't been run yet; the safety checks in front of it are tested.

Note on Phase 8, ext2/3/4: folders, extent trees and block maps, holes, symbolic
links, hash-tree folders, backup superblocks and deleted files (from folder slack,
with sizes and data from older inode copies in the journal) are tested on volumes
built in memory. The `ext4` test images are made by mkfs.ext4 on Linux, so their
tests run in CI only.

Note on Phase 8, HFS+: data and resource forks, extents overflow, hard links,
symbolic links, HFSX, decomposed names, the backup volume header, and deleted
records left over in catalog nodes are tested on volumes built in memory. The
`hfsplus` test image needs a Linux kernel with the HFS+ driver; where it has none,
the image and its tests are skipped. Deleted HFS+ files come back only when the
driver left their catalog record behind, which it often doesn't.

Note on Phase 8, APFS: containers (the newest valid checkpoint, object maps),
several volumes, file-system trees with index nodes, data and resource forks,
hard links, symbolic links, holes and decomposed names are tested on containers
built in memory from Apple's APFS reference. No Linux tool writes APFS, so there is
no generated test image; a small one made on a Mac would let the image tests cover
it (see testdata/README.md). Not yet: encrypted volumes, snapshots, compressed
files, and deleted files (which on APFS means searching for older copies of
B-tree nodes).

Note on Phase 8, btrfs: chunk mapping, the root tree, subvolumes, inline,
regular and preallocated extents, holes, hard links, symbolic links and the
superblock copy at 64 MiB are tested on volumes built in memory, and the `btrfs`
test image (mkfs.btrfs, the kernel driver) checks real ones in CI. Not yet:
compressed data (zlib, lzo, zstd), volumes spanning several disks, and deleted
files (old tree nodes, as with APFS).

Note on Phase 8, ReFS: deferred until there is a real ReFS volume to test
against. Microsoft hasn't published the format, no sample images are public, and
the public notes (libyal's libfsrefs documentation, and the ReFS 3.4 extension of
The Sleuth Kit by Prade et al.) leave key parts uncertain: how 3.x translates
addresses through its container table, and how file data runs are laid out. A
reader built from those notes alone could pass its own tests and still fail on
real disks. To pick it up again: a small ReFS volume (Windows Server, Windows Pro
for Workstations, or a Dev Drive) with a few files, some deleted, saved as a raw
image. ReFS volumes are recognized meanwhile.


## Releases

Each release is a version tag (`v0.1.0`, …) on `main`. Pushing it runs
`.github/workflows/release.yml`, which builds the Windows executables and attaches
them to a draft GitHub release to look over and publish; see
[RELEASING.md](RELEASING.md). Before the first public release: choose a license
(see the README), and consider code signing (Phase 10) so Windows doesn't warn
about an unknown publisher.

Not planned unless asked for: a DOS version; DMDE-style tools that create or delete
NTFS files directly on disk; free and paid editions with activation.

## Repository layout

```
crates/
  io/          disks, image files, sector cache, bad-sector handling   (Phase 1)
  templates/   disk-editor structure templates                         (Phase 2)
  partitions/  MBR, GPT, lost-partition search                         (Phase 3)
  fs/          ntfs, fat, exfat, then ext, hfs, apfs, refs, btrfs      (Phases 4, 8)
  scan/        quick scan, full scan, raw recovery                     (Phases 5, 6)
  imaging/     image, clone, multi-pass copy                           (Phase 7)
  raid/        virtual RAID                                            (Phase 9)
apps/
  cli/         `datareco` command-line tool
  gui/         Tauri + React desktop app
testdata/      test disk-image generator
```
