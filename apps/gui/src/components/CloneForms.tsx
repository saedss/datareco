import { useEffect, useRef, useState } from "react";
import {
  CloneInfo,
  CloneProgress,
  CloneReading,
  DiskCopyPreview,
  PartitionCopyPreview,
  Segment,
  api,
  toCommandError,
} from "../api";
import { formatNumber, formatSize } from "../format";

type SectionProps = {
  sourceId: string;
  /** The partition picked on the map, to copy it alone. */
  segment: Segment | null;
  /** "partition 2 (E: Photos)". */
  segmentWhat: string | null;
  /** Why nothing can be copied now, if so. */
  blocked: string | null;
};

/** Copying partitions onto another disk: the whole disk (moving Windows,
 *  for the disk it runs from), or one partition into free space. */
export function CopySection({ sourceId, segment, segmentWhat, blocked }: SectionProps) {
  const [info, setInfo] = useState<CloneInfo | null>(null);
  const [form, setForm] = useState<"disk" | "partition" | null>(null);
  const [message, setMessage] = useState<string | null>(null);
  useEffect(() => {
    setForm(null);
    setMessage(null);
    api
      .cloneInfo(sourceId)
      .then(setInfo)
      .catch(() => setInfo(null));
  }, [sourceId]);
  if (sourceId.startsWith("volume:")) return null;
  const windows = info?.windows ?? false;
  const done = (text: string) => {
    setForm(null);
    setMessage(text);
  };
  const partition = segment?.kind === "partition" ? segment : null;

  return (
    <section className="disk-section">
      <h2>Copy to another disk</h2>
      <p className="muted small">
        Copies partitions onto another disk, only the space their files use, so a smaller disk will do if they fit.
        This disk is only read.
        {windows &&
          " It's the disk Windows runs from: its copy is one Windows starts from, so you can move Windows to a new disk."}
      </p>
      {blocked && <p className="problem warning">{blocked}</p>}
      {message && <p className="notice-line small">{message}</p>}
      <div className="button-row">
        <button
          className="primary"
          disabled={!!blocked || !info || form !== null}
          onClick={() => {
            setMessage(null);
            setForm("disk");
          }}
        >
          {windows ? "Move Windows to another disk…" : "Copy this disk…"}
        </button>
        {partition && (
          <button
            disabled={!!blocked || !info || form !== null}
            onClick={() => {
              setMessage(null);
              setForm("partition");
            }}
          >
            Copy {segmentWhat ?? "the partition"} only…
          </button>
        )}
      </div>
      {form === "disk" && info && (
        <CopyDiskForm sourceId={sourceId} info={info} onDone={done} onClose={() => setForm(null)} />
      )}
      {form === "partition" && info && partition && (
        <CopyPartitionForm
          key={partition.startLba}
          sourceId={sourceId}
          info={info}
          segment={partition}
          what={segmentWhat ?? "the partition"}
          onDone={done}
          onClose={() => setForm(null)}
        />
      )}
    </section>
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

/** Runs a copy to the end, showing progress; Stop drops it. */
function useCopyRun(sourceId: string) {
  const [progress, setProgress] = useState<CloneProgress | null>(null);
  const [running, setRunning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const stopping = useRef(false);
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
  onClose: () => void;
};

function CopyDiskForm({ sourceId, info, onDone, onClose }: DiskFormProps) {
  const usable = info.targets.filter((t) => !t.problem);
  const [target, setTarget] = useState<number | null>(usable[0]?.number ?? null);
  const [newIds, setNewIds] = useState(!info.windows);
  const [spaceAfter, setSpaceAfter] = useState<number | null>(null);
  const [preview, setPreview] = useState<DiskCopyPreview | null>(null);
  const [typed, setTyped] = useState("");
  const copy = useCopyRun(sourceId);

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
      <h2>{info.windows ? "Move Windows to another disk" : "Copy this disk to another"}</h2>
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
            {info.windows ? "Move Windows" : "Copy"}
          </button>
        )}
        <button onClick={onClose} disabled={copy.running}>
          Close
        </button>
      </div>
    </div>
  );
}

type PartitionFormProps = {
  sourceId: string;
  info: CloneInfo;
  segment: Segment;
  what: string;
  onDone: (message: string) => void;
  onClose: () => void;
};

function CopyPartitionForm({ sourceId, info, segment, what, onDone, onClose }: PartitionFormProps) {
  const usable = info.targets.filter((t) => !t.problem);
  const [target, setTarget] = useState<number | null>(usable[0]?.number ?? null);
  const [at, setAt] = useState<number | null>(null);
  const [preview, setPreview] = useState<PartitionCopyPreview | null>(null);
  const [typed, setTyped] = useState("");
  const copy = useCopyRun(sourceId);

  useEffect(() => {
    setPreview(null);
    if (target == null) return;
    let current = true;
    api
      .previewPartitionCopy(sourceId, segment.startLba, target, at)
      .then((p) => current && setPreview(p))
      .catch((e) => current && copy.setError(toCommandError(e).message));
    return () => {
      current = false;
    };
  }, [sourceId, segment.startLba, target, at]);

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
    const finished = await copy.run(() => api.startPartitionCopy(sourceId, segment.startLba, target, place, typed));
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
        <button onClick={onClose} disabled={copy.running}>
          Close
        </button>
      </div>
    </div>
  );
}
