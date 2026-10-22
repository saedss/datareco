# Test disk images

DataReco is tested against disk images whose contents we know exactly.
`make_images.py` formats each image with the real Linux file-system drivers,
fills it with files, deletes some of them, and for some images damages the
disk on purpose. It writes `out/manifest.json` with the ground truth, and the
tests check that DataReco finds and recovers exactly those files.

Never test against a real disk. Use these images.

## Building the images

The generator needs Linux and root, because it formats and mounts loop devices.

| Where | Command |
|---|---|
| Windows (Docker Desktop) | `.\testdata\build-images.ps1` |
| macOS (Docker) | `./testdata/build-images.sh` |
| Linux | `sudo python3 testdata/make_images.py` |
| GitHub Actions | Built on every push; download the `test-images` artifact from the CI run |

Build only some images with `--only`, e.g. `.\testdata\build-images.ps1 --only fat32 mbr-wiped`.
List them with `python testdata/make_images.py --list`.

The full set is about 800 MB and lands in `testdata/out/`, which git ignores.

## The images

| Image | What it is |
|---|---|
| `fat12` | 1.44 MB floppy, FAT12 (smaller file set) |
| `fat16` | FAT16 volume |
| `fat32` | FAT32 volume with 512-byte clusters |
| `exfat` | exFAT volume |
| `ntfs` | NTFS volume, including an alternate data stream |
| `ext4` | ext4 volume with 4 KiB blocks |
| `mbr` | MBR disk: primary FAT32, then NTFS and ext4 as logical partitions |
| `gpt` | GPT disk: exFAT and NTFS partitions |
| `mbr-wiped` | `mbr` with sector 0 zeroed: lost partitions |
| `gpt-header-damaged` | `gpt` with the primary GPT header zeroed: the backup header must be used |
| `ntfs-reformatted` | `ntfs` quick-formatted as FAT32: recovery after format |
| `fat32-no-fat` | `fat32` with boot sector and both FATs zeroed: full scan and raw recovery |

The first six have no partition table: the volume starts at byte 0.

## What's on each volume

Each volume gets the same files (the "full" set; `fat12` gets a smaller one):

- **Plain files** of many kinds: text, PDF, ZIP, PNG, BMP, WAV and random binary,
  from 0 bytes to 3 MiB. Each has a real file header, so raw recovery can find them.
- **Tricky names:** accented and Japanese characters, a long name that doesn't fit
  8.3, and a folder five levels deep.
- **Small files** (under 1 KB) that NTFS stores inside the MFT record itself.
- **Fragmented files:** pairs of files written in alternating chunks, so their pieces
  interleave on disk.
- **Deleted files:** written last and deleted at the very end, so their data is never
  overwritten. This includes a fragmented deleted file and a whole deleted folder.

File contents come from a fixed seed, so every run writes identical files.
`python testdata/make_images.py --content-only DIR` writes just the files to a folder.
It needs no root and works on Windows.

## manifest.json

```jsonc
{
  "schema": 1,
  "sector_size": 512,
  "images": [{
    "name": "mbr",
    "file": "mbr.img",
    "size": 167772160,
    "layout": "mbr",                      // superfloppy | mbr | gpt
    "partitions": [{ "number": 1, "start_lba": 2048, "sectors": 98304, "type": "c", "extended": false }],
    "volumes": [{
      "fs": "fat32", "label": "DR_P1_FAT32", "partition": 1,
      "offset": 1048576, "size": 50331648,  // bytes from the start of the image
      "files": [{
        "path": "docs/notes.txt", "kind": "txt", "size": 12288,
        "sha256": "…", "mtime": 1705321800,  // Unix seconds, UTC
        "state": "live",                   // live | deleted
        "extents": 1                       // pieces on disk; null if the driver can't say
      }],
      "streams": [],                       // NTFS alternate data streams
      "deleted_dirs": ["deleted-folder"]   // folders deleted along with their contents
    }],
    "derived_from": null,                  // for damaged images: the image they were copied from
    "damage": []                           // what was damaged, with sector ranges
  }]
}
```

FAT stores local time with 2-second resolution. The images are built in UTC, so
`mtime` matches exactly as long as tests read FAT times as UTC.
