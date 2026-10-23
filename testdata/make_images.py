#!/usr/bin/env python3
"""Build test disk images with known contents for DataReco.

Each image is formatted and filled by the real Linux file-system drivers. Some
files are then deleted, and some images are deliberately damaged. manifest.json
records the ground truth for every file: path, size, SHA-256, modification time,
and whether it was deleted. Recovery tests compare what DataReco finds against it.

Needs Linux and root, because it formats and mounts loop devices. On Windows or
macOS, use testdata/build-images.ps1 or testdata/build-images.sh (Docker).

    sudo python3 testdata/make_images.py                # all images -> testdata/out
    sudo python3 testdata/make_images.py --only fat32 mbr-wiped
    python3 testdata/make_images.py --list
    python3 testdata/make_images.py --content-only DIR  # no root: just the files

Only the Python standard library is used.
"""

from __future__ import annotations

import argparse
import copy
import functools
import hashlib
import io
import json
import os
import platform
import re
import shutil
import stat
import struct
import subprocess
import sys
import tempfile
import time
import traceback
import wave
import zipfile
import zlib
from collections.abc import Callable, Iterator
from typing import IO
from contextlib import contextmanager
from dataclasses import dataclass, field
from pathlib import Path

SCHEMA_VERSION = 1
SECTOR = 512
KiB = 1024
MiB = 1024 * KiB
# 2024-01-15 10:30:00 UTC. Whole even seconds, so FAT's 2-second resolution keeps it exact.
BASE_MTIME = 1_705_314_600
SCRIPT_DIR = Path(__file__).resolve().parent

GPT_MS_BASIC_DATA = "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7"
GPT_LINUX_FS = "0FC63DAF-8483-4772-8E79-3D69D8477DE4"
MBR_EXTENDED_TYPES = {"5", "f", "85"}

REQUIRED_TOOLS = [
    "losetup", "mount", "umount", "sfdisk", "filefrag",
    "mkfs.fat", "fsck.fat", "mkfs.exfat", "fsck.exfat",
    "mkntfs", "ntfs-3g", "ntfsfix", "mkfs.ext4", "e2fsck",
]


def log(message: str) -> None:
    print(message, flush=True)


# --------------------------------------------------------------------------
# Deterministic file contents. Real headers, so raw (signature) recovery can
# be tested on them too.
# --------------------------------------------------------------------------


def prng_bytes(seed: str, size: int) -> bytes:
    """Pseudo-random bytes that are identical on every run (SHA-256 counter mode)."""
    out = bytearray()
    counter = 0
    while len(out) < size:
        out += hashlib.sha256(f"{seed}:{counter}".encode()).digest()
        counter += 1
    return bytes(out[:size])


def make_text(seed: str, size: int) -> bytes:
    out = bytearray()
    line = 0
    while len(out) < size:
        out += f"{seed} line {line:05d}: the quick brown fox jumps over the lazy dog\n".encode()
        line += 1
    return bytes(out[:size])


def _png_chunk(kind: bytes, data: bytes) -> bytes:
    return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))


def make_png(seed: str, width: int, height: int) -> bytes:
    noise = prng_bytes(seed, width * height)
    raw = bytearray()
    for y in range(height):
        raw.append(0)  # PNG row filter: none
        for x in range(width):
            n = noise[y * width + x]
            raw += bytes(((x + n // 4) & 0xFF, (y + n // 4) & 0xFF, n))
    header = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)  # 8-bit RGB
    return (
        b"\x89PNG\r\n\x1a\n"
        + _png_chunk(b"IHDR", header)
        + _png_chunk(b"IDAT", zlib.compress(bytes(raw), 9))
        + _png_chunk(b"IEND", b"")
    )


def make_bmp(seed: str, width: int, height: int) -> bytes:
    noise = prng_bytes(seed, width * height)
    row_size = (width * 3 + 3) & ~3
    pixels = bytearray()
    for y in range(height):
        row = bytearray()
        for x in range(width):
            row += bytes(((x * 2) & 0xFF, (y * 2) & 0xFF, noise[y * width + x]))
        row += b"\0" * (row_size - width * 3)
        pixels += row
    header_size = 14 + 40
    return (
        b"BM"
        + struct.pack("<IHHI", header_size + len(pixels), 0, 0, header_size)
        + struct.pack("<IiiHHIIiiII", 40, width, height, 1, 24, 0, len(pixels), 2835, 2835, 0, 0)
        + bytes(pixels)
    )


def make_wav(seed: str, frames: int) -> bytes:
    buf = io.BytesIO()
    with wave.open(buf, "wb") as wav:
        wav.setnchannels(1)
        wav.setsampwidth(2)
        wav.setframerate(8000)
        wav.writeframes(prng_bytes(seed, frames * 2))
    return buf.getvalue()


def make_zip(seed: str) -> bytes:
    members = [
        ("readme.txt", make_text(f"{seed}/readme", 20 * KiB), zipfile.ZIP_DEFLATED),
        ("data/blob.bin", prng_bytes(f"{seed}/blob", 30 * KiB), zipfile.ZIP_STORED),
        ("data/notes.txt", make_text(f"{seed}/notes", 2 * KiB), zipfile.ZIP_DEFLATED),
    ]
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as zf:
        for name, data, method in members:
            info = zipfile.ZipInfo(name, date_time=(2024, 1, 15, 10, 30, 0))
            info.compress_type = method
            zf.writestr(info, data)
    return buf.getvalue()


def make_pdf(title: str, size: int) -> bytes:
    """A valid one-page PDF, padded to roughly `size` bytes with an extra stream."""
    text = ["BT /F1 12 Tf 72 760 Td 16 TL"]
    text += [f"({title}, line {i}) Tj T*" for i in range(40)]
    text.append("ET")
    content = "\n".join(text).encode("latin-1")
    padding = make_text(title, max(0, size - len(content) - 600))
    objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
        b"/Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>",
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
        b"<< /Length %d >>\nstream\n" % len(content) + content + b"\nendstream",
        b"<< /Length %d >>\nstream\n" % len(padding) + padding + b"\nendstream",
    ]
    out = bytearray(b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n")
    offsets = []
    for number, body in enumerate(objects, start=1):
        offsets.append(len(out))
        out += b"%d 0 obj\n" % number + body + b"\nendobj\n"
    xref_at = len(out)
    out += b"xref\n0 %d\n0000000000 65535 f \n" % (len(objects) + 1)
    for offset in offsets:
        out += b"%010d 00000 n \n" % offset
    out += b"trailer\n<< /Size %d /Root 1 0 R >>\nstartxref\n%d\n%%%%EOF\n" % (len(objects) + 1, xref_at)
    return bytes(out)


# --------------------------------------------------------------------------
# What goes on each volume
# --------------------------------------------------------------------------


@dataclass
class FileItem:
    path: str  # relative, forward slashes
    kind: str
    data: bytes
    deleted: bool = False


@dataclass
class Interleaved:
    """Two files written in alternating chunks, so both end up fragmented."""

    first: FileItem
    second: FileItem
    chunk: int


@dataclass
class Stream:
    """An NTFS alternate data stream attached to a file. Only used on NTFS."""

    host: str
    name: str
    data: bytes


@dataclass
class Profile:
    steps: list[FileItem | Interleaved]  # written in this order
    streams: list[Stream] = field(default_factory=list)
    deleted_dirs: list[str] = field(default_factory=list)  # removed with all contents

    def files(self) -> list[FileItem]:
        items: list[FileItem] = []
        for step in self.steps:
            if isinstance(step, Interleaved):
                items += [step.first, step.second]
            else:
                items.append(step)
        return items


LONG_NAME = "long-names/This file name is much too long for the old 8.3 format.txt"


@functools.cache
def full_profile() -> Profile:
    return Profile(
        steps=[
            # Small enough to live inside its NTFS MFT record ("resident").
            FileItem("readme.txt", "txt", make_text("readme", 300)),
            FileItem("empty.txt", "txt", b""),
            FileItem("docs/notes.txt", "txt", make_text("notes", 12 * KiB)),
            FileItem("docs/report.pdf", "pdf", make_pdf("Quarterly report", 60 * KiB)),
            FileItem("docs/archive.zip", "zip", make_zip("archive")),
            FileItem("photos/sunset.png", "png", make_png("sunset", 320, 240)),
            FileItem("photos/scan.bmp", "bmp", make_bmp("scan", 200, 150)),
            FileItem("audio/tone.wav", "wav", make_wav("tone", 40_000)),
            FileItem("data/random-1m.bin", "bin", prng_bytes("random-1m", 1 * MiB)),
            FileItem("data/random-3m.bin", "bin", prng_bytes("random-3m", 3 * MiB)),
            FileItem("deep/level1/level2/level3/level4/nested.txt", "txt", make_text("nested", 2 * KiB)),
            FileItem("unicode/Résumé café.txt", "txt", make_text("resume", 1 * KiB)),
            FileItem("unicode/日本語のファイル.txt", "txt", make_text("japanese", 1 * KiB)),
            FileItem(LONG_NAME, "txt", make_text("long", 1 * KiB)),
            Interleaved(
                FileItem("fragmented/part-a.bin", "bin", prng_bytes("frag-a", 512 * KiB)),
                FileItem("fragmented/part-b.bin", "bin", prng_bytes("frag-b", 512 * KiB)),
                chunk=32 * KiB,
            ),
            # Written last and deleted at the very end, so their data is never overwritten.
            FileItem("deleted/old-photo.png", "png", make_png("old-photo", 200, 200), deleted=True),
            FileItem("deleted/budget.zip", "zip", make_zip("budget"), deleted=True),
            FileItem("deleted/tiny.txt", "txt", make_text("tiny", 200), deleted=True),
            FileItem("deleted/report-old.pdf", "pdf", make_pdf("Old report", 40 * KiB), deleted=True),
            FileItem("deleted/large-2m.bin", "bin", prng_bytes("large-2m", 2 * MiB), deleted=True),
            # A deleted file whose pieces sit between the pieces of a file that stays.
            Interleaved(
                FileItem("deleted/frag-deleted.bin", "bin", prng_bytes("frag-deleted", 256 * KiB), deleted=True),
                FileItem("fragmented/frag-kept.bin", "bin", prng_bytes("frag-kept", 256 * KiB)),
                chunk=16 * KiB,
            ),
            FileItem("deleted-folder/inner-1.txt", "txt", make_text("inner-1", 3 * KiB), deleted=True),
            FileItem("deleted-folder/inner-2.bmp", "bmp", make_bmp("inner-2", 64, 64), deleted=True),
            FileItem("deleted-folder/sub/inner-3.txt", "txt", make_text("inner-3", 5 * KiB), deleted=True),
        ],
        streams=[Stream("docs/notes.txt", "hidden-note.txt", make_text("hidden-note", 2 * KiB))],
        deleted_dirs=["deleted-folder"],
    )


@functools.cache
def small_profile() -> Profile:
    """Fits on a 1.44 MB floppy."""
    return Profile(
        steps=[
            FileItem("readme.txt", "txt", make_text("readme", 300)),
            FileItem("docs/notes.txt", "txt", make_text("notes", 4 * KiB)),
            FileItem("photos/small.png", "png", make_png("small", 64, 64)),
            FileItem("unicode/Résumé café.txt", "txt", make_text("resume", 1 * KiB)),
            FileItem(LONG_NAME, "txt", make_text("long", 1 * KiB)),
            FileItem("deleted/tiny.txt", "txt", make_text("tiny", 200), deleted=True),
            FileItem("deleted/old-small.png", "png", make_png("old-small", 48, 48), deleted=True),
            Interleaved(
                FileItem("deleted/frag-deleted.bin", "bin", prng_bytes("frag-deleted-small", 24 * KiB), deleted=True),
                FileItem("fragmented/frag-kept.bin", "bin", prng_bytes("frag-kept-small", 24 * KiB)),
                chunk=2 * KiB,
            ),
        ],
    )


PROFILES: dict[str, Callable[[], Profile]] = {"full": full_profile, "small": small_profile}


# --------------------------------------------------------------------------
# Image definitions
# --------------------------------------------------------------------------


@dataclass
class VolumeSpec:
    fs: str  # fat12 | fat16 | fat32 | exfat | ntfs | ext4 | hfsplus
    label: str
    profile: str = "full"
    size: int = 0  # partition size in bytes (partitioned disks only)
    mbr_type: str = ""  # MBR partition type code, e.g. "c"
    logical: bool = False  # MBR logical partition (inside the extended partition)
    gpt_name: str = ""


@dataclass
class ImageSpec:
    name: str
    description: str
    size: int
    layout: str  # superfloppy (no partition table) | mbr | gpt
    volumes: list[VolumeSpec]


@dataclass
class DamagedSpec:
    name: str
    description: str
    base: str
    damage: Callable[[Path, dict], list[dict]]


BASE_IMAGES = [
    ImageSpec("fat12", "1.44 MB floppy, FAT12, no partition table", 1440 * KiB, "superfloppy",
              [VolumeSpec("fat12", "DR_FAT12", profile="small")]),
    ImageSpec("fat16", "FAT16 volume, no partition table", 32 * MiB, "superfloppy",
              [VolumeSpec("fat16", "DR_FAT16")]),
    ImageSpec("fat32", "FAT32 volume (512-byte clusters), no partition table", 48 * MiB, "superfloppy",
              [VolumeSpec("fat32", "DR_FAT32")]),
    ImageSpec("exfat", "exFAT volume, no partition table", 32 * MiB, "superfloppy",
              [VolumeSpec("exfat", "DR_EXFAT")]),
    ImageSpec("ntfs", "NTFS volume, no partition table", 48 * MiB, "superfloppy",
              [VolumeSpec("ntfs", "DR_NTFS")]),
    ImageSpec("ext4", "ext4 volume (4 KiB blocks), no partition table", 32 * MiB, "superfloppy",
              [VolumeSpec("ext4", "dr_ext4")]),
    ImageSpec("hfsplus", "HFS+ volume, no partition table", 32 * MiB, "superfloppy",
              [VolumeSpec("hfsplus", "dr_hfsplus")]),
    ImageSpec("mbr", "MBR disk: primary FAT32, then NTFS and ext4 as logical partitions", 160 * MiB, "mbr", [
        VolumeSpec("fat32", "DR_P1_FAT32", size=48 * MiB, mbr_type="c"),
        VolumeSpec("ntfs", "DR_P5_NTFS", size=48 * MiB, mbr_type="7", logical=True),
        VolumeSpec("ext4", "dr_p6_ext4", size=32 * MiB, mbr_type="83", logical=True),
    ]),
    ImageSpec("gpt", "GPT disk: exFAT and NTFS partitions", 96 * MiB, "gpt", [
        VolumeSpec("exfat", "DR_P1_EXFAT", size=32 * MiB, gpt_name="DataReco exFAT"),
        VolumeSpec("ntfs", "DR_P2_NTFS", size=48 * MiB, gpt_name="DataReco NTFS"),
    ]),
]


def zero_sectors(img: Path, lba: int, count: int) -> None:
    with open(img, "r+b") as f:
        f.seek(lba * SECTOR)
        f.write(b"\0" * (count * SECTOR))


def damage_wipe_mbr(img: Path, entry: dict) -> list[dict]:
    zero_sectors(img, 0, 1)
    return [{"kind": "zeroed", "lba": 0, "sectors": 1,
             "what": "MBR sector (partition table and signature). Boot sectors and EBRs are intact."}]


def damage_gpt_header(img: Path, entry: dict) -> list[dict]:
    zero_sectors(img, 1, 1)
    return [{"kind": "zeroed", "lba": 1, "sectors": 1,
             "what": "Primary GPT header. The backup header at the end of the disk is intact."}]


def damage_reformat(img: Path, entry: dict) -> list[dict]:
    with attached(img, 0, entry["size"]) as dev:
        run("mkfs.fat", "-F", "32", "-s", "1", "-n", "NEW_VOLUME", "-I", "--mbr=n", dev)
    return [{"kind": "reformatted", "new_fs": "fat32",
             "what": "Quick-formatted as an empty FAT32 volume. The files listed belong to the old "
                     "NTFS volume; the new FAT structures overwrote some of its start."}]


def damage_fat_tables(img: Path, entry: dict) -> list[dict]:
    with open(img, "rb") as f:
        boot = f.read(SECTOR)
    bytes_per_sector, = struct.unpack_from("<H", boot, 11)
    reserved, = struct.unpack_from("<H", boot, 14)
    fat_count = boot[16]
    fat_sectors, = struct.unpack_from("<I", boot, 36)
    if bytes_per_sector != SECTOR or fat_sectors == 0:
        raise RuntimeError("fat32 image has an unexpected boot sector")
    count = reserved + fat_count * fat_sectors
    zero_sectors(img, 0, count)
    return [{"kind": "zeroed", "lba": 0, "sectors": count,
             "what": "Boot sector, backup boot sector and both FATs. Folder entries and file data are intact."}]


DAMAGED_IMAGES = [
    DamagedSpec("mbr-wiped", "The mbr disk with its partition table wiped (lost partitions)",
                "mbr", damage_wipe_mbr),
    DamagedSpec("gpt-header-damaged", "The gpt disk with its primary GPT header destroyed",
                "gpt", damage_gpt_header),
    DamagedSpec("ntfs-reformatted", "The ntfs volume quick-formatted as FAT32 (recovery after format)",
                "ntfs", damage_reformat),
    DamagedSpec("fat32-no-fat", "The fat32 volume with boot sector and FATs wiped (full scan / raw recovery)",
                "fat32", damage_fat_tables),
]

ALL_NAMES = [s.name for s in BASE_IMAGES] + [s.name for s in DAMAGED_IMAGES]


# --------------------------------------------------------------------------
# Linux plumbing: loop devices, mkfs, mount
# --------------------------------------------------------------------------


def run(*cmd: object, stdin: str | None = None) -> str:
    args = [str(c) for c in cmd]
    result = subprocess.run(args, input=stdin, capture_output=True, text=True)
    if result.returncode != 0:
        raise RuntimeError(
            f"command failed (exit {result.returncode}): {' '.join(args)}\n{result.stdout}{result.stderr}"
        )
    return result.stdout


def ensure_loop_nodes(count: int = 64) -> None:
    """Docker only copies /dev at container start, so loop devices created later
    are missing. Create the device nodes up front."""
    if not Path("/.dockerenv").exists():
        return
    for i in range(count):
        node = Path(f"/dev/loop{i}")
        if not node.exists():
            os.mknod(node, 0o660 | stat.S_IFBLK, os.makedev(7, i))


@contextmanager
def attached(img: Path, offset: int, size: int) -> Iterator[str]:
    dev = run("losetup", "--find", "--show", "--offset", offset, "--sizelimit", size, img).strip()
    try:
        yield dev
    finally:
        subprocess.run(["losetup", "--detach", dev], check=False)


def volume_id(label: str) -> str:
    return f"{zlib.crc32(label.encode()):08x}"


def mkfs(fs: str, dev: str, label: str, start_lba: int) -> None:
    if fs in ("fat12", "fat16", "fat32"):
        cmd = ["mkfs.fat", "-F", fs[3:], "-n", label, "-i", volume_id(label),
               "-h", start_lba, "-I", "--mbr=n"]
        if fs == "fat32":
            cmd += ["-s", "1"]
        run(*cmd, dev)
    elif fs == "exfat":
        run("mkfs.exfat", "-L", label, dev)
    elif fs == "ntfs":
        run("mkntfs", "-F", "-Q", "-L", label, "-p", start_lba, "-H", 255, "-S", 63, dev)
    elif fs == "ext4":
        run("mkfs.ext4", "-F", "-q", "-b", 4096, "-L", label,
            "-E", "nodiscard,lazy_itable_init=0,lazy_journal_init=0", dev)
    elif fs == "hfsplus":
        run("mkfs.hfsplus", "-v", label, dev)
    else:
        raise ValueError(f"unknown file system {fs}")


def check_fs(fs: str, dev: str) -> None:
    """Read-only consistency check, so a broken image never reaches the tests."""
    if fs.startswith("fat"):
        run("fsck.fat", "-n", dev)
    elif fs == "exfat":
        run("fsck.exfat", "-n", dev)
    elif fs == "ntfs":
        run("ntfsfix", "-n", dev)
    elif fs == "ext4":
        run("e2fsck", "-fn", dev)
    elif fs == "hfsplus":
        run("fsck.hfsplus", "-n", dev)


def start_fuse(cmd: list[str], mnt: Path, log_file: IO[bytes]) -> subprocess.Popen[bytes]:
    """Run a FUSE driver in the foreground and wait until its mount appears.

    Foreground, so we can wait for it to finish writing after unmount (umount
    returns before a FUSE driver exits). Its output goes to a file, because an
    unread pipe could fill up and stall the driver.
    """
    process = subprocess.Popen(cmd, stdout=log_file, stderr=subprocess.STDOUT)
    deadline = time.monotonic() + 30
    while not os.path.ismount(mnt):
        if process.poll() is not None or time.monotonic() > deadline:
            if process.poll() is None:
                process.kill()
            log_file.seek(0)
            output = log_file.read().decode(errors="replace")[-4000:]
            raise RuntimeError(f"{cmd[0]} failed to mount on {mnt}:\n{output}")
        time.sleep(0.1)
    return process


@contextmanager
def mounted(fs: str, dev: str) -> Iterator[Path]:
    mnt = Path(tempfile.mkdtemp(prefix="datareco-mnt-"))
    fuse_process: subprocess.Popen[bytes] | None = None
    fuse_log = tempfile.TemporaryFile()
    try:
        if fs.startswith("fat"):
            run("mount", "-t", "vfat", "-o", "utf8,shortname=mixed", dev, mnt)
        elif fs == "exfat":
            try:
                # -i: use the kernel driver itself, never a mount.exfat helper.
                run("mount", "-i", "-t", "exfat", "-o", "iocharset=utf8", dev, mnt)
            except RuntimeError as error:
                # Some kernels (e.g. GitHub's Linux runners) ship without exFAT.
                if "unknown filesystem type" not in str(error) or not shutil.which("mount.exfat-fuse"):
                    raise
                log("    no exFAT in this kernel, using exfat-fuse")
                # -d keeps the driver in the foreground (and logs to fuse_log).
                fuse_process = start_fuse(["mount.exfat-fuse", "-d", dev, str(mnt)], mnt, fuse_log)
        elif fs == "ext4":
            run("mount", "-t", "ext4", dev, mnt)
        elif fs == "hfsplus":
            run("mount", "-t", "hfsplus", dev, mnt)
        elif fs == "ntfs":
            fuse_process = start_fuse(
                ["ntfs-3g", "-o", "streams_interface=windows,no_detach", dev, str(mnt)], mnt, fuse_log
            )
        yield mnt
    finally:
        if os.path.ismount(mnt):
            run("umount", mnt)
        if fuse_process is not None:
            try:
                fuse_process.wait(timeout=120)
            except subprocess.TimeoutExpired:
                fuse_process.kill()
                raise
        fuse_log.close()
        mnt.rmdir()


def count_extents(path: Path) -> int | None:
    """How many pieces a file is stored in, if the driver can tell us."""
    result = subprocess.run(["filefrag", str(path)], capture_output=True, text=True)
    match = re.search(r": (\d+) extents? found", result.stdout)
    return int(match.group(1)) if result.returncode == 0 and match else None


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def write_file(root: Path, item: FileItem) -> None:
    path = root / item.path
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(item.data)


def write_interleaved(root: Path, step: Interleaved) -> None:
    files = [step.first, step.second]
    for item in files:
        (root / item.path).parent.mkdir(parents=True, exist_ok=True)
    handles = [open(root / item.path, "wb") for item in files]
    try:
        longest = max(len(item.data) for item in files)
        for offset in range(0, longest, step.chunk):
            for handle, item in zip(handles, files):
                piece = item.data[offset:offset + step.chunk]
                if piece:
                    handle.write(piece)
                    handle.flush()
                    os.fsync(handle.fileno())  # allocate now, in this order
    finally:
        for handle in handles:
            handle.close()


def populate(root: Path, profile: Profile, fs: str) -> tuple[list[dict], list[dict]]:
    """Write the profile's files, delete the ones marked deleted, and return
    manifest records for files and streams."""
    for step in profile.steps:
        if isinstance(step, Interleaved):
            write_interleaved(root, step)
        else:
            write_file(root, step)

    # Streams before timestamps: writing a stream can touch its file's mtime.
    streams = []
    if fs == "ntfs":
        for stream in profile.streams:
            (root / f"{stream.host}:{stream.name}").write_bytes(stream.data)
            streams.append({"file": stream.host, "name": stream.name,
                            "size": len(stream.data), "sha256": sha256(stream.data)})

    items = profile.files()
    mtimes = {item.path: BASE_MTIME + i * 3600 for i, item in enumerate(items)}
    for item in items:
        os.utime(root / item.path, (mtimes[item.path], mtimes[item.path]))
    os.sync()

    records = [
        {
            "path": item.path,
            "kind": item.kind,
            "size": len(item.data),
            "sha256": sha256(item.data),
            "mtime": mtimes[item.path],
            "state": "deleted" if item.deleted else "live",
            "extents": count_extents(root / item.path),
        }
        for item in items
    ]

    for item in items:
        if item.deleted:
            (root / item.path).unlink()
    for directory in profile.deleted_dirs:
        shutil.rmtree(root / directory)
    os.sync()
    return records, streams


# --------------------------------------------------------------------------
# Building images
# --------------------------------------------------------------------------


def write_partition_table(img: Path, spec: ImageSpec) -> list[dict]:
    if spec.layout == "mbr":
        lines = ["label: dos", "label-id: 0x0da7a8ec", "unit: sectors", ""]
        extended_added = False
        for vol in spec.volumes:
            if vol.logical and not extended_added:
                lines.append("type=f")  # extended partition over the rest of the disk
                extended_added = True
            lines.append(f"size={vol.size // SECTOR}, type={vol.mbr_type}")
    else:
        lines = ["label: gpt", "label-id: 3B1D5F8E-9C2A-4E7B-A6D4-1F0E2C3B4A59", "unit: sectors", ""]
        for vol in spec.volumes:
            gpt_type = GPT_LINUX_FS if vol.fs == "ext4" else GPT_MS_BASIC_DATA
            lines.append(f'size={vol.size // SECTOR}, type={gpt_type}, name="{vol.gpt_name}"')
    run("sfdisk", "--quiet", img, stdin="\n".join(lines) + "\n")

    table = json.loads(run("sfdisk", "--json", img))["partitiontable"]
    partitions = []
    for part in table["partitions"]:
        number = int(re.search(r"(\d+)$", part["node"]).group(1))
        entry = {
            "number": number,
            "start_lba": part["start"],
            "sectors": part["size"],
            "type": part["type"],
            "extended": spec.layout == "mbr" and part["type"].lower() in MBR_EXTENDED_TYPES,
        }
        for key in ("name", "uuid"):
            if key in part:
                entry[key] = part[key]
        partitions.append(entry)
    return partitions


def build_image(spec: ImageSpec, work: Path) -> dict:
    log(f"[{spec.name}] {spec.description}")
    img = work / f"{spec.name}.img"
    with open(img, "wb") as f:
        f.truncate(spec.size)

    partitions: list[dict] = []
    if spec.layout == "superfloppy":
        regions: list[tuple[int, int, int | None]] = [(0, spec.size, None)]
    else:
        partitions = write_partition_table(img, spec)
        regions = [(p["start_lba"] * SECTOR, p["sectors"] * SECTOR, p["number"])
                   for p in partitions if not p["extended"]]
        if len(regions) != len(spec.volumes):
            raise RuntimeError(f"{spec.name}: expected {len(spec.volumes)} partitions, got {partitions}")

    volumes = []
    for vol, (offset, size, number) in zip(spec.volumes, regions):
        log(f"  {vol.fs} '{vol.label}' at byte {offset}, {size // MiB} MiB")
        profile = PROFILES[vol.profile]()
        with attached(img, offset, size) as dev:
            mkfs(vol.fs, dev, vol.label, offset // SECTOR)
            with mounted(vol.fs, dev) as mnt:
                files, streams = populate(mnt, profile, vol.fs)
            check_fs(vol.fs, dev)
        volumes.append({
            "fs": vol.fs,
            "label": vol.label,
            "partition": number,
            "offset": offset,
            "size": size,
            "files": files,
            "streams": streams,
            "deleted_dirs": profile.deleted_dirs,
        })

    return {
        "name": spec.name,
        "file": img.name,
        "description": spec.description,
        "size": spec.size,
        "layout": spec.layout,
        "partitions": partitions,
        "volumes": volumes,
        "derived_from": None,
        "damage": [],
    }


def build_damaged(spec: DamagedSpec, base_entry: dict, work: Path) -> dict:
    log(f"[{spec.name}] {spec.description}")
    img = work / f"{spec.name}.img"
    shutil.copyfile(work / base_entry["file"], img)
    entry = copy.deepcopy(base_entry)
    entry.update(name=spec.name, file=img.name, description=spec.description, derived_from=spec.base)
    entry["damage"] = spec.damage(img, entry)
    return entry


def selected_names(only: list[str] | None) -> list[str]:
    if not only:
        return ALL_NAMES
    unknown = [name for name in only if name not in ALL_NAMES]
    if unknown:
        sys.exit(f"unknown image(s): {', '.join(unknown)}. Known: {', '.join(ALL_NAMES)}")
    return [name for name in ALL_NAMES if name in only]


def hfsplus_missing() -> str | None:
    """Why HFS+ images can't be made here, if they can't: they need hfsprogs
    and a kernel with the HFS+ driver, which some (cloud, Docker) lack."""
    if shutil.which("mkfs.hfsplus") is None:
        return "mkfs.hfsplus is missing (apt install hfsprogs)"
    try:
        subprocess.run(["modprobe", "hfsplus"], capture_output=True)
    except OSError:
        pass  # no modprobe: the driver may still be built in
    try:
        known = Path("/proc/filesystems").read_text()
    except OSError:
        known = ""
    if not re.search(r"\bhfsplus\b", known):
        return "this kernel has no HFS+ driver (try: apt install linux-modules-extra-$(uname -r))"
    return None


def build_all(out: Path, names: list[str]) -> None:
    needs_hfsplus = [s.name for s in BASE_IMAGES if any(v.fs == "hfsplus" for v in s.volumes)]
    if any(name in needs_hfsplus for name in names):
        reason = hfsplus_missing()
        if reason:
            log(f"Skipping {', '.join(needs_hfsplus)}: {reason}")
            names = [name for name in names if name not in needs_hfsplus]
    damaged = [d for d in DAMAGED_IMAGES if d.name in names]
    needed_bases = set(names) | {d.base for d in damaged}
    out.mkdir(parents=True, exist_ok=True)

    with tempfile.TemporaryDirectory(prefix="datareco-images-") as tmp:
        work = Path(tmp)
        entries = {spec.name: build_image(spec, work) for spec in BASE_IMAGES if spec.name in needed_bases}
        for spec in damaged:
            entries[spec.name] = build_damaged(spec, entries[spec.base], work)
        for name in names:
            target = out / f"{name}.img"
            target.unlink(missing_ok=True)
            shutil.move(str(work / f"{name}.img"), target)

    manifest = {
        "schema": SCHEMA_VERSION,
        "generator": "testdata/make_images.py",
        "kernel": platform.release(),
        "sector_size": SECTOR,
        "images": [entries[name] for name in names],
    }
    manifest_path = out / "manifest.json"
    manifest_path.write_text(json.dumps(manifest, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    hand_back_to_sudo_user(out, [f"{name}.img" for name in names] + ["manifest.json"])
    log(f"Done: {len(names)} image(s) and manifest.json in {out}")


def hand_back_to_sudo_user(out: Path, files: list[str]) -> None:
    """Under sudo, give the output back to the user who ran it."""
    uid, gid = os.environ.get("SUDO_UID"), os.environ.get("SUDO_GID")
    if uid is None or gid is None:
        return
    os.chown(out, int(uid), int(gid))
    for name in files:
        os.chown(out / name, int(uid), int(gid))


def write_content_only(dest: Path) -> None:
    """Write each profile's files to plain folders. Needs no root and works on
    any OS. Handy for looking at the test content."""
    summary = {}
    for name, factory in PROFILES.items():
        profile = factory()
        root = dest / name
        for step in profile.steps:
            if isinstance(step, Interleaved):
                write_interleaved(root, step)
            else:
                write_file(root, step)
        items = profile.files()
        for i, item in enumerate(items):
            mtime = BASE_MTIME + i * 3600
            os.utime(root / item.path, (mtime, mtime))
        summary[name] = [
            {"path": item.path, "kind": item.kind, "size": len(item.data), "sha256": sha256(item.data),
             "mtime": BASE_MTIME + i * 3600, "state": "deleted" if item.deleted else "live"}
            for i, item in enumerate(items)
        ]
    (dest / "content.json").write_text(json.dumps(summary, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    log(f"Wrote {sum(len(v) for v in summary.values())} files to {dest}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--out", type=Path, default=SCRIPT_DIR / "out", help="output folder (default: testdata/out)")
    parser.add_argument("--only", nargs="+", metavar="NAME", help="build only these images")
    parser.add_argument("--list", action="store_true", help="list the images and exit")
    parser.add_argument("--content-only", type=Path, metavar="DIR",
                        help="only write the test files to DIR (no root needed)")
    args = parser.parse_args()

    if args.list:
        for spec in BASE_IMAGES:
            print(f"{spec.name:20} {spec.description}")
        for spec in DAMAGED_IMAGES:
            print(f"{spec.name:20} {spec.description}")
        return
    if args.content_only:
        write_content_only(args.content_only)
        return

    if sys.platform != "linux" or os.geteuid() != 0:
        sys.exit("Building images needs Linux and root. Run with sudo, or use "
                 "testdata/build-images.ps1 / build-images.sh to run it in Docker.")
    missing = [tool for tool in REQUIRED_TOOLS if shutil.which(tool) is None]
    if missing:
        sys.exit(f"missing tools: {', '.join(missing)}. On Debian/Ubuntu: "
                 "apt install dosfstools exfatprogs exfat-fuse ntfs-3g e2fsprogs fdisk")
    ensure_loop_nodes()
    build_all(args.out.resolve(), selected_names(args.only))


def report_ci_error(message: str) -> None:
    """In GitHub Actions, also post the error as an annotation. Annotations show
    on the run's summary page to anyone, while job logs need a signed-in account."""
    if os.environ.get("GITHUB_ACTIONS") != "true":
        return
    message = message[-4000:]  # annotations are size-limited; the end matters most
    escaped = message.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")
    print(f"::error title=make_images.py failed::{escaped}", flush=True)


if __name__ == "__main__":
    try:
        main()
    except SystemExit as stop:
        if isinstance(stop.code, str):
            report_ci_error(stop.code)
        raise
    except Exception:
        report_ci_error(traceback.format_exc())
        raise
