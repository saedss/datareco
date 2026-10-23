import { useRef, useState } from "react";
import { open, save } from "@tauri-apps/plugin-dialog";
import { FoundVolume, RawStatus, ScanStatus, api, toCommandError } from "../api";
import { formatNumber, formatSize } from "../format";
import { RawScanPanel } from "./RawScanPanel";

type Props = {
  sourceId: string;
  sectorSize: number;
  /** The latest scan, kept by the editor so the file browser can offer its volumes. */
  status: ScanStatus | null;
  onStatus: (status: ScanStatus) => void;
  /** The latest raw scan (files found by their contents). */
  raw: RawStatus | null;
  onRaw: (status: RawStatus) => void;
  /** Opens a found volume in the file browser, by its volume key. */
  onBrowse: (key: string) => void;
  /** Shows a byte offset of the source in the sector view. */
  onShowInSectors: (offset: number) => void;
};

const SCAN_FILES = [{ name: "DataReco scan", extensions: ["drscan"] }];

/** Scans for volumes, lost and damaged ones included, and lists them best first. */
export function ScanView({ sourceId, sectorSize, status, onStatus, raw, onRaw, onBrowse, onShowInSectors }: Props) {
  const [running, setRunning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const pausing = useRef(false);

  async function run(first: () => Promise<ScanStatus>) {
    pausing.current = false;
    setRunning(true);
    setError(null);
    setNotice(null);
    try {
      let current = await first();
      onStatus(current);
      while (!current.progress.done && !pausing.current) {
        current = await api.continueScan(sourceId);
        onStatus(current);
      }
      if (!current.progress.done) {
        // Paused: show what it has found so far.
        onStatus(await api.scanResults(sourceId));
      }
    } catch (e) {
      setError(toCommandError(e).message);
    } finally {
      setRunning(false);
    }
  }

  async function saveScan() {
    const path = await save({ title: "Save scan", defaultPath: "scan.drscan", filters: SCAN_FILES });
    if (!path) return;
    try {
      await api.saveScan(sourceId, path);
      setNotice(`Saved to ${path}.`);
    } catch (e) {
      setError(toCommandError(e).message);
    }
  }

  async function openScan() {
    const path = await open({ title: "Open saved scan", multiple: false, directory: false, filters: SCAN_FILES });
    if (typeof path !== "string") return;
    setError(null);
    try {
      onStatus(await api.openSavedScan(sourceId, path));
      setNotice(`Opened ${path}.`);
    } catch (e) {
      setError(toCommandError(e).message);
    }
  }

  const progress = status?.progress;
  const paused = !running && progress && !progress.done;
  const percent = progress && progress.total > 0 ? Math.min(100, (progress.position / progress.total) * 100) : 0;

  return (
    <div className="scan-view">
      <div className="toolbar">
        <div className="toolbar-group">
          <button className="primary" disabled={running} onClick={() => run(() => api.startScan(sourceId, "quick"))}>
            Quick scan
          </button>
          <button disabled={running} onClick={() => run(() => api.startScan(sourceId, "full"))}>
            Full scan
          </button>
          {running && <button onClick={() => (pausing.current = true)}>Pause</button>}
          {paused && <button onClick={() => run(() => api.continueScan(sourceId))}>Resume</button>}
        </div>
        <div className="toolbar-group">
          <button disabled={running || !status} onClick={saveScan}>
            Save scan…
          </button>
          <button disabled={running} onClick={openScan}>
            Open saved scan…
          </button>
        </div>
      </div>

      {error && (
        <div className="banner error" role="alert">
          {error}
        </div>
      )}
      {notice && <p className="muted small scan-notice">{notice}</p>}

      {!status && !running && (
        <div className="notice">
          <h2>Find lost and damaged volumes</h2>
          <p>
            A <strong>quick scan</strong> checks the partition table, where volumes usually start, and the disk's last
            sectors, where backup boot sectors are. It takes seconds.
          </p>
          <p>
            A <strong>full scan</strong> reads every sector. It also finds volumes whose boot sector, partition entry or
            FAT is gone, and NTFS files from before a format. You can pause it, save it and carry on later.
          </p>
        </div>
      )}

      {progress && (
        <div className="scan-progress">
          <progress max={100} value={percent} />
          <p className="small">
            {progress.done ? "Done: " : paused ? "Paused at " : `${progress.mode === "full" ? "Full" : "Quick"} scan… `}
            {formatSize(progress.position)} of {formatSize(progress.total)} · {formatNumber(progress.volumeClues)} boot
            sectors and tables
            {progress.mode === "full" &&
              ` · ${formatNumber(progress.mftRecords)} MFT records · ${formatNumber(progress.folders)} FAT folder sectors`}
          </p>
        </div>
      )}

      {status?.volumes && (
        <VolumeList
          volumes={status.volumes}
          generation={status.generation}
          sectorSize={sectorSize}
          onBrowse={onBrowse}
          onShowInSectors={onShowInSectors}
        />
      )}

      <RawScanPanel sourceId={sourceId} status={raw} onStatus={onRaw} onBrowse={onBrowse} />

    </div>
  );
}

type ListProps = {
  volumes: FoundVolume[];
  generation: number;
  sectorSize: number;
  onBrowse: (key: string) => void;
  onShowInSectors: (offset: number) => void;
};

function VolumeList({ volumes, generation, sectorSize, onBrowse, onShowInSectors }: ListProps) {
  if (volumes.length === 0) {
    return <p className="muted scan-empty">No volumes found. A full scan looks at every sector and finds more.</p>;
  }
  return (
    <ol className="found-volumes">
      {volumes.map((volume, i) => (
        <li key={`${volume.kind}-${volume.offset}-${i}`} className="found-volume">
          <div className="found-head">
            <span className="found-name">
              {volume.name}
              {volume.label && ` "${volume.label}"`}
            </span>
            <span className={`state ${volume.state}`}>{stateText(volume)}</span>
            <span className="score" title="How much of the volume's own structure is there">
              <span className="score-bar" style={{ width: `${volume.score}%` }} />
            </span>
            <span className="found-actions">
              <button className="primary" onClick={() => onBrowse(`scan:${generation}:${i}`)}>
                Browse files
              </button>
              <button onClick={() => onShowInSectors(volume.offset)}>Show in sectors</button>
            </span>
          </div>
          <p className="muted small">
            Starts at sector {formatNumber(Math.floor(volume.offset / sectorSize))} · {formatSize(volume.size)}
            {volume.inTable ? " · in the partition table" : " · not in the partition table"}
            {volume.found !== null &&
              ` · ${formatNumber(volume.found)} ${volume.kind === "ntfs" ? "MFT records" : "folder sectors"} found`}
          </p>
          <ul className="evidence">
            {volume.evidence.map((clue) => (
              <li key={clue}>{clue}</li>
            ))}
          </ul>
        </li>
      ))}
    </ol>
  );
}

function stateText(volume: FoundVolume): string {
  switch (volume.state) {
    case "intact":
      return "Opens normally";
    case "fromBackup":
      return "From its backup boot sector";
    case "rebuilt":
      return "Rebuilt from its folders";
  }
}
