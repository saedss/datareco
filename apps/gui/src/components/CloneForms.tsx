import { useEffect, useRef, useState } from "react";
import {
  CloneInfo,
  CloneProgress,
  CloneReading,
  DiskCopyPreview,
  PartitionCopyPreview,
  PartitionTable,
  api,
  toCommandError,
} from "../api";
import { useActivity } from "../activity";
import { formatNumber, formatSize } from "../format";

/** The disks a copy could go onto, loaded once per source. */
function useCloneInfo(sourceId: string): { info: CloneInfo | null; error: string | null } {
  const [info, setInfo] = useState<CloneInfo | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    api
      .cloneInfo(sourceId)
      .then(setInfo)
      .catch((e) => setError(toCommandError(e).message));
  }, [sourceId]);
  return { info, error };
}

/** Copying every partition of the disk onto another disk, used space only
 *  (moving Windows, for the disk it runs from). */
export function CloneDiskPanel({ sourceId, blocked }: { sourceId: string; blocked: string | null }) {
  const { info, error } = useCloneInfo(sourceId);
  const [message, setMessage] = useState<string | null>(null);
  if (sourceId.startsWith("volume:")) {
    return <p className="muted">To clone it, open the disk this volume is on (above it, on the left).</p>;
  }
  return (
    <>
      {blocked && <p className="problem warning">{blocked}</p>}
      {error && <p className="problem error">{error}</p>}
      {message && <p className="notice-line small">{message}</p>}
      {!info && !error && <p className="muted small">Looking for disks…</p>}
      {info && !blocked && <CopyDiskForm sourceId={sourceId} info={info} onDone={setMessage} />}
    </>
  );
}

/** Copying one partition into free space on another disk. */
export function ClonePartitionPanel({
  sourceId,
  sectorSize,
  startLba,
  blocked,
}: {
  sourceId: string;
  sectorSize: number;
  /** The partition to start with, by its first sector. */
  startLba: number | null;
  blocked: string | null;
}) {
  const { info, error } = useCloneInfo(sourceId);
  const [table, setTable] = useState<PartitionTable | null>(null);
  const [picked, setPicked] = useState<number | null>(startLba);
  const [message, setMessage] = useState<string | null>(null);
  useEffect(() => {
    api
      .readPartitions(sourceId)
      .then(setTable)
      .catch(() => setTable(null));
  }, [sourceId]);
  useEffect(() => {
    if (startLba !== null) setPicked(startLba);
  }, [startLba]);
  if (sourceId.startsWith("volume:")) {
    return <p className="muted">To copy it, open the disk this volume is on (above it, on the left).</p>;
  }
  const partitions = table?.partitions.filter((p) => p.kind !== "extended") ?? [];
  const chosen = partitions.find((p) => p.startLba === picked) ?? null;
  const what = chosen
    ? `partition ${chosen.number} (${chosen.fileSystem?.label ?? chosen.fileSystem?.name ?? chosen.typeName}, ${formatSize(chosen.sectors * sectorSize)})`
    : "";
  return (
    <>
      {blocked && <p className="problem warning">{blocked}</p>}
      {error && <p className="problem error">{error}</p>}
      {message && <p className="notice-line small">{message}</p>}
      <div className="edit-row">
        <label>
          Partition{" "}
          <select value={picked ?? ""} onChange={(e) => setPicked(e.target.value === "" ? null : Number(e.target.value))}>
            <option value="">Pick one…</option>
            {partitions.map((p) => (
              <option key={p.startLba} value={p.startLba}>
                {p.number}: {p.fileSystem?.label ?? p.fileSystem?.name ?? p.typeName} · {formatSize(p.sectors * sectorSize)}
              </option>
            ))}
          </select>
        </label>
      </div>
      {info && chosen && !blocked && (
        <CopyPartitionForm
          key={chosen.startLba}
          sourceId={sourceId}
          info={info}
          startLba={chosen.startLba}
          what={what}
          onDone={setMessage}
        />
      )}
    </>
  );
}

function minutes(bytes: number): string {
  const n = Math.round(bytes / (100 << 20) / 60);
  return n < 1 ? "under a minute" : n === 1 ? "about a minute" : `about ${n} minutes`;
}

function ReadingNotes({ reading }: { reading: CloneReading }) {
  return (
    <>
      {reading.closed.length > 0 && (
        <p className="small">
          Closes <strong>{reading.closed.join(", ")}</strong> while it copies.
        </p>
      )}
      {reading.snapshots.length > 0 && (
        <p className="small">
          Reads <strong>{reading.snapshots.join(", ")}</strong> from a snapshot Windows takes, so it's copied as it was
          at one moment while Windows goes on using it.
        </p>
      )}
      {reading.live.length > 0 && <p className="small">Reads {reading.live.join(", ")} as it is.</p>}
    </>
  );
}

/** Runs a copy to the end, showing progress; Stop drops it. `label` names
 *  it in Activity. */
function useCopyRun(sourceId: string, label: string) {
  const [progress, setProgress] = useState<CloneProgress | null>(null);
  const [running, setRunning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const stopping = useRef(false);
  useActivity(
    `${sourceId}:clone`,
    sourceId,
    progress
      ? {
          label,
          detail: `${progress.description} · ${formatSize(progress.done)} of ${formatSize(progress.total)}`,
          percent: progress.total > 0 ? Math.round((progress.done / progress.total) * 1000) / 10 : null,
          state: error ? "failed" : running ? "running" : progress.finished ? "finished" : "stopped",
          view: "backup",
        }
      : null,
    { stop: () => (stopping.current = true) },
  );
  // Leaving stops it.
  useEffect(
    () => () => {
      stopping.current = true;
    },
    [],
  );
  async function run(start: () => Promise<CloneProgress>): Promise<CloneProgress | null> {
    stopping.current = false;
    setRunning(true);
    setError(null);
    try {
      let current = await start();
      setProgress(current);
      while (!current.finished && !stopping.current) {
        current = await api.continueClone(sourceId);
        setProgress(current);
      }
      if (!current.finished) {
        await api.stopClone(sourceId);
        return null;
      }
      return current;
    } catch (e) {
      setError(toCommandError(e).message);
      return null;
    } finally {
      setRunning(false);
    }
  }
  return { progress, running, error, setError, run, stop: () => (stopping.current = true) };
}

function ProgressBar({ progress, running }: { progress: CloneProgress; running: boolean }) {
  const percent = progress.total > 0 ? (progress.done / progress.total) * 100 : 0;
  return (
    <div className="scan-progress">
      <progress max={100} value={percent} />
      <p className="small">
        {progress.description} · {formatSize(progress.done)} of {formatSize(progress.total)}
        {progress.bytesPerSecond > 0 && ` · ${formatSize(progress.bytesPerSecond)}/s`}
        {!running && !progress.finished && " · stopped: start again to copy (this disk wasn't changed)"}
      </p>
    </div>
  );
}

type DiskFormProps = {
  sourceId: string;
  info: CloneInfo;
  onDone: (message: string) => void;
};

function CopyDiskForm({ sourceId, info, onDone }: DiskFormProps) {
  const usable = info.targets.filter((t) => !t.problem);
  const [target, setTarget] = useState<number | null>(usable[0]?.number ?? null);
  const [newIds, setNewIds] = useState(!info.windows);
  const [spaceAfter, setSpaceAfter] = useState<number | null>(null);
  const [preview, setPreview] = useState<DiskCopyPreview | null>(null);
  const [typed, setTyped] = useState("");
  const copy = useCopyRun(sourceId, `${info.windows ? "Move Windows" : "Clone"} onto Disk ${target ?? "?"}`);

  useEffect(() => {
    setPreview(null);
    if (target == null) return;
    let current = true;
    api
      .previewDiskCopy(sourceId, target, !newIds, spaceAfter)
      .then((p) => current && setPreview(p))
      .catch((e) => current && copy.setError(toCommandError(e).message));
    return () => {
      current = false;
    };
  }, [sourceId, target, newIds, spaceAfter]);

  const targetName = info.targets.find((t) => t.number === target)?.name ?? "the disk";
  const ready =
    !!preview?.confirmation &&
    !preview.problem &&
    typed.trim().toLowerCase() === preview.confirmation.toLowerCase();

  async function start() {
    if (target == null) return;
    const finished = await copy.run(() => api.startDiskCopy(sourceId, target, !newIds, spaceAfter, typed));
    if (!finished) return;
    const unreadable =
      finished.unreadable > 0 ? ` ${formatSize(finished.unreadable)} couldn't be read; those parts are zeros.` : "";
    onDone(
      info.windows
        ? `Windows is copied onto ${targetName}.${unreadable} To start it from there, turn the computer off, then start it from ${targetName}: swap the disks, or pick it in the computer's boot menu.`
        : `Copied onto ${targetName}.${unreadable}`,
    );
  }

  return (
    <div className="confirm-erase move-box">
      <h2>{info.windows ? "Move Windows to a new disk" : "Clone onto another disk"}</h2>
      {info.targets.length === 0 && <p className="problem warning">There's no other disk to copy onto.</p>}
      <div className="edit-row">
        <label>
          Onto{" "}
          <select
            value={target ?? ""}
            onChange={(e) => setTarget(Number(e.target.value))}
            disabled={copy.running}
          >
            {info.targets.map((t) => (
              <option key={t.number} value={t.number} disabled={!!t.problem} title={t.problem ?? undefined}>
                {t.name} · {t.model} · {formatSize(t.size)}
                {t.problem ? " (can't)" : ""}
              </option>
            ))}
          </select>
        </label>
        <label>
          Spare room{" "}
          <select
            value={spaceAfter ?? ""}
            onChange={(e) => setSpaceAfter(e.target.value === "" ? null : Number(e.target.value))}
            disabled={copy.running}
          >
            <option value="">at the end</option>
            {preview?.copy.parts
              .filter((p) => !p.logical)
              .map((p) => (
                <option key={p.fromLba} value={p.fromLba}>
                  after {p.what}
                </option>
              ))}
          </select>
        </label>
      </div>
      <label className="small">
        <input type="checkbox" checked={newIds} onChange={(e) => setNewIds(e.target.checked)} disabled={copy.running} />{" "}
        Give the copy new disk IDs, so both disks can be used side by side
        {info.windows && " (Windows won't start from a copy with new IDs)"}
      </label>
      {!preview && target != null && !copy.error && <p className="muted small">Working out the copy…</p>}
      {preview && (
        <>
          {preview.problem && <p className="problem warning">{preview.problem}</p>}
          {preview.copy.parts.length > 0 && (
            <ul className="small">
              {preview.copy.parts.map((p) => (
                <li key={p.fromLba}>
                  {p.what}:{" "}
                  {p.fromLba === p.toLba ? "same place" : `to sector ${formatNumber(p.toLba)}`}
                </li>
              ))}
            </ul>
          )}
          {preview.copy.warnings.map((w) => (
            <p key={w} className="small">
              {w}
            </p>
          ))}
          {!preview.problem && (
            <>
              <p className="small">
                Copies {formatSize(preview.bytes)} of used space (at 100 MiB/s, {minutes(preview.bytes)}).
                {preview.keepIds &&
                  " The copy keeps this disk's IDs: while both disks are connected, Windows keeps one of them offline."}
              </p>
              <ReadingNotes reading={preview.reading} />
              <p className="small">
                Erases everything on <strong>{targetName}</strong>
                {preview.erasing.length > 0 && <> ({preview.erasing.join(", ")})</>}.
              </p>
            </>
          )}
        </>
      )}
      {preview?.confirmation && !preview.problem && !copy.running && !copy.progress?.finished && (
        <label className="confirm-type">
          Type <code>{preview.confirmation}</code> to confirm:{" "}
          <input value={typed} onChange={(e) => setTyped(e.target.value)} spellCheck={false} />
        </label>
      )}
      {copy.progress && <ProgressBar progress={copy.progress} running={copy.running} />}
      {copy.error && <p className="problem error">{copy.error}</p>}
      <div className="button-row">
        {copy.running ? (
          <button className="stop" onClick={copy.stop}>
            Stop
          </button>
        ) : (
          <button className="danger" disabled={!ready} onClick={start}>
            {info.windows ? "Move Windows" : "Clone"}
          </button>
        )}
      </div>
    </div>
  );
}

type PartitionFormProps = {
  sourceId: string;
  info: CloneInfo;
  /** The partition, by its first sector. */
  startLba: number;
  what: string;
  onDone: (message: string) => void;
};

function CopyPartitionForm({ sourceId, info, startLba, what, onDone }: PartitionFormProps) {
  const usable = info.targets.filter((t) => !t.problem);
  const [target, setTarget] = useState<number | null>(usable[0]?.number ?? null);
  const [at, setAt] = useState<number | null>(null);
  const [preview, setPreview] = useState<PartitionCopyPreview | null>(null);
  const [typed, setTyped] = useState("");
  const copy = useCopyRun(sourceId, `Copy ${what} onto Disk ${target ?? "?"}`);

  useEffect(() => {
    setPreview(null);
    if (target == null) return;
    let current = true;
    api
      .previewPartitionCopy(sourceId, startLba, target, at)
      .then((p) => current && setPreview(p))
      .catch((e) => current && copy.setError(toCommandError(e).message));
    return () => {
      current = false;
    };
  }, [sourceId, startLba, target, at]);

  const targetName = info.targets.find((t) => t.number === target)?.name ?? "the disk";
  const sectorSize = info.targets.find((t) => t.number === target)?.sectorSize ?? 512;
  const ready =
    !!preview?.confirmation &&
    preview.at != null &&
    !preview.problem &&
    typed.trim().toLowerCase() === preview.confirmation.toLowerCase();

  async function start() {
    if (target == null || preview?.at == null) return;
    const place = preview.at;
    const finished = await copy.run(() => api.startPartitionCopy(sourceId, startLba, target, place, typed));
    if (!finished) return;
    onDone(
      `Copied ${what} onto ${targetName}. Its old partition table is saved in ${finished.backup ?? "the backups folder"}; "Undo a change" on ${targetName} takes the copy off again.`,
    );
  }

  return (
    <div className="confirm-erase move-box">
      <h2>Copy {what} to another disk</h2>
      <div className="edit-row">
        <label>
          Onto{" "}
          <select
            value={target ?? ""}
            onChange={(e) => {
              setAt(null);
              setTarget(Number(e.target.value));
            }}
            disabled={copy.running}
          >
            {info.targets.map((t) => (
              <option key={t.number} value={t.number} disabled={!!t.problem} title={t.problem ?? undefined}>
                {t.name} · {t.model} · {formatSize(t.size)}
                {t.problem ? " (can't)" : ""}
              </option>
            ))}
          </select>
        </label>
        {preview && preview.free.length > 0 && (
          <label>
            Into{" "}
            <select
              value={preview.at ?? ""}
              onChange={(e) => setAt(Number(e.target.value))}
              disabled={copy.running}
            >
              {preview.free.map((f) => (
                <option key={f.startLba} value={f.startLba}>
                  free space at sector {formatNumber(f.startLba)} ({formatSize(f.sectors * sectorSize)})
                  {f.logical ? ", in the extended partition" : ""}
                </option>
              ))}
            </select>
          </label>
        )}
      </div>
      {!preview && target != null && !copy.error && <p className="muted small">Working out the copy…</p>}
      {preview && (
        <>
          {preview.description && <p className="small">{preview.description}.</p>}
          {preview.problem && <p className="problem warning">{preview.problem}</p>}
          {!preview.problem && (
            <>
              <p className="small">
                Copies {formatSize(preview.bytes)} of used space (at 100 MiB/s, {minutes(preview.bytes)}), then adds it
                to {targetName}'s partition table. The table is backed up first, so "Undo a change" can take the copy
                off again.
              </p>
              <ReadingNotes reading={preview.reading} />
              {preview.closing.length > 0 && (
                <p className="small">
                  Closed while it copies: <strong>{preview.closing.join(", ")}</strong>.
                </p>
              )}
            </>
          )}
        </>
      )}
      {preview?.confirmation && !preview.problem && !copy.running && !copy.progress?.finished && (
        <label className="confirm-type">
          Type <code>{preview.confirmation}</code> to confirm:{" "}
          <input value={typed} onChange={(e) => setTyped(e.target.value)} spellCheck={false} />
        </label>
      )}
      {copy.progress && <ProgressBar progress={copy.progress} running={copy.running} />}
      {copy.error && <p className="problem error">{copy.error}</p>}
      <div className="button-row">
        {copy.running ? (
          <button className="stop" onClick={copy.stop}>
            Stop
          </button>
        ) : (
          <button className="danger" disabled={!ready} onClick={start}>
            Copy
          </button>
        )}
      </div>
    </div>
  );
}
