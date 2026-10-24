// Typed wrappers around the Rust commands in src-tauri/src/main.rs.
// Field names mirror the Rust structs (camelCase on the wire).

import { Channel, invoke } from "@tauri-apps/api/core";

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
  firmware: string | null;
  /** Solid-state (no seek penalty), when Windows knows. */
  solidState: boolean | null;
};

export type DiskExtent = { disk: number; offset: number; length: number };

export type Volume = {
  path: string;
  mountPoints: string[];
  label: string | null;
  fileSystem: string | null;
  size: number | null;
  /** Free space, as the file system reports it (mounted volumes only). */
  free: number | null;
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
  /** Hidden from Windows (a hidden MBR type, or GPT's hidden attribute). */
  hidden: boolean;
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
  /** GPT: the sectors partitions may use. */
  usable: { first: number; last: number } | null;
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
export type FsKind =
  | "ntfs"
  | "fat12"
  | "fat16"
  | "fat32"
  | "exfat"
  | "ext2"
  | "ext3"
  | "ext4"
  | "refs"
  | "hfsplus"
  | "apfs"
  | "btrfs"
  /** Files found by their contents (raw recovery). */
  | "raw";

/** A volume the file browser can offer (see src-tauri/src/files.rs). */
export type VolumeChoice = {
  /** What the file commands call it: `at:<offset>`, or `scan:…` for a scan's volume. */
  key: string;
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
  | { kind: "inode"; number: number; deleted: boolean }
  | { kind: "catalog"; id: number; deleted: boolean }
  | { kind: "apfs"; volume: number; id: number }
  | { kind: "btrfs"; tree: number; inode: number }
  | { kind: "orphans" }
  | { kind: "lostFolder"; number: number }
  | { kind: "span"; offset: number; length: number }
  | { kind: "group"; index: number };

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

export type ScanProgress = {
  mode: ScanMode;
  position: number;
  total: number;
  done: boolean;
  volumeClues: number;
  mftRecords: number;
  folders: number;
};

/** How a found volume opens (see crates/scan). */
export type VolumeState = "intact" | "fromBackup" | "rebuilt";

export type FoundVolume = {
  kind: FsKind;
  name: string;
  label: string | null;
  offset: number;
  size: number;
  state: VolumeState;
  /** 0-100: how much of the volume's own structure is there. */
  score: number;
  inTable: boolean;
  /** MFT records (NTFS) or folder sectors (FAT) the scan found in it. */
  found: number | null;
  evidence: string[];
};

export type ScanStatus = {
  progress: ScanProgress;
  /** Volume keys of this scan are `scan:<generation>:<index>`. */
  generation: number;
  volumes: FoundVolume[] | null;
};

export type RawProgress = { start: number; position: number; end: number; done: boolean; files: number };

/** A raw scan: how it's going, and (when done or paused) the files found. */
export type RawStatus = {
  progress: RawProgress;
  /** The volume key of the files found, for the file browser. */
  volume: string | null;
  /** Files found, by extension. */
  counts: [string, number][];
};

export type SignaturesInfo = {
  /** Where the user's signature list is (or would be). */
  path: string;
  /** Whether it exists; if not, the built-in list is used. */
  custom: boolean;
  count: number;
  problem: string | null;
};

export type RecoverProgress = {
  files: number;
  folders: number;
  bytes: number;
  incomplete: number;
  failed: number;
  current: string;
};

export type RecoverSummary = {
  progress: RecoverProgress;
  destination: string;
  log: string;
  checksums: string;
};

// ---------------------------------------------------------------- disk view

export type SegmentKind = "partition" | "free" | "unusable" | "wholeDisk";

/** A stretch of the disk map: a partition, or space no partition uses. */
export type Segment = {
  kind: SegmentKind;
  startLba: number;
  sectors: number;
  /** The partition's number in the table. */
  partition: number | null;
  /** Inside the extended partition. */
  logical: boolean;
  /** Why the space can't be used. */
  note: string | null;
  /** The volume Windows has mounted here. */
  volume: Volume | null;
};

export type DiskMap = { table: PartitionTable; segments: Segment[] };

export type Verdict = "good" | "caution" | "bad" | "unknown";
export type Finding = { level: "info" | "caution" | "bad"; message: string };

export type SmartAttribute = {
  id: number;
  name: string;
  current: number;
  worst: number;
  threshold: number | null;
  raw: number;
  prefail: boolean;
  state: "ok" | "watch" | "failedBefore" | "failing";
};

export type NvmeHealth = {
  criticalWarning: number;
  temperature: number | null;
  availableSpare: number;
  spareThreshold: number;
  percentageUsed: number;
  bytesRead: number;
  bytesWritten: number;
  hostReads: number;
  hostWrites: number;
  busyMinutes: number;
  powerCycles: number;
  powerOnHours: number;
  unsafeShutdowns: number;
  mediaErrors: number;
  errorLogEntries: number;
  warningTemperatureMinutes: number;
  criticalTemperatureMinutes: number;
};

export type HealthReport = {
  verdict: Verdict;
  summary: string;
  source: string | null;
  findings: Finding[];
  temperature: number | null;
  powerOnHours: number | null;
  powerCycles: number | null;
  lifeUsed: number | null;
  bytesWritten: number | null;
  bytesRead: number | null;
  solidState: boolean | null;
  rpm: number | null;
  attributes: SmartAttribute[];
  nvme: NvmeHealth | null;
};

export type DiskHealth = { disk: number | null; report: HealthReport };

export type SpeedCounts = { fast: number; normal: number; slow: number; verySlow: number; unreadable: number };

export type SurfaceProgress = {
  startLba: number;
  sectors: number;
  tested: number;
  counts: SpeedCounts;
  unreadableSectors: number;
  badRanges: { startLba: number; sectors: number }[];
  badRangeCount: number;
  elapsedMs: number;
  bytesPerSecond: number;
  /** One symbol per square: . untested, f fast, n normal, s slow, v very slow, x unreadable. */
  picture: string;
  done: boolean;
  stopped: string | null;
};

export type SpeedStatus = {
  sequential: { place: "start" | "middle" | "end"; startLba: number; bytesPerSecond: number }[];
  random: { readsPerSecond: number; bytesPerSecond: number; averageMicros: number } | null;
  errors: number;
  percent: number;
  done: boolean;
  stopped: string | null;
};

// ---------------------------------------------------------------- partition table changes

export type TableStyle = "mbr" | "gpt";

/** One change to a partition table (see Operation in crates/partitions/src/edit). Partitions
 *  are named by their first sector. */
export type Operation =
  | { op: "initialize"; style: TableStyle }
  | { op: "create"; startLba: number; sectors: number; typeCode: string; logical: boolean; name: string | null }
  | { op: "delete"; startLba: number }
  | { op: "setActive"; startLba: number; active: boolean }
  | { op: "setHidden"; startLba: number; hidden: boolean }
  | { op: "setType"; startLba: number; typeCode: string }
  | { op: "setName"; startLba: number; name: string }
  | { op: "convert"; style: TableStyle }
  | { op: "makeLogical"; startLba: number }
  | { op: "makePrimary"; startLba: number };

export type PlanStep = { description: string; warnings: string[] };

export type TablePlan = {
  /** Why the table can't be changed at all. */
  blocked: string | null;
  styleBefore: TableStyle | null;
  style: TableStyle | null;
  /** One per operation done. */
  steps: PlanStep[];
  /** The first operation that can't be done; later ones aren't tried. */
  error: { step: number; message: string } | null;
  sectorsWritten: number;
};

export type TypeChoice = { code: string; name: string };

export type BackupInfo = {
  path: string;
  source: string;
  disk: string;
  /** Milliseconds since 1970. */
  created: number;
  changes: string[];
  /** For an undo: the backup file it undid. */
  undoes: string | null;
  /** A later undo put this change back. */
  undone: boolean;
};

export type TableView = {
  plan: TablePlan;
  /** The map after the pending changes (as it is, with none). */
  map: DiskMap;
  /** What to type to write changes, or why they can't be written. */
  confirmation: string | null;
  notEditable: string | null;
  /** Volumes closed while the table is written. */
  volumes: string[];
  typesMbr: TypeChoice[];
  typesGpt: TypeChoice[];
  backups: BackupInfo[];
};

export type UndoView = {
  backup: string;
  changes: string[];
  created: number;
  map: DiskMap | null;
  refusal: string | null;
};

// ---------------------------------------------------------------- formatting and wiping

export type FormatSummary = {
  kind: "fat32" | "exfat";
  label: string | null;
  clusterSize: number;
  clusters: number;
  /** Bytes files can use. */
  capacity: number;
  serial: number;
};

/** Where a format or wipe writes, and what to type to confirm it. */
export type SpotInfo = {
  size: number;
  confirmation: string;
  /** Volumes closed while it's written. */
  closing: string[];
  /** The Windows volume it's written through, if any. */
  volume: string | null;
};

export type FormatPreview = { spot: SpotInfo; summary: FormatSummary | null; problem: string | null };

export type WipeProgress = { total: number; done: number; bytesPerSecond: number; finished: boolean };

// ---------------------------------------------------------------- moving and resizing

export type MovePhase = "copying" | "fixingBoot" | "writingTable" | "done";

export type MoveProgress = {
  phase: MovePhase;
  description: string;
  /** Bytes to copy, and copied. */
  total: number;
  done: number;
  bytesPerSecond: number;
  finished: boolean;
  journal: string;
  /** The partition table's backup, once written. */
  backup: string | null;
};

/** A move that didn't finish: how far it got, or why it can't carry on. */
export type UnfinishedMove = { journal: string; progress: MoveProgress | null; problem: string | null };

export type MovePreview = {
  description: string | null;
  problem: string | null;
  bytes: number;
  confirmation: string | null;
  closing: string[];
  journalDir: string;
};

export type ResizeLimits = { min: number; max: number; now: number; confirmation: string };

export type CommandError = {
  kind:
    | "access_denied"
    | "not_found"
    | "out_of_range"
    | "unsupported"
    | "io"
    | "template"
    | "damaged"
    | "destination"
    | "stopped"
    | "saved_scan"
    | "signatures"
    | "refused"
    | "copy";
  message: string;
};

export type CopyPhase = "copying" | "trimming" | "scraping" | "retrying" | "filling" | "finished";

/** Bytes of the source in each state. */
export type CopyTotals = {
  copied: number;
  untried: number;
  untrimmed: number;
  unscraped: number;
  bad: number;
  /** Separate stretches of unreadable sectors. */
  badAreas: number;
};

export type CopyProgress = {
  phase: CopyPhase;
  pass: number;
  /** What the copy is doing, in words. */
  description: string;
  position: number;
  size: number;
  totals: CopyTotals;
  readErrors: number;
  finished: boolean;
};

export type CopyStatus = {
  progress: CopyProgress;
  /** The map in squares: "+" copied, "?" not tried, "*" and "/" failed and being worked on, "-" unreadable. */
  picture: string;
  mapPath: string;
  /** Bytes per second since the copy was started or carried on. */
  rate: number | null;
  /** False once the copy has finished or stopped (its target is closed). */
  running: boolean;
};

export type CopyTarget = { kind: "image"; path: string } | { kind: "disk"; number: number };

/** What copying onto a disk will do; show it before asking to confirm. */
export type DiskTargetCheck = {
  number: number;
  name: string;
  model: string;
  size: number;
  sectorSize: number;
  /** Its volumes, which will be closed and overwritten. */
  volumes: string[];
  warnings: string[];
  /** What to type to confirm: "disk2". */
  confirmation: string;
};

export type DiskChoice = {
  number: number;
  name: string;
  model: string;
  size: number;
  check: DiskTargetCheck | null;
  /** Why the copy can't go onto it. */
  problem: string | null;
};

/** What a map file says about a copy. */
export type MapInfo = {
  source: string | null;
  target: string | null;
  size: number;
  totals: CopyTotals;
  finished: boolean;
  picture: string;
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
  openFileSystem: (id: string, volume: string) => invoke<OpenedVolume>("open_file_system", { id, volume }),
  /** A folder's entries, deleted ones included. */
  listDirectory: (id: string, volume: string, dir: EntryId) =>
    invoke<FileEntry[]>("list_directory", { id, volume, dir }),
  /** Up to `length` bytes (at most 16 MiB) of a file or NTFS stream, from `start`. */
  readFile: (id: string, volume: string, file: EntryId, stream: string | null, start: number, length: number) =>
    invoke<ArrayBuffer>("read_file", { id, volume, file, stream, start, length }),
  fileExtents: (id: string, volume: string, file: EntryId, stream: string | null) =>
    invoke<Extent[]>("file_extents", { id, volume, file, stream }),
  /** Saves `items` (null: everything but NTFS system files) into `destination`. */
  recoverFiles: (
    id: string,
    volume: string,
    items: FileEntry[] | null,
    destination: string,
    deleted: boolean,
    onProgress: (progress: RecoverProgress) => void,
  ) => {
    const progress = new Channel<RecoverProgress>();
    progress.onmessage = onProgress;
    return invoke<RecoverSummary>("recover_files", { id, volume, items, destination, deleted, progress });
  },
  stopRecovery: (id: string) => invoke<void>("stop_recovery", { id }),
  showFolder: (path: string) => invoke<void>("show_folder", { path }),
  /** Starts a scan for volumes (replacing an earlier one) and runs its first step. */
  startScan: (id: string, mode: ScanMode) => invoke<ScanStatus>("start_scan", { id, mode }),
  continueScan: (id: string) => invoke<ScanStatus>("continue_scan", { id }),
  /** What a (paused) scan has found so far. */
  scanResults: (id: string) => invoke<ScanStatus>("scan_results", { id }),
  saveScan: (id: string, path: string) => invoke<void>("save_scan", { id, path }),
  openSavedScan: (id: string, path: string) => invoke<ScanStatus>("open_saved_scan", { id, path }),
  /** Starts a raw scan (files by their contents) of the whole source. */
  startRawScan: (id: string) => invoke<RawStatus>("start_raw_scan", { id }),
  continueRawScan: (id: string) => invoke<RawStatus>("continue_raw_scan", { id }),
  /** What a (paused) raw scan has found so far. */
  rawResults: (id: string) => invoke<RawStatus>("raw_results", { id }),
  saveRawScan: (id: string, path: string) => invoke<void>("save_raw_scan", { id, path }),
  openSavedRawScan: (id: string, path: string) => invoke<RawStatus>("open_saved_raw_scan", { id, path }),
  signaturesInfo: () => invoke<SignaturesInfo>("signatures_info"),
  /** Shows the user's signature list, creating it from the built-in one. */
  openSignatures: () => invoke<void>("open_signatures"),


  /** The disks this source could be copied onto, and why the others can't. */
  copyDiskChoices: (id: string) => invoke<DiskChoice[]>("copy_disk_choices", { id }),
  readCopyMap: (path: string) => invoke<MapInfo>("read_copy_map", { path }),
  /** Starts a copy, or carries one on if `mapPath` exists; then call continueCopy. */
  startCopy: (
    id: string,
    target: CopyTarget,
    mapPath: string,
    retries: number,
    skip: boolean,
    confirmation: string | null,
  ) => invoke<CopyStatus>("start_copy", { id, target, mapPath, retries, skip, confirmation }),
  continueCopy: (id: string) => invoke<CopyStatus>("continue_copy", { id }),
  /** Saves the map and closes the target; null if no copy was running. */
  stopCopy: (id: string) => invoke<CopyStatus | null>("stop_copy", { id }),

  /** The partitions and free space on the source, with their volumes. */
  diskMap: (id: string) => invoke<DiskMap>("disk_map", { id }),
  /** The drive's S.M.A.R.T. health (of the disk under a volume). */
  diskHealth: (id: string) => invoke<DiskHealth>("disk_health", { id }),
  /** Starts a surface test of the whole source or one partition; then call continueSurfaceTest. */
  startSurfaceTest: (id: string, partition: number | null) =>
    invoke<SurfaceProgress>("start_surface_test", { id, partition }),
  continueSurfaceTest: (id: string) => invoke<SurfaceProgress>("continue_surface_test", { id }),
  /** Ends a surface test; null if none was running. */
  stopSurfaceTest: (id: string) => invoke<SurfaceProgress | null>("stop_surface_test", { id }),
  startSpeedTest: (id: string) => invoke<SpeedStatus>("start_speed_test", { id }),
  continueSpeedTest: (id: string) => invoke<SpeedStatus>("continue_speed_test", { id }),
  stopSpeedTest: (id: string) => invoke<void>("stop_speed_test", { id }),

  /** The table as it would be after `ops`, without writing. */
  planTable: (id: string, ops: Operation[]) => invoke<TableView>("plan_table", { id, ops }),
  /** Writes `ops` once `confirmation` is the disk's name; returns the backup of the old table. */
  applyTable: (id: string, ops: Operation[], confirmation: string) =>
    invoke<{ backup: string }>("apply_table", { id, ops, confirmation }),
  previewUndo: (id: string, backup: string) => invoke<UndoView>("preview_undo", { id, backup }),
  undoTable: (id: string, backup: string, confirmation: string) =>
    invoke<{ backup: string }>("undo_table", { id, backup, confirmation }),

  /** What formatting a partition (or, with nulls, the whole source) would make. */
  previewFormat: (
    id: string,
    startLba: number | null,
    sectors: number | null,
    fileSystem: string,
    label: string | null,
    clusterSize: number | null,
  ) => invoke<FormatPreview>("preview_format", { id, startLba, sectors, fileSystem, label, clusterSize }),
  /** Formats once `confirmation` is the disk's name; says what it made. */
  formatPartition: (
    id: string,
    startLba: number | null,
    sectors: number | null,
    fileSystem: string,
    label: string | null,
    clusterSize: number | null,
    full: boolean,
    confirmation: string,
  ) =>
    invoke<string>("format_partition", { id, startLba, sectors, fileSystem, label, clusterSize, full, confirmation }),
  previewWipe: (id: string, startLba: number | null, sectors: number | null) =>
    invoke<SpotInfo>("preview_wipe", { id, startLba, sectors }),
  /** Starts wiping once `confirmation` is the disk's name; then call continueWipe. */
  startWipe: (id: string, startLba: number | null, sectors: number | null, pattern: "zeros" | "random", confirmation: string) =>
    invoke<WipeProgress>("start_wipe", { id, startLba, sectors, pattern, confirmation }),
  continueWipe: (id: string) => invoke<WipeProgress>("continue_wipe", { id }),
  stopWipe: (id: string) => invoke<WipeProgress | null>("stop_wipe", { id }),
  setLabel: (id: string, startLba: number | null, label: string) => invoke<void>("set_label", { id, startLba, label }),
  setLetter: (id: string, startLba: number | null, letter: string | null) =>
    invoke<void>("set_letter", { id, startLba, letter }),

  previewMove: (id: string, startLba: number, newStartLba: number) =>
    invoke<MovePreview>("preview_move", { id, startLba, newStartLba }),
  /** Where a partition would start on a 1 MiB boundary it has room for (its own start if aligned). */
  alignTarget: (id: string, startLba: number) => invoke<number | null>("align_partition_target", { id, startLba }),
  unfinishedMove: (id: string) =>
    invoke<UnfinishedMove | null>("unfinished_move", { id }),
  /** Starts a move (or, with nulls, carries on the unfinished one); then call continueMove. */
  startMove: (id: string, startLba: number | null, newStartLba: number | null, confirmation: string) =>
    invoke<MoveProgress>("start_move", { id, startLba, newStartLba, confirmation }),
  continueMove: (id: string) => invoke<MoveProgress>("continue_move", { id }),
  /** Stops a move; its journal stays so it can carry on. */
  stopMove: (id: string) => invoke<MoveProgress | null>("stop_move", { id }),
  resizeLimits: (id: string, startLba: number) => invoke<ResizeLimits>("resize_limits", { id, startLba }),
  resizePartition: (id: string, startLba: number, size: number, confirmation: string) =>
    invoke<void>("resize_partition", { id, startLba, size, confirmation }),

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
