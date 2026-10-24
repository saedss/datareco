import { useEffect, useRef, useState } from "react";
import { open, save } from "@tauri-apps/plugin-dialog";
import { RawStatus, SignaturesInfo, api, toCommandError } from "../api";
import { useActivity } from "../activity";
import { formatNumber, formatSize } from "../format";

type Props = {
  sourceId: string;
  /** The latest raw scan, kept by the editor so the file browser can offer it. */
  status: RawStatus | null;
  onStatus: (status: RawStatus) => void;
  /** Opens the files found in the file browser, by their volume key. */
  onBrowse: (key: string) => void;
};

const RAW_FILES = [{ name: "DataReco raw scan", extensions: ["drraw"] }];

/** Raw recovery: finding files by their contents, by file signature. */
export function RawScanPanel({ sourceId, status, onStatus, onBrowse }: Props) {
  const [running, setRunning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [signatures, setSignatures] = useState<SignaturesInfo | null>(null);
  const pausing = useRef(false);

  const refreshSignatures = () => {
    api.signaturesInfo().then(setSignatures).catch(() => setSignatures(null));
  };
  useEffect(refreshSignatures, []);

  // Closing the disk pauses the search.
  useEffect(
    () => () => {
      pausing.current = true;
    },
    [],
  );

  async function run(first: () => Promise<RawStatus>) {
    pausing.current = false;
    setRunning(true);
    setError(null);
    setNotice(null);
    refreshSignatures();
    try {
      let current = await first();
      onStatus(current);
      while (!current.progress.done && !pausing.current) {
        current = await api.continueRawScan(sourceId);
        onStatus(current);
      }
      if (!current.progress.done) onStatus(await api.rawResults(sourceId));
    } catch (e) {
      setError(toCommandError(e).message);
    } finally {
      setRunning(false);
    }
  }

  async function saveScan() {
    const path = await save({ title: "Save raw scan", defaultPath: "files.drraw", filters: RAW_FILES });
    if (!path) return;
    try {
      await api.saveRawScan(sourceId, path);
      setNotice(`Saved to ${path}.`);
    } catch (e) {
      setError(toCommandError(e).message);
    }
  }

  async function openScan() {
    const path = await open({ title: "Open saved raw scan", multiple: false, directory: false, filters: RAW_FILES });
    if (typeof path !== "string") return;
    setError(null);
    try {
      onStatus(await api.openSavedRawScan(sourceId, path));
      setNotice(`Opened ${path}.`);
    } catch (e) {
      setError(toCommandError(e).message);
    }
  }

  async function editSignatures() {
    try {
      await api.openSignatures();
      refreshSignatures();
      setNotice("Your signature list is open in Explorer. Raw scans started from now on use it.");
    } catch (e) {
      setError(toCommandError(e).message);
    }
  }

  const progress = status?.progress;
  const paused = !running && progress && !progress.done;
  const span = progress ? progress.end - progress.start : 0;
  const done = progress ? progress.position - progress.start : 0;
  const found = status?.counts.reduce((sum, [, n]) => sum + n, 0) ?? 0;

  useActivity(
    `${sourceId}:raw`,
    sourceId,
    progress
      ? {
          label: "Find files by type",
          detail: `${formatSize(done)} of ${formatSize(span)} · ${formatNumber(progress.files)} files found`,
          percent: span > 0 ? Math.round((done / span) * 1000) / 10 : null,
          state: error ? "failed" : running ? "running" : progress.done ? "finished" : "paused",
          view: "recover",
        }
      : null,
    {
      pause: () => (pausing.current = true),
      resume: () => run(() => api.continueRawScan(sourceId)),
    },
  );

  return (
    <section className="raw-section">
      <p className="muted">
        When nothing says where files are any more (after a format, or when even a deep search finds no file system),
        DataReco can recognize files by what's in them: photos, documents, archives, music and video. Their names and
        folders are lost, and a fragmented file comes back wrong.
      </p>
      <div className="toolbar">
        <div className="toolbar-group">
          <button disabled={running} onClick={() => run(() => api.startRawScan(sourceId))}>
            Find files by type
          </button>
          {running && <button onClick={() => (pausing.current = true)}>Pause</button>}
          {paused && <button onClick={() => run(() => api.continueRawScan(sourceId))}>Carry on</button>}
        </div>
        <div className="toolbar-group">
          <button disabled={running || !status} onClick={saveScan}>
            Save the results…
          </button>
          <button disabled={running} onClick={openScan}>
            Open saved results…
          </button>
          <button onClick={editSignatures} title={signatures?.path}>
            File types to look for…
          </button>
        </div>
      </div>
      {signatures && (
        <p className="muted small">
          {signatures.problem
            ? null
            : signatures.custom
              ? `Using your ${formatNumber(signatures.count)} signatures (${signatures.path}).`
              : `Using the ${formatNumber(signatures.count)} built-in signatures.`}
        </p>
      )}
      {signatures?.problem && <p className="problem error">Your signature list has a problem: {signatures.problem}</p>}
      {error && (
        <div className="banner error" role="alert">
          {error}
        </div>
      )}
      {notice && <p className="muted small scan-notice">{notice}</p>}

      {progress && (
        <div className="scan-progress">
          <progress max={Math.max(1, span)} value={done} />
          <p className="small">
            {progress.done ? "Done: " : paused ? "Paused at " : "Looking… "}
            {formatSize(done)} of {formatSize(span)} · {formatNumber(progress.files)} files found
          </p>
        </div>
      )}

      {status?.volume && (
        <div className="found-volume">
          <div className="found-head">
            <span className="found-name">{found > 0 ? `${formatNumber(found)} files found` : "No files found"}</span>
            <span className="found-actions">
              <button className="primary" disabled={found === 0} onClick={() => onBrowse(status.volume!)}>
                Browse files
              </button>
            </span>
          </div>
          <div className="chips raw-counts">
            {status.counts.map(([extension, count]) => (
              <span key={extension} className="count-chip">
                {extension.toUpperCase()} <strong>{formatNumber(count)}</strong>
              </span>
            ))}
          </div>
        </div>
      )}
    </section>
  );
}
