// Typed wrappers around the Rust commands in src-tauri/src/main.rs.
// Field names mirror the Rust structs (camelCase on the wire).

import { invoke } from "@tauri-apps/api/core";

export type AppInfo = {
  name: string;
  version: string;
  os: string;
  arch: string;
  elevated: boolean;
  /** Source id from `--open` on the command line, shown at startup. */
  startSource: string | null;
};

export type PhysicalDisk = {
  number: number;
  path: string;
  model: string;
  serial: string | null;
  bus: string;
  removable: boolean;
  size: number;
  sectorSize: number;
  physicalSectorSize: number | null;
};

export type DiskExtent = { disk: number; offset: number; length: number };

export type Volume = {
  path: string;
  mountPoints: string[];
  label: string | null;
  fileSystem: string | null;
  size: number | null;
  extents: DiskExtent[];
};

export type DeviceList = { disks: PhysicalDisk[]; volumes: Volume[] };

export type SourceInfo = {
  id: string;
  name: string;
  size: number;
  sectorSize: number;
  sectorCount: number;
};

export type ByteData = { offset: number; data: number[]; unreadable: number[] };

export type SearchProgress =
  | { status: "found"; offset: number }
  | { status: "continue"; next: number }
  | { status: "notFound" };

export type TemplateSummary = {
  id: string;
  name: string;
  description: string;
  group: string;
  size: number | null;
  extent: number;
  source: string;
};

export type TemplateList = { templates: TemplateSummary[]; problems: string[]; folder: string };

/** A decoded field or structure (see `Node` in crates/templates). */
export type TemplateNode = {
  name: string;
  offset: number;
  size: number;
  value: string;
  note: string | null;
  description: string | null;
  problem: boolean;
  children: TemplateNode[];
};

export type FileSystemInfo = { kind: string; name: string; label: string | null; size: number | null };

export type Partition = {
  number: number;
  kind: "primary" | "extended" | "logical" | "gpt";
  startLba: number;
  sectors: number;
  typeCode: string;
  typeName: string;
  name: string | null;
  guid: string | null;
  bootable: boolean;
  fileSystem: FileSystemInfo | null;
};

export type Problem = { severity: "info" | "warning" | "error"; message: string };

export type PartitionTable = {
  kind: "mbr" | "gpt" | "none";
  sectorSize: number;
  diskSectors: number;
  diskId: string | null;
  partitions: Partition[];
  wholeDisk: FileSystemInfo | null;
  problems: Problem[];
};

export type ScanMode = "quick" | "full";

export type FoundPartition = {
  startLba: number;
  sectors: number;
  fileSystem: FileSystemInfo | null;
  typeCode: string | null;
  typeName: string | null;
  name: string | null;
  confidence: "low" | "medium" | "high";
  evidence: string[];
  inTable: boolean;
};

export type SearchStatus = {
  mode: ScanMode;
  progress: { position: number; total: number; done: boolean; clues: number };
  found: FoundPartition[];
};

/** File-system kinds, as `FsKind` in crates/partitions serializes them. */
export type FsKind = "ntfs" | "fat12" | "fat16" | "fat32" | "exfat" | "ext2" | "ext3" | "ext4" | "refs" | "hfsplus" | "apfs";

/** A volume the file browser can offer (see src-tauri/src/files.rs). */
export type VolumeChoice = {
  offset: number;
  partition: number | null;
  kind: FsKind;
  name: string;
  label: string | null;
  size: number | null;
  readable: boolean;
};

export type FsVolumeInfo = {
  kind: FsKind;
  name: string;
  label: string | null;
  offset: number;
  size: number;
  clusterSize: number;
};

/** Identifies a file or folder; handed back to the engine as is. */
export type EntryId =
  | { kind: "clusters"; cluster: number; length: number; chain: boolean }
  | { kind: "fixedRoot" }
  | { kind: "record"; number: number }
  | { kind: "orphans" };

export type OpenedVolume = { info: FsVolumeInfo; root: EntryId };

export type StreamInfo = { name: string; size: number };

/** A file or folder in a listing (see `Entry` in crates/fs). */
export type FileEntry = {
  id: EntryId;
  name: string;
  isDir: boolean;
  size: number;
  deleted: boolean;
  /** Seconds since 1970-01-01 UTC. */
  created: number | null;
  modified: number | null;
  accessed: number | null;
  attributes: number;
  streams: StreamInfo[];
  note: string | null;
};

/** Where part of a file is: a byte offset on the source, or nowhere (sparse). */
export type Extent = { offset: number; length: number; sparse: boolean };

export type CommandError = {
  kind: "access_denied" | "not_found" | "out_of_range" | "unsupported" | "io" | "template" | "damaged";
  message: string;
};

export const api = {
  appInfo: () => invoke<AppInfo>("app_info"),
  listDevices: () => invoke<DeviceList>("list_devices"),
  openSource: (id: string) => invoke<SourceInfo>("open_source", { id }),
  readBytes: (id: string, offset: number, length: number) =>
    invoke<ByteData>("read_bytes", { id, offset, length }),
  /** Searches one step from `from`; call again with `next` to go on. */
  find: (id: string, pattern: number[], from: number, ignoreCase: boolean) =>
    invoke<SearchProgress>("find", { id, pattern, from, ignoreCase }),
  listTemplates: () => invoke<TemplateList>("list_templates"),
  reloadTemplates: () => invoke<TemplateList>("reload_templates"),
  detectTemplates: (id: string, offset: number) =>
    invoke<TemplateSummary[]>("detect_templates", { id, offset }),
  decodeTemplate: (id: string, template: string, offset: number) =>
    invoke<TemplateNode>("decode_template", { id, template, offset }),
  openTemplatesFolder: () => invoke<void>("open_templates_folder"),
  readPartitions: (id: string) => invoke<PartitionTable>("read_partitions", { id }),
  /** Starts a lost-partition search and runs its first step. */
  startPartitionSearch: (id: string, mode: ScanMode) =>
    invoke<SearchStatus>("start_partition_search", { id, mode }),
  continuePartitionSearch: (id: string) => invoke<SearchStatus>("continue_partition_search", { id }),
  stopPartitionSearch: (id: string) => invoke<void>("stop_partition_search", { id }),
  listVolumes: (id: string) => invoke<VolumeChoice[]>("list_volumes", { id }),
  openFileSystem: (id: string, offset: number) => invoke<OpenedVolume>("open_file_system", { id, offset }),
  /** A folder's entries, deleted ones included. */
  listDirectory: (id: string, offset: number, dir: EntryId) =>
    invoke<FileEntry[]>("list_directory", { id, offset, dir }),
  /** Up to `length` bytes (at most 16 MiB) of a file or NTFS stream, from `start`. */
  readFile: (id: string, offset: number, file: EntryId, stream: string | null, start: number, length: number) =>
    invoke<ArrayBuffer>("read_file", { id, offset, file, stream, start, length }),
  fileExtents: (id: string, offset: number, file: EntryId, stream: string | null) =>
    invoke<Extent[]>("file_extents", { id, offset, file, stream }),
  /** Restarts elevated, reopening `open` (a source id) if given. */
  restartAsAdmin: (open: string | null) => invoke<void>("restart_as_admin", { open }),
};

/** Normalizes anything a command throws into a CommandError. */
export function toCommandError(error: unknown): CommandError {
  if (typeof error === "object" && error !== null && "kind" in error && "message" in error) {
    return error as CommandError;
  }
  return { kind: "io", message: String(error) };
}

/** Source ids, as `SourceSpec::parse` in crates/io understands them. */
export const sourceId = {
  disk: (disk: PhysicalDisk) => `disk:${disk.number}`,
  volume: (volume: Volume) => `volume:${driveLetter(volume) ?? volume.path}`,
  image: (path: string) => `image:${path}`,
};

/** The volume's drive letter ("C:"), if it has one. */
export function driveLetter(volume: Volume): string | null {
  const mount = volume.mountPoints.find((m) => /^[A-Za-z]:\\$/.test(m));
  return mount ? mount.slice(0, 2) : null;
}
