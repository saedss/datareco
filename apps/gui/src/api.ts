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

export type SectorData = { lba: number; data: number[]; unreadable: number[] };

export type CommandError = {
  kind: "access_denied" | "not_found" | "out_of_range" | "unsupported" | "io";
  message: string;
};

export const api = {
  appInfo: () => invoke<AppInfo>("app_info"),
  listDevices: () => invoke<DeviceList>("list_devices"),
  openSource: (id: string) => invoke<SourceInfo>("open_source", { id }),
  readSectors: (id: string, lba: number, count: number) =>
    invoke<SectorData>("read_sectors", { id, lba, count }),
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
