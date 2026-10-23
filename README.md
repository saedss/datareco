# DataReco

A disk editor and data recovery tool: browse and edit raw disk sectors, find lost
partitions, and recover deleted or lost files from NTFS, FAT, exFAT, ext4, HFS+, APFS, btrfs
and more.

> **Status: early development (Phase 8).** DataReco lists your disks and volumes,
> has a read-only disk editor (hex view, search, bookmarks, structure templates),
> reads and checks MBR and GPT partition tables, finds lost partitions, browses
> NTFS, FAT, exFAT, ext2/3/4, HFS+, APFS and btrfs volumes (deleted files
> included, except on APFS and btrfs), scans disks for lost and damaged volumes,
> recovers files to another disk, finds files by their contents when no file
> system is left, and copies failing disks into image files or onto other disks.
> ReFS comes next; see the [roadmap](docs/ROADMAP.md).

## Using it

DataReco only reads the disks you look at. The one thing that writes is copying:
into a new image file, or onto another disk once you type that disk's name.
Windows lets programs read disks and volumes directly only when they run as
administrator; image files work without. The desktop app offers **Restart as
administrator** when it needs to.

**Desktop app:** pick a disk, volume or image file on the left.

- Scroll through the whole disk; arrow keys, Page Up/Down and Ctrl+Home/End move
  the cursor. Sector numbers are shown on the left.
- **Go to** a sector or byte offset (decimal, or hex starting with `0x`).
- **Find** text, UTF-16 text or hex bytes from the cursor onwards.
- **Structure** suggests what starts at the cursor's sector (MBR, GPT, boot
  sectors, MFT records, …) and decodes it field by field; click a field to see
  its bytes. You can [write your own templates](docs/TEMPLATES.md).
- **Partitions** shows the partition table (MBR with logical partitions, or GPT)
  with each partition's file system, and flags problems such as overlaps or a
  damaged GPT header. **Quick search** and **Full search** find lost partitions
  from the boot sectors, superblocks and backups left on the disk.
- **Bookmarks** remember places, separately for each disk and image.
- **Files** (top right) browses the NTFS, FAT, exFAT, ext2/3/4, HFS+, APFS and
  btrfs volumes on the disk: folders, files and their details, a preview, and deleted files and folders
  (struck through). NTFS files whose folder is gone are under **Lost files**.
  ext3 and ext4 clear a deleted file's size and where its data was; DataReco
  finds them in an older copy of the file's inode in the journal, when it still
  has one. On HFS+ (Mac OS Extended), a file's resource fork shows as a stream
  called `rsrc`, and deleted files are found when their catalog records are left
  over in the catalog's free space. An APFS container shows a folder per volume
  ("Macintosh HD - Data", "Preboot", …); encrypted volumes can't be read yet, and
  deleted APFS files aren't found yet. btrfs subvolumes show as folders;
  compressed btrfs files and deleted ones can't be read yet.
  **Show in sectors** jumps to where a file's data is. Tick files and folders and
  **Recover selected…**, or **Recover everything…**, to save them to a folder on
  another disk.
- **Scan** (top right) looks for volumes, including lost and damaged ones. A
  **quick scan** checks the partition table, where volumes usually start and the
  backup boot sectors at the end; a **full scan** reads every sector and also finds
  volumes whose boot sector, partition entry or FAT is gone, and NTFS files from
  before a format. Scans can be paused, saved and carried on later. **Browse files**
  opens a volume it found in the Files view.
- **Raw scan** (in Scan) finds files by their contents (photos, documents,
  archives, music, video) when no file system says where they are, for example
  after a format. What it finds opens in the Files view in a folder per kind,
  ready to preview and recover. **Edit signatures…** opens the list of what it
  looks for, to change it or add your own.
- **Copy** (top right) copies the whole disk into an image file or onto another
  disk, in passes made for failing drives: a fast first pass that skips past read
  errors, then backwards, then the edges and insides of what failed, then retries.
  A map file records what was copied (shown as a picture), so a copy can be paused
  and carried on, even after closing DataReco. Then recover from the copy instead of
  the failing drive. Copying onto a disk shows what will be erased and asks you to
  type its name; DataReco refuses the disk being copied, the disk Windows runs
  from, and disks holding the image or map file.

`datareco-gui --open disk0` starts on a given source.

**Command line:**

```sh
datareco disks                         # list physical disks and volumes
datareco read disk0                    # sector 0 of the first disk (as administrator)
datareco read backup.img -s 2048 -n 2  # two sectors of an image file
datareco decode disk0 auto             # recognize and decode sector 0
datareco decode C: ntfs_mft_record --offset 0xC0000000
datareco templates                     # the structures decode understands
datareco partitions disk1              # the partition table, file systems, problems
datareco find-partitions disk1 --full  # search every sector for lost partitions
datareco find backup.img --text "IMPORTANT" --ignore-case
datareco find disk1 --hex "55 AA" -n 10
datareco ls disk1 -p 2 -r              # every file on partition 2, deleted ones too
datareco ls E: Photos --deleted        # only what was deleted from E:\Photos
datareco cat E: "Photos/beach.jpg" > D:\beach.jpg  # print a file (cmd or PowerShell 7)
datareco cat E: notes.txt --stream Zone.Identifier  # an NTFS alternate data stream
datareco recover E: Photos Documents --to D:\Recovered  # save folders to another disk
datareco recover disk1 -p 2 --to D:\Recovered          # everything on partition 2
datareco scan disk1 --full --save disk1.drscan         # find lost and damaged volumes
datareco scan disk1 --resume disk1.drscan              # carry on after stopping it
datareco ls disk1 --scan disk1.drscan --volume 2 -r    # look inside a volume it found
datareco recover disk1 --scan disk1.drscan --volume 2 --to D:\Recovered
datareco raw disk1 --to D:\Recovered                   # find files by their contents, save them
datareco raw disk1 -p 2 --save disk1.drraw             # only partition 2; keep the results
datareco ls disk1 --raw disk1.drraw -r                 # what that raw scan found
datareco signatures                                    # what raw scans look for
datareco signatures --write my-signatures.toml         # a copy to edit; use with --signatures
datareco image disk1 --to D:\disk1.img                 # copy into an image; run again to carry on
datareco image disk1 --to D:\disk1.img --retries 3     # once done: try unreadable sectors again
datareco clone disk1 --to disk2 --map D:\c.map         # shows what would be erased on disk2
datareco clone disk1 --to disk2 --map D:\c.map --erase disk2   # copies disk1 onto disk2

```

Recovering saves each file with its times, and writes a log (what was saved, what
couldn't be and why) and a SHA-256 checksum list next to the files.

Copies keep their map in GNU ddrescue's mapfile format, so ddrescue can carry on a
copy DataReco started, and the other way round. Sectors that couldn't be read
are zeros in an image; on a disk they are filled with zeros at the end.


A source is `diskN`, a drive letter like `C:`, a volume path (`\\?\Volume{…}`),
or an image file path.

## Safety first

- **Read-only by default.** DataReco never writes to the disk you are recovering from
  unless you turn on write mode and confirm.
- **Recover to a different disk.** Saving recovered files to the source disk can
  overwrite the very data you are trying to get back, so DataReco blocks it. The
  same goes for image and map files when copying.
- **Copying onto a disk erases it**, so DataReco shows what's on it and asks you to
  type its name first. It never writes onto the disk being copied or the disk
  Windows runs from, and it closes the target's volumes before writing.
- **Copy a failing drive first.** Every read can be its last: copy it once, then
  scan and recover from the copy.

## Building

DataReco targets Windows for now; Linux and macOS support comes in Phase 10.

Prerequisites:

- [Rust](https://rustup.rs) (stable)
- [Node.js](https://nodejs.org) 22 or newer
- Visual Studio Build Tools with the "Desktop development with C++" workload
- The WebView2 runtime (built into Windows 10 and 11)

Engine and command-line tool:

```sh
cargo build
cargo test
cargo run -p datareco-cli -- --help
```

Desktop app (from `apps/gui`):

```sh
npm install
npm run tauri dev      # run with live reload
npm run tauri build    # release build: target/release/datareco-gui(.exe)
```

Plain `cargo build` at the repository root skips the desktop app, because it
needs the frontend built first. Use the `npm run tauri` commands above for it.

## Testing

Unit and integration tests run with `cargo test`. Recovery features are tested
against generated disk images with known contents; see
[testdata/README.md](testdata/README.md) for how to build them.

## Repository layout

| Path | What's there |
|---|---|
| `crates/` | The engine: disk access, partitions, file systems, scanning, imaging, RAID, templates |
| `apps/cli/` | `datareco` command-line tool |
| `apps/gui/` | Desktop app (Tauri + React) |
| `testdata/` | Test disk-image generator |
| `docs/ROADMAP.md` | Phases and what each one delivers |

## License

Not chosen yet.
