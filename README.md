# DataReco

A disk editor and data recovery tool: browse and edit raw disk sectors, find lost
partitions, and recover deleted or lost files from NTFS, FAT, exFAT, ext4, HFS+, APFS, btrfs
and more.

> **Status: early development (Phase 9).** DataReco lists your disks and volumes,
> has a read-only disk editor (hex view, search, bookmarks, structure templates),
> reads and checks MBR and GPT partition tables, finds lost partitions, browses
> NTFS, FAT, exFAT, ext2/3/4, HFS+, APFS and btrfs volumes (deleted files
> included, except on APFS and btrfs), scans disks for lost and damaged volumes,
> recovers files to another disk, finds files by their contents when no file
> system is left, and copies failing disks into image files or onto other disks.
> It maps each disk, checks drive health (S.M.A.R.T., surface test, read
> speed), changes partition tables (create, delete, hide, convert MBR↔GPT, put
> lost partitions back, undo), formats partitions (FAT32 of any size, exFAT,
> NTFS), wipes them, changes labels and drive letters, moves partitions (with a
> journal that carries on after a power cut), resizes NTFS ones through
> Windows, and copies partitions or whole disks onto other disks, used space
> only (so a smaller disk will do), moving Windows to a new disk included. See
> the [roadmap](docs/ROADMAP.md).

## Using it

DataReco only reads the disks you look at, until you ask it to write: copying
into a new image file or onto another disk, changing a partition table,
formatting, wiping, moving or resizing. Writing to a disk always needs you to
type its name first.
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
- **Disk** (top right) maps the disk: its partitions and free space as a bar
  and a table, with drive letters, labels and how full each volume is. Below it:
  - **Health** reads the drive's S.M.A.R.T. data (SATA and NVMe drives, and USB
    adapters that pass it on) and says in plain words whether it's good, worth
    watching or failing, with every value on request.
  - **Surface test** reads every sector once (or one partition's) and times
    it, drawing slow and unreadable areas on a map. Nothing is written or
    repaired; on a failing drive, copy it first.
  - **Read speed** measures big reads at the start, middle and end, and small
    reads at random places, in about 12 seconds. Write speed isn't tested:
    writing could overwrite deleted files.
  - **Partition table changes**: pick a partition or free space on the map to
    create a partition, delete one, hide it, set it active, change its type or
    name, or make it logical or primary; or initialize an empty disk, or
    convert it between MBR and GPT (keeping every partition). Changes wait in a
    list, and the map shows the disk as it will be; nothing is written until you
    **Apply** them and type the disk's name. The sectors they replace are saved
    first, and **Undo a change** puts the old table back. New partitions aren't
    formatted (Windows offers to). In **Partitions** (Sectors view), a lost
    partition the search found can be **put back in the partition table** the
    same way. The disk Windows runs from, dynamic disks and damaged tables are
    left alone.
  - **Format** a partition: FAT32 (also over 32 GiB, which Windows won't do)
    and exFAT are made by DataReco and read back to check them; NTFS by
    Windows' own formatter. **Wipe** writes zeros or random bytes over a
    partition or the whole disk, so nothing on it can be recovered; it shows
    its progress and can be stopped. **Change label** and **Change drive
    letter** do what they say. Formatting or wiping a partition closes only
    its own volume, so partitions on the disk Windows runs from can be
    formatted too (not Windows' own, and the whole disk can't be wiped).
  - **Move** a partition into free space next to it, or by a number of MiB:
    DataReco copies it block by block, updates its boot sector, then writes
    the new partition table. A journal (in `%LOCALAPPDATA%\DataReco\moves`)
    records each step, so a move that was stopped, or cut off by a power cut,
    carries on where it left off (**Carry on…** on the Disk view); until then
    nothing else changes that disk. **Align to 1 MiB** moves a partition that
    doesn't start on a 1 MiB boundary onto one, which SSDs and drives with
    4 KiB sectors work best with. **Resize** shrinks or grows an NTFS
    partition through Windows (which moves files out of the way when
    shrinking), C: included while Windows runs from it; growing needs free
    space right after the partition.
  - **Copy to another disk** copies the whole disk, or the partition picked
    on the map, onto another disk. Only the space files use is copied (for
    NTFS, FAT and exFAT; other partitions whole), so a smaller disk will do
    as long as the partitions fit: they're moved closer together if they
    have to be. A whole disk goes over everything on the target; one
    partition goes into free space there and is added to its partition table
    (backed up first, so **Undo a change** takes it off again). On the disk
    Windows runs from the button says **Move Windows to another disk**: C:
    is read from a snapshot Windows takes, and the copy keeps the disk's IDs
    so Windows starts from it once you start the computer from the new disk.
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
datareco map disk1                     # partitions and free space, with drive letters
datareco health disk0                  # S.M.A.R.T.: a verdict, why, and every value
datareco surface disk1 -p 2            # read and time every sector of partition 2
datareco speed disk0                   # read speed: start, middle, end, random 4 KiB
datareco table disk1 create --size 20GiB --type ntfs   # shows the change; nothing written
datareco table disk1 create --size 20GiB --type ntfs --apply disk1   # writes it
datareco table disk1 delete 3 --apply disk1            # data stays on the disk
datareco table disk1 convert gpt --apply disk1         # keeps every partition
datareco table disk1 restore --start 206848 --sectors 41943040 --apply disk1
datareco table disk1 undo --apply disk1                # put back the table before the last change
datareco table disk1 backups                           # the changes made, with their backups
datareco format disk1 -p 2 --fs exfat --label Photos   # shows what it would make
datareco format disk1 -p 2 --fs fat32 --apply disk1    # FAT32, any size up to 2 TiB
datareco format disk1 -p 3 --fs ntfs --label Data --apply disk1   # through Windows
datareco wipe disk1 -p 2 --apply disk1                 # zeros over partition 2
datareco wipe disk2 --random --apply disk2             # the whole disk, random bytes
datareco move disk1 -p 2 --by -500MiB                  # shows the move; nothing written
datareco move disk1 -p 2 --to-sector 2048 --apply disk1   # copies it there, with a journal
datareco move disk1 --resume --apply disk1             # carry on a move that was cut off
datareco align disk1 -p 1 --apply disk1                # onto the nearest 1 MiB boundary
datareco resize disk1 -p 2 --size 50GiB --apply disk1  # NTFS, through Windows (or min, max)
datareco migrate disk0 --to disk2                      # shows how disk0's partitions would go onto disk2
datareco migrate disk0 --to disk2 --apply disk2        # copies them, used space only (Windows too)
datareco migrate disk1 --to disk2 --space-after 2 --apply disk2   # spare room after partition 2
datareco clone-partition disk1 -p 2 --to disk2 --apply disk2      # into free space on disk2
datareco label E: "Holiday 2026"                       # or without a label to remove it
datareco letter E: F                                   # or "none" to take the letter away
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

- **Read-only by default.** DataReco never writes to a disk unless you ask it to
  and type the disk's name: copying onto it, or applying partition table changes.
- **Recover to a different disk.** Saving recovered files to the source disk can
  overwrite the very data you are trying to get back, so DataReco blocks it. The
  same goes for image and map files when copying.
- **Copying onto a disk erases it**, so DataReco shows what's on it and asks you to
  type its name first. It never writes onto the disk being copied or the disk
  Windows runs from, and it closes the target's volumes before writing.
- **Copy a failing drive first.** Every read can be its last: copy it once, then
  scan and recover from the copy.
- **Partition table changes can be undone.** Before writing, DataReco saves every
  sector it replaces to a backup (in `%LOCALAPPDATA%\DataReco\table-backups`),
  and Undo writes them back, but only while the disk still holds what was
  written, so it never undoes over something newer. Deleting a partition removes
  it from the table; its files stay until something overwrites them. DataReco
  refuses the disk Windows runs from, dynamic disks, damaged tables, and disks a
  copy, recovery, search or test is using, and initializing never writes over
  sectors that hold data.
- **Formatting and wiping can't be undone.** Both show what they'll do and need
  the disk's name typed. A quick format leaves old files' data in place until
  it's overwritten (a scan may find them); a wipe overwrites everything in its
  range. Neither touches the volume Windows runs from, and the whole disk
  Windows runs from can't be wiped.
- **Moves keep a journal on another disk.** Moving a partition overwrites where
  it was, so DataReco writes down each step before taking it (and, when the
  partition moves less than a block, saves that block first). After a power cut
  or Ctrl+C the move carries on from the journal, and DataReco won't change the
  disk in any other way until it has. It checks the partition table is still
  the one the move started from, and refuses if it isn't. A move can't be
  undone from the table backup: to put a partition back, move it back. Moving
  refuses the disk Windows runs from.
- **Copying to another disk only reads the source.** While it copies, the
  source's volumes are closed so nothing changes them; on the disk Windows
  runs from, its NTFS volumes are read from a snapshot Windows takes instead
  (BitLocker volumes there are refused: turn BitLocker off first). The target's
  partition table is written last, so a copy that stops leaves no partition
  pointing at half-copied data; run it again. Copying refuses to write onto
  the source disk or the disk Windows runs from. A copy of Windows' disk keeps
  the disk's IDs, which Windows needs to start from it; while both disks are
  connected, Windows keeps one of them offline. Other disks' copies get new
  IDs, so both can be used side by side.
- **Health checks only read.** Windows passes S.M.A.R.T. commands to SATA drives
  only through a handle opened for writing, so the health check opens one; it
  sends three read-only commands through it (read the attributes, read their
  thresholds, identify the drive) and nothing else. It never changes a drive's
  settings, not even to turn S.M.A.R.T. on.

## Download

Releases are on the [Releases page](https://github.com/saedss/datareco/releases).
Each one has, for 64-bit Windows 10 and 11:

| File | What it is |
|---|---|
| `DataReco_x.y.z_x64-setup.exe` | Installer: adds DataReco to the Start menu, with an uninstaller |
| `DataReco-x.y.z-portable.exe` | The desktop app as a single file: run it from anywhere, nothing installed |
| `datareco-x.y.z.exe` | The command-line tool, a single file |

The desktop app uses Microsoft's WebView2 runtime, which comes with Windows 11 and
most up-to-date Windows 10 systems; the installer adds it when it's missing. Reading
disks and volumes needs administrator rights; the app offers to restart as
administrator when it needs them, and image files work without. The executables
aren't signed yet, so Windows SmartScreen may ask you to confirm the first time
("More info", then "Run anyway").

## Building

DataReco targets Windows for now; Linux and macOS support comes in Phase 11.

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
npm run tauri build    # release build: the app and its installer
```

`npm run tauri build` makes `target/release/datareco-gui.exe` (the app on its own)
and `target/release/bundle/nsis/DataReco_<version>_x64-setup.exe` (the installer).
Both executables, and `datareco.exe` from `cargo build --release`, include the C
runtime, so they run without Visual C++ redistributables installed (see
`.cargo/config.toml`). [RELEASING.md](docs/RELEASING.md) covers making a release.

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
