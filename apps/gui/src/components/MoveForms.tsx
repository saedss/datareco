import { useEffect, useRef, useState } from "react";
import { MovePreview, MoveProgress, ResizeLimits, Segment, api, toCommandError } from "../api";
import { formatSize } from "../format";

type MoveProps = {
  sourceId: string;
  sectorSize: number;
  /** "partition 2 (D:)". */
  what: string;
  /** The partition to move, and the map's segments around it; null to carry on the unfinished move. */
  segment: Segment | null;
  segments: Segment[];
  /** Move it to the nearest 1 MiB boundary it has room for. */
  align?: boolean;
  /** Carrying on: how far the unfinished move got. */
  unfinished?: MoveProgress | null;
  onDone: (message: string) => void;
  onClose: () => void;
};

type Choice = "before" | "after" | "by" | "fixed";

/** Moving a partition: where to, a last look, then the move with its
 *  progress. Stopping keeps the journal so it can carry on. */
export function MoveForm({
  sourceId,
  sectorSize,
  what,
  segment,
  segments,
  align: aligning,
  unfinished,
  onDone,
  onClose,
}: MoveProps) {
  const [target, setTarget] = useState<number | null>(null);
  const [noRoom, setNoRoom] = useState(false);
  useEffect(() => {
    if (!aligning || !segment) return;
    api
      .alignTarget(sourceId, segment.startLba)
      .then((t) => (t == null ? setNoRoom(true) : setTarget(t)))
      .catch(() => setNoRoom(true));
  }, [aligning, sourceId, segment]);
  const align = Math.max(1, Math.floor((1 << 20) / sectorSize));
  const index = segment ? segments.findIndex((s) => s.kind === segment.kind && s.startLba === segment.startLba) : -1;
  const before = index > 0 ? segments[index - 1] : null;
  const after = index >= 0 && index + 1 < segments.length ? segments[index + 1] : null;
  const fits = (s: Segment | null) => !!segment && s?.kind === "free" && s.logical === segment.logical;
  const intoBefore =
    segment && before && fits(before)
      ? Math.ceil((before.startLba + (segment.logical ? 1 : 0)) / align) * align
      : null;
  const intoAfter =
    segment && after && fits(after)
      ? Math.floor((after.startLba + after.sectors - segment.sectors) / align) * align
      : null;
  const [choice, setChoice] = useState<Choice>(
    aligning ? "fixed" : intoBefore != null && intoBefore < (segment?.startLba ?? 0) ? "before" : "after",
  );
  const [by, setBy] = useState("100");
  const mib = (1 << 20) / sectorSize;
  const destination = !segment
    ? null
    : choice === "fixed"
      ? (target ?? null)
      : choice === "before"
        ? intoBefore
        : choice === "after"
          ? intoAfter
          : Number.isFinite(Number(by))
            ? segment.startLba + Math.round(Number(by) * mib)
            : null;

  const [preview, setPreview] = useState<MovePreview | null>(null);
  const [typed, setTyped] = useState("");
  const [progress, setProgress] = useState<MoveProgress | null>(unfinished ?? null);
  const [running, setRunning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const stopping = useRef(false);

  useEffect(() => {
    if (!segment || destination == null || destination === segment.startLba || destination < 0) {
      setPreview(null);
      return;
    }
    let current = true;
    api
      .previewMove(sourceId, segment.startLba, destination)
      .then((p) => current && setPreview(p))
      .catch((e) => current && setError(toCommandError(e).message));
    return () => {
      current = false;
    };
  }, [sourceId, segment, destination]);

  // Carrying on: what to type comes from the unfinished move's disk.
  const [resumeConfirmation, setResumeConfirmation] = useState<string | null>(null);
  useEffect(() => {
    if (segment) return;
    setResumeConfirmation(sourceId.startsWith("disk:") ? `disk${sourceId.slice(5)}` : sourceId.split(/[\\/]/).pop() ?? "");
  }, [sourceId, segment]);

  // Leaving stops the move; its journal stays.
  useEffect(
    () => () => {
      stopping.current = true;
    },
    [],
  );

  async function move() {
    stopping.current = false;
    setRunning(true);
    setError(null);
    try {
      let current = await api.startMove(sourceId, segment?.startLba ?? null, segment ? destination : null, typed);
      setProgress(current);
      while (!current.finished && !stopping.current) {
        current = await api.continueMove(sourceId);
        setProgress(current);
      }
      if (current.finished) {
        onDone(`Moved ${what}. The old partition table is saved in ${current.backup ?? "the backups folder"}.`);
      } else {
        const last = await api.stopMove(sourceId);
        if (last) setProgress(last);
      }
    } catch (e) {
      setError(toCommandError(e).message);
    } finally {
      setRunning(false);
    }
  }

  const confirmation = segment ? preview?.confirmation : resumeConfirmation;
  const ready =
    !!confirmation &&
    typed.trim().toLowerCase() === confirmation.toLowerCase() &&
    (segment === null || (!!preview && !preview.problem));
  const percent = progress && progress.total > 0 ? (progress.done / progress.total) * 100 : 0;
  const phase =
    progress?.phase === "fixingBoot"
      ? "updating its boot sector"
      : progress?.phase === "writingTable"
        ? "writing the partition table"
        : "copying";

  return (
    <div className="confirm-erase move-box">
      <h2>{!segment ? "Carry on the unfinished move" : aligning ? `Align ${what} to 1 MiB` : `Move ${what}`}</h2>
      {aligning && (
        <p className="small">
          SSDs and drives with 4 KiB sectors work best when partitions start on a 1 MiB boundary; this one doesn't.
        </p>
      )}
      {noRoom && <p className="problem warning">There's no free room next to it to move it onto a boundary.</p>}
      {!segment && unfinished && (
        <p className="small">
          {unfinished.description}. It carries on from the last block it finished.
        </p>
      )}
      {segment && choice !== "fixed" && (
        <div className="move-choices">
          {intoBefore != null && intoBefore < segment.startLba && (
            <label>
              <input type="radio" checked={choice === "before"} onChange={() => setChoice("before")} disabled={running} />{" "}
              To the start of the free space before it
            </label>
          )}
          {intoAfter != null && intoAfter > segment.startLba && (
            <label>
              <input type="radio" checked={choice === "after"} onChange={() => setChoice("after")} disabled={running} />{" "}
              To the end of the free space after it
            </label>
          )}
          <label>
            <input type="radio" checked={choice === "by"} onChange={() => setChoice("by")} disabled={running} /> By{" "}
            <input
              className="size-input"
              value={by}
              onChange={(e) => {
                setBy(e.target.value);
                setChoice("by");
              }}
              disabled={running}
            />{" "}
            MiB (negative: towards the start)
          </label>
        </div>
      )}
      {preview?.description && <p className="small">{preview.description}.</p>}
      {preview?.problem && <p className="problem warning">{preview.problem}</p>}
      {segment && preview && !preview.problem && (
        <p className="small">
          Copies {formatSize(preview.bytes)} block by block (at 100 MiB/s, about{" "}
          {Math.max(1, Math.round(preview.bytes / (100 << 20) / 60))} min). A journal in {preview.journalDir} lets it
          carry on after a power cut. It can't be undone from a backup: to put it back, move it back.
        </p>
      )}
      {preview && preview.closing.length > 0 && (
        <p className="small">
          Closed while it moves: <strong>{preview.closing.join(", ")}</strong>.
        </p>
      )}
      {confirmation && !running && !progress?.finished && (
        <label className="confirm-type">
          Type <code>{confirmation}</code> to confirm:{" "}
          <input value={typed} onChange={(e) => setTyped(e.target.value)} spellCheck={false} />
        </label>
      )}
      {progress && (
        <div className="scan-progress">
          <progress max={100} value={percent} />
          <p className="small">
            {formatSize(progress.done)} of {formatSize(progress.total)} · {phase}
            {progress.bytesPerSecond > 0 && ` · ${formatSize(progress.bytesPerSecond)}/s`}
            {!running && !progress.finished && " · stopped: carry it on any time"}
          </p>
        </div>
      )}
      {error && <p className="problem error">{error}</p>}
      <div className="button-row">
        {running ? (
          <button className="stop" onClick={() => (stopping.current = true)}>
            Stop (carry on later)
          </button>
        ) : (
          <button className="danger" disabled={!ready} onClick={move}>
            {progress && !progress.finished ? "Carry on" : segment ? "Move" : "Carry on"}
          </button>
        )}
        <button onClick={onClose} disabled={running}>
          Close
        </button>
      </div>
    </div>
  );
}

type ResizeProps = {
  sourceId: string;
  segment: Segment;
  what: string;
  onDone: (message: string) => void;
  onClose: () => void;
};

/** Shrinking or growing an NTFS partition, through Windows. */
export function ResizeForm({ sourceId, segment, what, onDone, onClose }: ResizeProps) {
  const [limits, setLimits] = useState<ResizeLimits | null>(null);
  const [size, setSize] = useState("");
  const [typed, setTyped] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const MIB = 1 << 20;

  useEffect(() => {
    api
      .resizeLimits(sourceId, segment.startLba)
      .then((l) => {
        setLimits(l);
        setSize(String(Math.floor(l.now / MIB)));
      })
      .catch((e) => setError(toCommandError(e).message));
  }, [sourceId, segment.startLba, MIB]);

  const wanted = Math.floor(Number(size) * MIB);
  const inRange = !!limits && Number.isFinite(wanted) && wanted >= limits.min && wanted <= limits.max;
  const ready = inRange && !!limits && typed.trim().toLowerCase() === limits.confirmation.toLowerCase() && wanted !== limits.now;

  async function resize() {
    setBusy(true);
    setError(null);
    try {
      await api.resizePartition(sourceId, segment.startLba, wanted, typed);
      onDone(`Resized ${what} to ${formatSize(wanted)}.`);
    } catch (e) {
      setError(toCommandError(e).message);
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="confirm-erase move-box">
      <h2>Resize {what}</h2>
      {!limits && !error && <p className="muted small">Asking Windows how far it can shrink and grow (it looks at where files sit)…</p>}
      {limits && (
        <>
          <p className="small">
            It's {formatSize(limits.now)}. Windows can make it {formatSize(limits.min)} to {formatSize(limits.max)}
            {limits.max <= limits.now && " (growing needs free space right after it)"}.
          </p>
          <div className="edit-row">
            <label>
              New size <input className="size-input" value={size} onChange={(e) => setSize(e.target.value)} /> MiB
            </label>
            <button className="link-button" onClick={() => setSize(String(Math.ceil(limits.min / MIB)))}>
              Smallest
            </button>
            <button className="link-button" onClick={() => setSize(String(Math.floor(limits.max / MIB)))}>
              Largest
            </button>
          </div>
          {!inRange && size && <p className="problem warning">That's outside what Windows can do.</p>}
          {inRange && wanted < limits.now && (
            <p className="small">Shrinking: Windows moves files out of the way first, which can take a while.</p>
          )}
          <label className="confirm-type">
            Type <code>{limits.confirmation}</code> to confirm:{" "}
            <input value={typed} onChange={(e) => setTyped(e.target.value)} spellCheck={false} />
          </label>
        </>
      )}
      {error && <p className="problem error">{error}</p>}
      <div className="button-row">
        <button className="danger" disabled={busy || !ready} onClick={resize}>
          {busy ? "Resizing…" : "Resize"}
        </button>
        <button onClick={onClose} disabled={busy}>
          Close
        </button>
      </div>
    </div>
  );
}
