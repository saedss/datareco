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

export type CommandError = {
  kind: "access_denied" | "not_found" | "out_of_range" | "unsupported" | "io" | "template";
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
