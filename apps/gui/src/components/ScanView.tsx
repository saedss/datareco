import { useEffect, useRef, useState } from "react";
import { open, save } from "@tauri-apps/plugin-dialog";
import { FoundVolume, ScanStatus, api, toCommandError } from "../api";
import { useActivity } from "../activity";
import { formatNumber, formatSize } from "../format";

type Props = {
  sourceId: string;
  sectorSize: number;
  /** The latest scan, kept by the editor so the file browser can offer its volumes. */
  status: ScanStatus | null;
  onStatus: (status: ScanStatus) => void;
  /** Opens a found volume in the file browser, by its volume key. */
  onBrowse: (key: string) => void;
  /** Shows a byte offset of the source in the hex editor. */
  onShowInSectors: (offset: number) => void;
};

const SCAN_FILES = [{ name: "DataReco scan", extensions: ["drscan"] }];

/** Searches for drives (volumes), lost and damaged ones included, and lists them best first. */
export function ScanView({ sourceId, sectorSize, status, onStatus, onBrowse, onShowInSectors }: Props) {
  const [running, setRunning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const pausing = useRef(false);

  // Closing the disk pauses the search.
  useEffect(
    () => () => {
      pausing.current = true;
    },
    [],
  );

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
  const deep = progress?.mode === "full";

  useActivity(
    `${sourceId}:scan`,
    sourceId,
    progress
      ? {
          label: deep ? "Deep search for lost drives" : "Quick search for lost drives",
          detail: progress.done
            ? `${status?.volumes?.length ?? 0} found`
            : `${formatSize(progress.position)} of ${formatSize(progress.total)}`,
          percent: Math.round(percent * 10) / 10,
          state: error ? "failed" : running ? "running" : progress.done ? "finished" : "paused",
          view: "recover",
        }
      : null,
    {
      pause: () => (pausing.current = true),
      resume: () => run(() => api.continueScan(sourceId)),
    },
  );

  return (
    <div className="scan-view">
      <div className="toolbar">
        <div className="toolbar-group">
          <button
            className="primary"
            disabled={running}
            onClick={() => run(() => api.startScan(sourceId, "quick"))}
            title="Seconds: the partition table, where drives usually start, and the backups at the disk's end"
          >
            Quick search
          </button>
          <button
            disabled={running}
            onClick={() => run(() => api.startScan(sourceId, "full"))}
            title="Reads every sector: slow, finds more"
          >
            Deep search (every sector)
          </button>
          {running && <button onClick={() => (pausing.current = true)}>Pause</button>}
          {paused && <button onClick={() => run(() => api.continueScan(sourceId))}>Carry on</button>}
        </div>
        <div className="toolbar-group">
          <button disabled={running || !status} onClick={saveScan}>
            Save the search…
          </button>
          <button disabled={running} onClick={openScan}>
            Open a saved search…
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
        <p className="muted small">
          The <strong>quick search</strong> takes seconds: it checks the partition table, where drives usually start,
          and the backup boot sectors at the disk's end. The <strong>deep search</strong> reads every sector: it also
          finds drives whose boot sector, partition entry or FAT is gone, and NTFS files from before a format. It can be
          paused, saved and carried on later.
        </p>
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
    return <p className="muted scan-empty">No drives found. The deep search reads every sector and finds more.</p>;
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
              <button onClick={() => onShowInSectors(volume.offset)}>Show in the hex editor</button>
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
