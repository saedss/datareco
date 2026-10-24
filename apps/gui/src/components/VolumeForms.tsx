import { useEffect, useRef, useState } from "react";
import { FormatPreview, Segment, SpotInfo, WipeProgress, api, driveLetter, toCommandError } from "../api";
import { formatSize } from "../format";

/** A partition's sectors, or null for the whole source. */
type Range = { startLba: number; sectors: number } | null;

export function rangeOf(segment: Segment | null): Range {
  if (!segment || segment.kind === "wholeDisk") return null;
  return { startLba: segment.startLba, sectors: segment.sectors };
}

type Props = {
  sourceId: string;
  /** What's formatted or wiped, for people ("E: (Photos)", "all of Disk 2"). */
  what: string;
  range: Range;
  /** Formatting: why Windows won't show it afterwards, if it won't. */
  note?: string | null;
  onDone: (message: string) => void;
  onClose: () => void;
};
/** Partition types Windows mounts: FAT and NTFS ones on MBR, basic data on GPT. */
const WINDOWS_MBR_TYPES = ["0x01", "0x04", "0x06", "0x07", "0x0B", "0x0C", "0x0E"];
const BASIC_DATA = "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7";

/** Why Windows won't show a partition once formatted, if it won't. */
export function typeNote(partition: { typeCode: string; hidden: boolean } | undefined): string | null {
  if (!partition) return null;
  if (partition.hidden) return "It's hidden, so Windows won't show it until it's unhidden.";
  const windows = partition.typeCode.startsWith("0x")
    ? WINDOWS_MBR_TYPES.includes(partition.typeCode.toUpperCase().replace("0X", "0x"))
    : partition.typeCode.toUpperCase() === BASIC_DATA;
  return windows
    ? null
    : "Its partition type isn't one Windows uses, so Windows won't show it until its type is changed (Change type…, to NTFS or FAT32).";
}


const CLUSTERS: [string, number | null][] = [
  ["Usual for its size", null],
  ["4 KiB", 4096],
  ["32 KiB", 32 * 1024],
  ["128 KiB", 128 * 1024],
];

/** Formatting a partition: FAT32 and exFAT by DataReco, NTFS by Windows. */
export function FormatForm({ sourceId, what, range, note, onDone, onClose }: Props) {
  const isImage = sourceId.startsWith("image:");
  const [fileSystem, setFileSystem] = useState(isImage ? "exfat" : "ntfs");
  const [label, setLabel] = useState("");
  const [cluster, setCluster] = useState<number | null>(null);
  const [full, setFull] = useState(false);
  const [preview, setPreview] = useState<FormatPreview | null>(null);
  const [typed, setTyped] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let current = true;
    setError(null);
    api
      .previewFormat(sourceId, range?.startLba ?? null, range?.sectors ?? null, fileSystem, label.trim() || null, cluster)
      .then((p) => current && setPreview(p))
      .catch((e) => current && setError(toCommandError(e).message));
    return () => {
      current = false;
    };
  }, [sourceId, range?.startLba, range?.sectors, fileSystem, label, cluster]);

  async function format() {
    setBusy(true);
    setError(null);
    try {
      const message = await api.formatPartition(
        sourceId,
        range?.startLba ?? null,
        range?.sectors ?? null,
        fileSystem,
        label.trim() || null,
        cluster,
        full,
        typed,
      );
      onDone(message);
    } catch (e) {
      setError(toCommandError(e).message);
    } finally {
      setBusy(false);
    }
  }

  const spot = preview?.spot;
  const summary = preview?.summary;
  const ready = preview && !preview.problem && spot && typed.trim().toLowerCase() === spot.confirmation.toLowerCase();
  return (
    <div className="confirm-erase">
      <h2>Format {what}</h2>
      <div className="edit-row">
        <label>
          File system{" "}
          <select value={fileSystem} onChange={(e) => setFileSystem(e.target.value)}>
            {!isImage && <option value="ntfs">NTFS (Windows)</option>}
            <option value="exfat">exFAT (USB sticks, cards, Macs)</option>
            <option value="fat32">FAT32 (older devices; files up to 4 GiB)</option>
          </select>
        </label>
        <label>
          Label <input value={label} maxLength={fileSystem === "ntfs" ? 32 : 11} onChange={(e) => setLabel(e.target.value)} />
        </label>
        {fileSystem !== "ntfs" ? (
          <label>
            Clusters{" "}
            <select value={cluster ?? ""} onChange={(e) => setCluster(e.target.value ? Number(e.target.value) : null)}>
              {CLUSTERS.map(([name, size]) => (
                <option key={name} value={size ?? ""}>
                  {name}
                </option>
              ))}
            </select>
          </label>
        ) : (
          <label className="checkbox" title="Reads and checks every sector: slow on big disks">
            <input type="checkbox" checked={full} onChange={(e) => setFull(e.target.checked)} /> Full format
          </label>
        )}
      </div>
      {summary && (
        <p className="small">
          {summary.kind === "fat32" ? "FAT32" : "exFAT"}, clusters of {formatSize(summary.clusterSize)}:{" "}
          {formatSize(summary.capacity)} for files.
        </p>
      )}
      {fileSystem === "ntfs" && !preview?.problem && (
        <p className="small">Windows' own formatter makes the NTFS file system{full ? ", reading every sector first" : ""}.</p>
      )}
      {preview?.problem && <p className="problem warning">{preview.problem}</p>}
      {!preview?.problem && note && <p className="problem info">{note}</p>}
      <p className="small">
        Everything on it is erased. Until new files overwrite them, a scan may still find the old ones.
      </p>
      {spot && spot.closing.length > 0 && (
        <p className="small">
          Closed while it's formatted: <strong>{spot.closing.join(", ")}</strong>.
        </p>
      )}
      {spot && (
        <label className="confirm-type">
          Type <code>{spot.confirmation}</code> to confirm:{" "}
          <input value={typed} onChange={(e) => setTyped(e.target.value)} spellCheck={false} />
        </label>
      )}
      {error && <p className="problem error">{error}</p>}
      <div className="button-row">
        <button className="danger" disabled={busy || !ready} onClick={format}>
          {busy ? "Formatting…" : "Format"}
        </button>
        <button onClick={onClose} disabled={busy}>
          Cancel
        </button>
      </div>
    </div>
  );
}

/** Wiping a partition or the whole disk, with progress and a way to stop. */
export function WipeForm({ sourceId, what, range, onDone, onClose }: Props) {
  const [pattern, setPattern] = useState<"zeros" | "random">("zeros");
  const [spot, setSpot] = useState<SpotInfo | null>(null);
  const [typed, setTyped] = useState("");
  const [progress, setProgress] = useState<WipeProgress | null>(null);
  const [running, setRunning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const stopping = useRef(false);

  useEffect(() => {
    api
      .previewWipe(sourceId, range?.startLba ?? null, range?.sectors ?? null)
      .then(setSpot)
      .catch((e) => setError(toCommandError(e).message));
  }, [sourceId, range?.startLba, range?.sectors]);

  // Leaving stops the wipe; what's written stays written.
  useEffect(
    () => () => {
      stopping.current = true;
    },
    [],
  );

  async function wipe() {
    stopping.current = false;
    setRunning(true);
    setError(null);
    try {
      let current = await api.startWipe(sourceId, range?.startLba ?? null, range?.sectors ?? null, pattern, typed);
      setProgress(current);
      while (!current.finished && !stopping.current) {
        current = await api.continueWipe(sourceId);
        setProgress(current);
      }
      if (current.finished) {
        onDone(`Wiped ${what}.`);
      } else {
        const last = await api.stopWipe(sourceId);
        if (last) setProgress(last);
      }
    } catch (e) {
      setError(toCommandError(e).message);
    } finally {
      setRunning(false);
    }
  }

  const percent = progress && progress.total > 0 ? (progress.done / progress.total) * 100 : 0;
  const ready = spot && typed.trim().toLowerCase() === spot.confirmation.toLowerCase();
  return (
    <div className="confirm-erase">
      <h2>Wipe {what}</h2>
      <p className="small">
        Writes over {spot ? formatSize(spot.size) : "it"}, so nothing on it can be recovered afterwards, by DataReco or
        anything else.{range === null && " The partition table goes too: the disk will show as empty."}
      </p>
      <p className="small muted">
        SSDs and USB sticks keep spare blocks the computer can't reach, which may still hold old data.
      </p>
      <div className="edit-row">
        <label>
          Write{" "}
          <select value={pattern} onChange={(e) => setPattern(e.target.value as "zeros" | "random")} disabled={running}>
            <option value="zeros">zeros</option>
            <option value="random">random bytes</option>
          </select>
        </label>
      </div>
      {spot && spot.closing.length > 0 && (
        <p className="small">
          Closed while it's wiped: <strong>{spot.closing.join(", ")}</strong>.
        </p>
      )}
      {spot && !running && !progress?.finished && (
        <label className="confirm-type">
          Type <code>{spot.confirmation}</code> to confirm:{" "}
          <input value={typed} onChange={(e) => setTyped(e.target.value)} spellCheck={false} />
        </label>
      )}
      {progress && (
        <div className="scan-progress">
          <progress max={100} value={percent} />
          <p className="small">
            {formatSize(progress.done)} of {formatSize(progress.total)}
            {progress.bytesPerSecond > 0 && ` · ${formatSize(progress.bytesPerSecond)}/s`}
            {!running && !progress.finished && " · stopped (what's written stays written)"}
          </p>
        </div>
      )}
      {error && <p className="problem error">{error}</p>}
      <div className="button-row">
        {running ? (
          <button className="stop" onClick={() => (stopping.current = true)}>
            Stop
          </button>
        ) : (
          <button className="danger" disabled={!ready} onClick={wipe}>
            {progress ? "Wipe from the start again" : "Wipe"}
          </button>
        )}
        <button onClick={onClose} disabled={running}>
          Close
        </button>
      </div>
    </div>
  );
}

type VolumeProps = {
  sourceId: string;
  segment: Segment;
  onDone: (message: string) => void;
  onClose: () => void;
};

/** A mounted volume's label. */
export function LabelForm({ sourceId, segment, onDone, onClose }: VolumeProps) {
  const [label, setLabel] = useState(segment.volume?.label ?? "");
  const [error, setError] = useState<string | null>(null);
  async function save() {
    setError(null);
    try {
      await api.setLabel(sourceId, segment.kind === "wholeDisk" ? null : segment.startLba, label);
      onDone(label.trim() ? `Labelled "${label.trim()}".` : "Label removed.");
    } catch (e) {
      setError(toCommandError(e).message);
    }
  }
  return (
    <div className="edit-form">
      <h3>Change the label</h3>
      <div className="edit-row">
        <input value={label} maxLength={32} onChange={(e) => setLabel(e.target.value)} aria-label="Label" autoFocus />
      </div>
      {error && <p className="problem error">{error}</p>}
      <div className="button-row">
        <button className="primary" onClick={save}>
          Save
        </button>
        <button onClick={onClose}>Cancel</button>
      </div>
    </div>
  );
}

/** A mounted volume's drive letter, from the ones free. */
export function LetterForm({ sourceId, segment, onDone, onClose }: VolumeProps) {
  const current = segment.volume ? driveLetter(segment.volume) : null;
  const [used, setUsed] = useState<string[] | null>(null);
  const [letter, setLetter] = useState(current?.[0] ?? "");
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    api
      .listDevices()
      .then((d) => setUsed(d.volumes.flatMap((v) => v.mountPoints.filter((m) => /^[A-Z]:\\$/i.test(m)).map((m) => m[0].toUpperCase()))))
      .catch(() => setUsed([]));
  }, []);
  const free = "CDEFGHIJKLMNOPQRSTUVWXYZ".split("").filter((l) => !used?.includes(l) || l === current?.[0]);
  async function save() {
    setError(null);
    try {
      await api.setLetter(sourceId, segment.kind === "wholeDisk" ? null : segment.startLba, letter || null);
      onDone(letter ? `It's ${letter}: now.` : "It has no drive letter now.");
    } catch (e) {
      setError(toCommandError(e).message);
    }
  }
  return (
    <div className="edit-form">
      <h3>Change the drive letter</h3>
      <p className="muted small">Programs that remember the old letter (shortcuts, settings) won't find it there anymore.</p>
      <div className="edit-row">
        <select value={letter} onChange={(e) => setLetter(e.target.value)} aria-label="Drive letter" disabled={!used}>
          <option value="">No letter</option>
          {free.map((l) => (
            <option key={l} value={l}>
              {l}:
            </option>
          ))}
        </select>
      </div>
      {error && <p className="problem error">{error}</p>}
      <div className="button-row">
        <button className="primary" onClick={save} disabled={!used}>
          Save
        </button>
        <button onClick={onClose}>Cancel</button>
      </div>
    </div>
  );
}
