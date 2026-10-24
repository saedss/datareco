import { useCallback, useEffect, useRef, useState } from "react";
import {
  DiskHealth,
  DiskMap,
  HealthReport,
  Operation,
  PartitionTable,
  Segment,
  SpeedStatus,
  SurfaceProgress,
  TableView,
  UnfinishedMove,
  api,
  driveLetter,
  toCommandError,
} from "../api";
import { formatNumber, formatSize } from "../format";
import { ApplyBox, CreateForm, EditActions, EditForm, FieldForm, PendingChanges, UndoBox } from "./TableEditing";
import { FormatForm, LabelForm, LetterForm, WipeForm, rangeOf, typeNote } from "./VolumeForms";
import { MoveForm, ResizeForm } from "./MoveForms";
import { useActivity } from "../activity";

type Props = {
  sourceId: string;
  /** The source's name for people ("Disk 1"). */
  sourceName: string;
  sectorSize: number;
  /** Partition table changes waiting for Apply (kept by the editor). */
  ops: Operation[];
  onOps: (ops: Operation[]) => void;
  /** Called once a table change is written, so other views read it again. */
  onTableChanged: () => void;
  /** Shows a byte offset of the source in the hex editor. */
  onShowInSectors: (offset: number) => void;
  /** Copies a partition to another disk (in Back up & clone). */
  onCopyPartition: (startLba: number) => void;
};

/** The Partitions tab: a map of the disk's partitions and free space, and
 *  changes to them: the table (written only on Apply), formatting, wiping,
 *  moving and resizing. */
export function DiskView({
  sourceId,
  sourceName,
  sectorSize,
  ops,
  onOps,
  onTableChanged,
  onShowInSectors,
  onCopyPartition,
}: Props) {
  const [view, setView] = useState<TableView | null>(null);
  const [mapError, setMapError] = useState<string | null>(null);
  const [selectedKey, setSelectedKey] = useState<string | null>(null);
  const [form, setForm] = useState<EditForm | null>(null);
  const [writing, setWriting] = useState(false);
  const [writeError, setWriteError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [version, setVersion] = useState(0);
  const [unfinishedMove, setUnfinishedMove] = useState<UnfinishedMove | null>(null);
  const latest = useRef(0);

  const loadMap = useCallback(() => {
    const request = ++latest.current;
    setMapError(null);
    api
      .planTable(sourceId, ops)
      .then((v) => request === latest.current && setView(v))
      .catch((e) => request === latest.current && setMapError(toCommandError(e).message));
  }, [sourceId, ops]);

  useEffect(() => {
    loadMap();
  }, [loadMap, version]);

  // A move of this disk that didn't finish (stopped, or a power cut).
  useEffect(() => {
    api
      .unfinishedMove(sourceId)
      .then(setUnfinishedMove)
      .catch(() => setUnfinishedMove(null));
  }, [sourceId, version]);

  const map: DiskMap | null = view?.map ?? null;
  const keyOf = (s: Segment) => `${s.kind}:${s.startLba}`;
  const selected = map && selectedKey ? map.segments.findIndex((s) => keyOf(s) === selectedKey) : -1;
  const segment = map && selected >= 0 ? map.segments[selected] : null;
  const select = (index: number) => {
    const s = map?.segments[index];
    setSelectedKey(s ? keyOf(s) : null);
  };
  const addOp = (op: Operation) => {
    setNotice(null);
    onOps([...ops, op]);
  };
  const canEdit = !!view && !view.notEditable && !view.plan.blocked && !writing;

  async function apply(confirmation: string) {
    setWriting(true);
    setWriteError(null);
    try {
      const { backup } = await api.applyTable(sourceId, ops, confirmation);
      setForm(null);
      setNotice(`Written. The old table is saved in ${backup}; "Undo a change" puts it back.`);
      onOps([]);
      onTableChanged();
      setVersion((v) => v + 1);
    } catch (e) {
      setWriteError(toCommandError(e).message);
    } finally {
      setWriting(false);
    }
  }

  /** A format, wipe, label or letter change finished. */
  function done(message: string) {
    setForm(null);
    setNotice(message);
    onTableChanged();
    setVersion((v) => v + 1);
  }

  /** A move form closed: a stopped move leaves a journal to carry on from. */
  function closeMove() {
    setForm(null);
    setVersion((v) => v + 1);
  }

  /** "partition 2 (E: Photos)", or the whole source. */
  function describe(s: Segment | null): string {
    if (!s || s.kind === "wholeDisk") return `all of ${sourceName}`;
    const name = map ? segmentName(s, map.table) : "";
    return s.partition != null ? `partition ${s.partition} (${name})` : name;
  }

  async function undo(backup: string, confirmation: string) {
    setWriting(true);
    setWriteError(null);
    try {
      await api.undoTable(sourceId, backup, confirmation);
      setForm(null);
      setNotice("The old table is back. (The one it replaced is saved too.)");
      onTableChanged();
      setVersion((v) => v + 1);
    } catch (e) {
      setWriteError(toCommandError(e).message);
    } finally {
      setWriting(false);
    }
  }

  return (
    <div className="disk-view">
      <section className="disk-section">
        <div className="disk-section-head">
          <h2>{ops.length > 0 ? "Disk map, with the changes" : "Disk map"}</h2>
          <button onClick={() => setVersion((v) => v + 1)}>Refresh</button>
        </div>
        {mapError && (
          <div className="banner error" role="alert">
            {mapError}
          </div>
        )}
        {!map && !mapError && <p className="muted small">Reading the partition table…</p>}
        {unfinishedMove?.progress && form?.kind !== "resumeMove" && (
          <div className="banner warning" role="alert">
            A partition move on this disk didn't finish ({formatSize(unfinishedMove.progress.done)} of{" "}
            {formatSize(unfinishedMove.progress.total)} copied). Its journal keeps it safe; carry it on before changing
            anything else.{" "}
            <button onClick={() => setForm({ kind: "resumeMove" })}>Carry on…</button>
          </div>
        )}
        {unfinishedMove && !unfinishedMove.progress && (
          <div className="banner error" role="alert">
            A partition move on this disk didn't finish, and can't carry on: {unfinishedMove.problem} DataReco won't
            change this disk while that journal is there.
          </div>
        )}
        {map && view && (
          <>
            <p className="muted small">{tableSummary(map.table)}</p>
            <DiskBar map={map} sectorSize={sectorSize} selected={selected} onSelect={select} />
            <SegmentTable map={map} sectorSize={sectorSize} selected={selected} onSelect={select} />
            {segment && ops.length === 0 && (
              <div className="button-row">
                <button onClick={() => onShowInSectors(segment.startLba * sectorSize)}>Show in the hex editor</button>
              </div>
            )}
            {map.table.problems.map((problem) => (
              <p key={problem.message} className={`problem ${problem.severity}`}>
                {problem.message}
              </p>
            ))}
            {notice && <p className="notice-line small">{notice}</p>}
            {view.notEditable || view.plan.blocked ? (
              <>
                <p className="muted small">Partition table changes: {view.notEditable ?? view.plan.blocked}</p>
                {!view.notEditable && (
                  <div className="button-row">
                    <button onClick={() => setForm({ kind: "wipe", segment: null })}>Wipe the whole disk…</button>
                  </div>
                )}
              </>
            ) : (
              <EditActions
                view={view}
                segment={segment}
                sectorSize={sectorSize}
                disabled={
                  !canEdit ||
                  unfinishedMove != null ||
                  form?.kind === "apply" ||
                  form?.kind === "undo" ||
                  form?.kind === "wipe" ||
                  form?.kind === "move" ||
                  form?.kind === "resumeMove"
                }
                pending={ops.length}
                isDisk={sourceId.startsWith("disk:")}
                onOp={addOp}
                onForm={(f) => {
                  setWriteError(null);
                  setForm(f);
                }}
                onCopy={(s) => onCopyPartition(s.startLba)}
              />
            )}
            {form?.kind === "create" && (
              <CreateForm
                view={view}
                segment={form.segment}
                sectorSize={sectorSize}
                onOp={addOp}
                onClose={() => setForm(null)}
              />
            )}
            {(form?.kind === "type" || form?.kind === "name") && (
              <FieldForm key={`${form.kind}-${form.startLba}`} form={form} view={view} onOp={addOp} onClose={() => setForm(null)} />
            )}
            {ops.length > 0 && (
              <PendingChanges
                view={view}
                ops={ops}
                disabled={writing || form?.kind === "apply"}
                onRemove={(i) => onOps(ops.filter((_, j) => j !== i))}
                onDiscard={() => {
                  setForm(null);
                  onOps([]);
                }}
                onApply={() => {
                  setWriteError(null);
                  setForm({ kind: "apply" });
                }}
              />
            )}
            {form?.kind === "apply" && (
              <ApplyBox
                view={view}
                diskName={sourceName}
                busy={writing}
                error={writeError}
                onApply={apply}
                onCancel={() => setForm(null)}
              />
            )}
            {form?.kind === "format" && (
              <FormatForm
                key={`format-${form.segment.startLba}`}
                sourceId={sourceId}
                what={describe(form.segment)}
                range={rangeOf(form.segment)}
                note={
                  sourceId.startsWith("image:")
                    ? null
                    : typeNote(map.table.partitions.find((p) => p.number === form.segment.partition))
                }
                onDone={done}
                onClose={() => setForm(null)}
              />
            )}
            {form?.kind === "wipe" && (
              <WipeForm
                key={`wipe-${form.segment?.startLba ?? "disk"}`}
                sourceId={sourceId}
                what={describe(form.segment)}
                range={rangeOf(form.segment)}
                onDone={done}
                onClose={() => setForm(null)}
              />
            )}
            {form?.kind === "move" && (
              <MoveForm
                key={`move-${form.segment.startLba}-${form.align ? "align" : ""}`}
                sourceId={sourceId}
                sectorSize={sectorSize}
                what={describe(form.segment)}
                segment={form.segment}
                segments={map.segments}
                align={form.align}
                onDone={done}
                onClose={closeMove}
              />
            )}
            {form?.kind === "resumeMove" && (
              <MoveForm
                sourceId={sourceId}
                sectorSize={sectorSize}
                what="the partition"
                segment={null}
                segments={map.segments}
                unfinished={unfinishedMove?.progress ?? null}
                onDone={done}
                onClose={closeMove}
              />
            )}
            {form?.kind === "resize" && (
              <ResizeForm
                sourceId={sourceId}
                segment={form.segment}
                what={describe(form.segment)}
                onDone={done}
                onClose={() => setForm(null)}
              />
            )}
            {form?.kind === "label" && (
              <LabelForm sourceId={sourceId} segment={form.segment} onDone={done} onClose={() => setForm(null)} />
            )}
            {form?.kind === "letter" && (
              <LetterForm sourceId={sourceId} segment={form.segment} onDone={done} onClose={() => setForm(null)} />
            )}
            {form?.kind === "undo" && (
              <UndoBox
                sourceId={sourceId}
                view={view}
                diskName={sourceName}
                busy={writing}
                error={writeError}
                onUndo={undo}
                onCancel={() => setForm(null)}
              />
            )}
          </>
        )}
      </section>
    </div>
  );
}

// ---------------------------------------------------------------- the map

export function tableSummary(table: PartitionTable): string {
  const size = formatSize(table.diskSectors * table.sectorSize);
  switch (table.kind) {
    case "mbr":
      return `MBR partition table · ${size} · ${table.sectorSize}-byte sectors`;
    case "gpt":
      return `GPT partition table · ${size} · ${table.sectorSize}-byte sectors`;
    case "none":
      return `No partition table · ${size}`;
  }
}

/** What a segment is called on the map: its drive letter, or what it is. */
export function segmentName(segment: Segment, table: PartitionTable): string {
  const letter = segment.volume ? driveLetter(segment.volume) : null;
  switch (segment.kind) {
    case "free":
      return segment.logical ? "Free (extended)" : "Free space";
    case "unusable":
      return "Unusable";
    case "wholeDisk":
      return letter ?? "Whole disk";
    case "partition": {
      const p = table.partitions.find((p) => p.number === segment.partition);
      const type = p && p.typeName !== "unknown" && !p.typeName.startsWith("NTFS, exFAT") ? p.typeName : null;
      return letter ?? p?.name ?? type ?? `Partition ${segment.partition}`;
    }
  }
}

/** The file system: as Windows reports it when mounted, else as DataReco recognized it. */
function fileSystemName(segment: Segment, table: PartitionTable): string | null {
  if (segment.volume?.fileSystem) return segment.volume.fileSystem;
  if (segment.kind === "wholeDisk") return table.wholeDisk?.name ?? null;
  const p = table.partitions.find((p) => p.number === segment.partition);
  return p?.fileSystem?.name ?? null;
}

function fileSystemKind(segment: Segment, table: PartitionTable): string | null {
  if (segment.kind === "wholeDisk") return table.wholeDisk?.kind ?? null;
  const p = table.partitions.find((p) => p.number === segment.partition);
  return p?.fileSystem?.kind ?? null;
}

/** The colour group of a segment. */
function segmentClass(segment: Segment, table: PartitionTable): string {
  if (segment.kind === "free") return "free";
  if (segment.kind === "unusable") return "unusable";
  const kind = fileSystemKind(segment, table) ?? segment.volume?.fileSystem?.toLowerCase() ?? "";
  if (kind === "ntfs" || kind === "refs") return "ntfs";
  if (kind.startsWith("fat") || kind === "exfat") return "fat";
  if (kind.startsWith("ext") || kind === "btrfs") return "linux";
  if (kind === "hfsplus" || kind === "apfs") return "apple";
  return "other";
}

/** How full a mounted volume is, 0–1, when Windows says. */
function usedShare(segment: Segment): number | null {
  const v = segment.volume;
  if (!v || v.free === null || !v.size) return null;
  return Math.min(1, Math.max(0, (v.size - v.free) / v.size));
}

type MapProps = {
  map: DiskMap;
  sectorSize: number;
  selected: number | null;
  onSelect: (index: number) => void;
};

/** The disk as a bar: each segment as wide as its share of the disk (with a
 *  minimum, so small partitions stay visible); logical partitions inside a
 *  box for the extended partition. */
export function DiskBar({ map, sectorSize, selected, onSelect }: MapProps) {
  const groups: { logical: boolean; items: [Segment, number][] }[] = [];
  map.segments.forEach((segment, index) => {
    const last = groups[groups.length - 1];
    if (last && last.logical && segment.logical) last.items.push([segment, index]);
    else groups.push({ logical: segment.logical, items: [[segment, index]] });
  });
  // Flex grow factors that add up to less than 1 leave space unfilled, so
  // each level's shares are of what that level shows.
  const sectorsOf = (items: [Segment, number][]) => Math.max(1, items.reduce((sum, [s]) => sum + s.sectors, 0));
  const total = sectorsOf(groups.flatMap((g) => g.items));

  const cell = (share: number) => ([segment, index]: [Segment, number]) => {
    const used = usedShare(segment);
    const name = segmentName(segment, map.table);
    const fs = fileSystemName(segment, map.table);
    return (
      <button
        key={index}
        className={`disk-seg ${segmentClass(segment, map.table)}${index === selected ? " selected" : ""}`}
        style={{ flexGrow: segment.sectors / share }}
        onClick={() => onSelect(index)}
        title={[name, fs, formatSize(segment.sectors * sectorSize), segment.note].filter(Boolean).join(" · ")}
      >
        <span className="disk-seg-name">{name}</span>
        <span className="disk-seg-size">{formatSize(segment.sectors * sectorSize)}</span>
        {used !== null && (
          <span className="disk-seg-used" aria-label={`${Math.round(used * 100)}% used`}>
            <span style={{ width: `${used * 100}%` }} />
          </span>
        )}
      </button>
    );
  };

  return (
    <div className="disk-bar">
      {groups.map((group) =>
        group.logical ? (
          <div
            key={`extended-${group.items[0][1]}`}
            className="disk-bar-extended"
            style={{ flexGrow: sectorsOf(group.items) / total }}
            title="Extended partition: holds the logical partitions"
          >
            {group.items.map(cell(sectorsOf(group.items)))}
          </div>
        ) : (
          group.items.map(cell(total))
        ),
      )}
    </div>
  );
}

export function SegmentTable({ map, sectorSize, selected, onSelect }: MapProps) {
  return (
    <div className="disk-table-box">
      <table className="file-table disk-table">
        <thead>
          <tr>
            <th>Partition</th>
            <th>File system</th>
            <th>Label</th>
            <th className="number">Size</th>
            <th className="number">Used</th>
            <th className="number">Free</th>
            <th>Type</th>
            <th className="number">Start sector</th>
          </tr>
        </thead>
        <tbody>
          {map.segments.map((segment, index) => {
            const p = map.table.partitions.find((p) => p.number === segment.partition);
            const v = segment.volume;
            const used = v && v.size && v.free !== null ? v.size - v.free : null;
            const type =
              segment.kind === "partition"
                ? `${p?.typeName ?? ""}${segment.logical ? " (logical)" : ""}${p?.bootable ? " · active" : ""}`
                : (segment.note ?? (segment.kind === "wholeDisk" ? "No partition table" : ""));
            return (
              <tr
                key={index}
                className={index === selected ? "selected" : ""}
                tabIndex={0}
                onClick={() => onSelect(index)}
                onKeyDown={(e) => e.key === "Enter" && onSelect(index)}
              >
                <td>
                  <span className={`disk-swatch ${segmentClass(segment, map.table)}`} />
                  {segment.partition !== null && <span className="muted">{segment.partition} </span>}
                  {segmentName(segment, map.table)}
                </td>
                <td>{fileSystemName(segment, map.table) ?? (segment.kind === "partition" ? "Not recognized" : "")}</td>
                <td>{v?.label ?? p?.fileSystem?.label ?? ""}</td>
                <td className="number">{formatSize(segment.sectors * sectorSize)}</td>
                <td className="number">{used !== null ? formatSize(used) : ""}</td>
                <td className="number">{v?.free != null ? formatSize(v.free) : ""}</td>
                <td className="disk-type" title={type}>
                  {type}
                </td>
                <td className="number">{formatNumber(segment.startLba)}</td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

// ---------------------------------------------------------------- health

const VERDICT: Record<string, string> = { good: "Good", caution: "Caution", bad: "Bad", unknown: "Unknown" };
const LEVEL_CLASS: Record<string, string> = { bad: "error", caution: "warning", info: "info" };

export function HealthSection({ sourceId }: { sourceId: string }) {
  const [health, setHealth] = useState<DiskHealth | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [details, setDetails] = useState(false);

  const load = useCallback(() => {
    setError(null);
    api
      .diskHealth(sourceId)
      .then(setHealth)
      .catch((e) => setError(toCommandError(e).message));
  }, [sourceId]);

  useEffect(() => {
    load();
  }, [load]);

  const report = health?.report;
  return (
    <section className="disk-section">
      <div className="disk-section-head">
        <h2>Health{health?.disk != null && sourceId.startsWith("volume:") ? ` of Disk ${health.disk}` : ""}</h2>
        <button onClick={load}>Check again</button>
      </div>
      {error && (
        <div className="banner error" role="alert">
          {error}
        </div>
      )}
      {!report && !error && <p className="muted small">Asking the drive…</p>}
      {report && (
        <>
          <div className="health-verdict">
            <span className={`verdict ${report.verdict}`}>{VERDICT[report.verdict]}</span>
            <div>
              <p>{report.summary}</p>
              {report.source && <p className="muted small">From {report.source}.</p>}
            </div>
          </div>
          {report.findings.map((finding) => (
            <p key={finding.message} className={`problem ${LEVEL_CLASS[finding.level]}`}>
              {finding.message}
            </p>
          ))}
          <HealthFacts report={report} />
          {(report.attributes.length > 0 || report.nvme) && (
            <button className="link-button" onClick={() => setDetails((d) => !d)}>
              {details ? "Hide details" : "Show all values"}
            </button>
          )}
          {details && <HealthDetails report={report} />}
        </>
      )}
    </section>
  );
}

function HealthFacts({ report }: { report: HealthReport }) {
  const facts: [string, string][] = [];
  if (report.temperature !== null) facts.push(["Temperature", `${report.temperature} °C`]);
  if (report.powerOnHours !== null)
    facts.push(["Powered on", `${formatNumber(report.powerOnHours)} h (${(report.powerOnHours / 8766).toFixed(1)} years)`]);
  if (report.powerCycles !== null) facts.push(["Power cycles", formatNumber(report.powerCycles)]);
  if (report.lifeUsed !== null) facts.push(["Rated life used", `${report.lifeUsed}%`]);
  if (report.bytesWritten !== null) facts.push(["Written", formatSize(report.bytesWritten)]);
  if (report.bytesRead !== null) facts.push(["Read", formatSize(report.bytesRead)]);
  if (report.solidState) facts.push(["Kind", "Solid-state"]);
  else if (report.rpm) facts.push(["Kind", `Spinning, ${formatNumber(report.rpm)} rpm`]);
  if (facts.length === 0) return null;
  return (
    <dl className="health-facts">
      {facts.map(([name, value]) => (
        <div key={name}>
          <dt>{name}</dt>
          <dd>{value}</dd>
        </div>
      ))}
    </dl>
  );
}

const ATTRIBUTE_STATE: Record<string, string> = {
  ok: "",
  watch: "Watch",
  failedBefore: "Failed before",
  failing: "Failing",
};

function HealthDetails({ report }: { report: HealthReport }) {
  if (report.nvme) {
    const n = report.nvme;
    const rows: [string, string][] = [
      ["Critical warning", n.criticalWarning === 0 ? "None" : `0x${n.criticalWarning.toString(16).padStart(2, "0")}`],
      ["Available spare", `${n.availableSpare}% (warns below ${n.spareThreshold}%)`],
      ["Rated life used", `${n.percentageUsed}%`],
      ["Data read", formatSize(n.bytesRead)],
      ["Data written", formatSize(n.bytesWritten)],
      ["Read commands", formatNumber(n.hostReads)],
      ["Write commands", formatNumber(n.hostWrites)],
      ["Busy time", `${formatNumber(n.busyMinutes)} minutes`],
      ["Power cycles", formatNumber(n.powerCycles)],
      ["Power-on hours", formatNumber(n.powerOnHours)],
      ["Unsafe shutdowns", formatNumber(n.unsafeShutdowns)],
      ["Media and data integrity errors", formatNumber(n.mediaErrors)],
      ["Error log entries", formatNumber(n.errorLogEntries)],
      ["Time above warning temperature", `${formatNumber(n.warningTemperatureMinutes)} minutes`],
      ["Time above critical temperature", `${formatNumber(n.criticalTemperatureMinutes)} minutes`],
    ];
    return (
      <table className="file-table health-table">
        <tbody>
          {rows.map(([name, value]) => (
            <tr key={name}>
              <td>{name}</td>
              <td className="number">{value}</td>
            </tr>
          ))}
        </tbody>
      </table>
    );
  }
  return (
    <table className="file-table health-table">
      <thead>
        <tr>
          <th className="number">ID</th>
          <th>Attribute</th>
          <th className="number">Value</th>
          <th className="number">Worst</th>
          <th className="number">Threshold</th>
          <th className="number">Raw</th>
          <th />
        </tr>
      </thead>
      <tbody>
        {report.attributes.map((a) => (
          <tr key={a.id} className={a.state === "ok" ? "" : `attribute-${a.state}`}>
            <td className="number">{a.id}</td>
            <td>
              {a.name}
              {a.prefail && <span className="muted small"> · pre-failure</span>}
            </td>
            <td className="number">{a.current}</td>
            <td className="number">{a.worst}</td>
            <td className="number">{a.threshold ?? "–"}</td>
            <td className="number">{formatNumber(a.raw)}</td>
            <td>{ATTRIBUTE_STATE[a.state]}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

// ---------------------------------------------------------------- surface test

const SPEED_CLASS: Record<string, string> = {
  ".": "untried",
  f: "fast",
  n: "normal",
  s: "slow",
  v: "veryslow",
  x: "bad",
};

type SurfaceProps = {
  sourceId: string;
  sectorSize: number;
  /** The partitions that can be tested on their own. */
  partitions: { number: number; name: string }[];
  onShowInSectors: (offset: number) => void;
};

export function SurfaceSection({ sourceId, sectorSize, partitions, onShowInSectors }: SurfaceProps) {
  const [picked, setPicked] = useState<number | null>(null);
  const pickedName = partitions.find((p) => p.number === picked)?.name ?? null;
  const [progress, setProgress] = useState<SurfaceProgress | null>(null);
  const [running, setRunning] = useState(false);
  const [ended, setEnded] = useState(false);
  const [what, setWhat] = useState<string>("");
  const [error, setError] = useState<string | null>(null);
  const pausing = useRef(false);

  // Leaving the disk stops its test.
  useEffect(
    () => () => {
      pausing.current = true;
      api.stopSurfaceTest(sourceId).catch(() => {});
    },
    [sourceId],
  );

  async function run(first: () => Promise<SurfaceProgress>) {
    pausing.current = false;
    setRunning(true);
    setError(null);
    try {
      let current = await first();
      setProgress(current);
      while (!current.done && !pausing.current) {
        current = await api.continueSurfaceTest(sourceId);
        setProgress(current);
      }
      if (current.done) setEnded(true);
    } catch (e) {
      setError(toCommandError(e).message);
    } finally {
      setRunning(false);
    }
  }

  function start(partition: number | null, label: string) {
    setEnded(false);
    setWhat(label);
    run(() => api.startSurfaceTest(sourceId, partition));
  }

  async function stop() {
    pausing.current = true;
    const last = await api.stopSurfaceTest(sourceId).catch(() => null);
    if (last) setProgress(last);
    setEnded(true);
  }

  const paused = !running && !ended && progress !== null && !progress.done;
  const percent = progress && progress.sectors > 0 ? (progress.tested / progress.sectors) * 100 : 0;
  const remaining =
    progress && progress.bytesPerSecond > 0
      ? ((progress.sectors - progress.tested) * sectorSize) / progress.bytesPerSecond
      : null;

  useActivity(
    `${sourceId}:surface`,
    sourceId,
    progress
      ? {
          label: what ? `Surface test of ${what}` : "Surface test",
          detail:
            `${formatSize(progress.tested * sectorSize)} of ${formatSize(progress.sectors * sectorSize)}` +
            (progress.unreadableSectors > 0 ? ` · ${formatSize(progress.unreadableSectors * sectorSize)} unreadable` : ""),
          percent: Math.round(percent * 10) / 10,
          state: error ? "failed" : running ? "running" : progress.done ? "finished" : ended ? "stopped" : "paused",
          view: "health",
        }
      : null,
    {
      pause: () => (pausing.current = true),
      resume: () => run(() => api.continueSurfaceTest(sourceId)),
      stop,
    },
  );

  return (
    <section className="disk-section">
      <div className="disk-section-head">
        <h2>Surface test</h2>
      </div>
      <p className="muted small">
        Reads every sector once and times it, to find slow and unreadable areas. Nothing is written or repaired. On a
        drive that's failing, rescue it first (Back up &amp; clone): every read wears it further.
      </p>
      <div className="button-row">
        <button className="primary" disabled={running} onClick={() => start(null, "the whole disk")}>
          Test the whole disk
        </button>
        {partitions.length > 0 && (
          <>
            <select
              aria-label="Partition to test on its own"
              value={picked ?? ""}
              disabled={running}
              onChange={(e) => setPicked(e.target.value === "" ? null : Number(e.target.value))}
            >
              <option value="">Or one partition…</option>
              {partitions.map((p) => (
                <option key={p.number} value={p.number}>
                  {p.name}
                </option>
              ))}
            </select>
            {picked !== null && (
              <button disabled={running} onClick={() => start(picked, pickedName ?? `partition ${picked}`)}>
                Test {pickedName ?? `partition ${picked}`} only
              </button>
            )}
          </>
        )}
        {running && <button onClick={() => (pausing.current = true)}>Pause</button>}
        {paused && <button onClick={() => run(() => api.continueSurfaceTest(sourceId))}>Carry on</button>}
        {(running || paused) && (
          <button className="stop" onClick={stop}>
            Stop
          </button>
        )}
      </div>
      {error && (
        <div className="banner error" role="alert">
          {error}
        </div>
      )}
      {progress && (
        <>
          <div className="scan-progress">
            <progress max={100} value={percent} />
            <p className="small">
              {progress.done ? "Done: " : running ? "Testing " : ended ? "Stopped: " : "Paused: "}
              {what && `${what}, `}
              {formatSize(progress.tested * sectorSize)} of {formatSize(progress.sectors * sectorSize)}
              {progress.bytesPerSecond > 0 && ` · ${formatSize(progress.bytesPerSecond)}/s`}
              {running && remaining !== null && ` · about ${duration(remaining)} left`}
            </p>
            {progress.stopped && <p className="problem error">{progress.stopped}</p>}
          </div>
          <div className="copy-map-box">
            <div className="surface-map" role="img" aria-label="Map of read times">
              {Array.from(progress.picture).map((symbol, i) => (
                <span key={i} className={`sq ${SPEED_CLASS[symbol] ?? "untried"}`} />
              ))}
            </div>
            <div className="copy-legend small">
              <span>
                <span className="sq fast" /> Under 20 ms ({formatNumber(progress.counts.fast)})
              </span>
              <span>
                <span className="sq normal" /> Under 100 ms ({formatNumber(progress.counts.normal)})
              </span>
              <span>
                <span className="sq slow" /> Under 500 ms ({formatNumber(progress.counts.slow)})
              </span>
              <span>
                <span className="sq veryslow" /> Slower ({formatNumber(progress.counts.verySlow)})
              </span>
              <span>
                <span className="sq bad" /> Unreadable ({formatNumber(progress.counts.unreadable)})
              </span>
              <span>
                <span className="sq untried" /> Not tested yet
              </span>
            </div>
            <p className="muted small">Counts are of 256 KiB blocks. Each square shows the slowest block in its part.</p>
          </div>
          {progress.badRangeCount > 0 && (
            <div className="surface-bad">
              <p>
                <strong>{formatSize(progress.unreadableSectors * sectorSize)}</strong> couldn't be read, in{" "}
                {formatNumber(progress.badRangeCount)} place{progress.badRangeCount === 1 ? "" : "s"}. Rescue this
                drive into an image (Back up &amp; clone) and recover from that: the rescue copy goes back over
                unreadable areas in smaller pieces.
              </p>
              <ul>
                {progress.badRanges.slice(0, 20).map((range) => (
                  <li key={range.startLba}>
                    <button className="link-button" onClick={() => onShowInSectors(range.startLba * sectorSize)}>
                      Sectors {formatNumber(range.startLba)}–{formatNumber(range.startLba + range.sectors - 1)}
                    </button>{" "}
                    <span className="muted">({formatSize(range.sectors * sectorSize)})</span>
                  </li>
                ))}
              </ul>
              {progress.badRangeCount > 20 && (
                <p className="muted small">… and {formatNumber(progress.badRangeCount - 20)} more.</p>
              )}
            </div>
          )}
        </>
      )}
    </section>
  );
}

/** 3725 → "1 h 2 min". */
function duration(seconds: number): string {
  if (seconds < 60) return "a minute";
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes} min`;
  return `${Math.floor(minutes / 60)} h ${minutes % 60} min`;
}

// ---------------------------------------------------------------- speed test

const PLACE: Record<string, string> = { start: "At the start", middle: "In the middle", end: "At the end" };

export function SpeedSection({ sourceId }: { sourceId: string }) {
  const [status, setStatus] = useState<SpeedStatus | null>(null);
  const [running, setRunning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const stopping = useRef(false);

  useEffect(
    () => () => {
      stopping.current = true;
      api.stopSpeedTest(sourceId).catch(() => {});
    },
    [sourceId],
  );

  async function start() {
    stopping.current = false;
    setRunning(true);
    setError(null);
    try {
      let current = await api.startSpeedTest(sourceId);
      setStatus(current);
      while (!current.done && !stopping.current) {
        current = await api.continueSpeedTest(sourceId);
        setStatus(current);
      }
      if (!current.done) await api.stopSpeedTest(sourceId);
    } catch (e) {
      setError(toCommandError(e).message);
    } finally {
      setRunning(false);
    }
  }

  useActivity(
    `${sourceId}:speed`,
    sourceId,
    running || status
      ? {
          label: "Read speed test",
          detail: "",
          percent: null,
          state: error ? "failed" : running ? "running" : "finished",
          view: "health",
        }
      : null,
    { stop: () => (stopping.current = true) },
  );

  return (
    <section className="disk-section">
      <div className="disk-section-head">
        <h2>Read speed</h2>
      </div>
      <p className="muted small">
        Reads big blocks at the start, middle and end of the disk, then small ones at random places, for about 12
        seconds. One request at a time, so SSDs show less than their makers quote. Write speed isn't tested: writing
        could overwrite deleted files you may want back.
      </p>
      <div className="button-row">
        <button className="primary" disabled={running} onClick={start}>
          Test read speed
        </button>
        {running && (
          <button className="stop" onClick={() => (stopping.current = true)}>
            Stop
          </button>
        )}
      </div>
      {error && (
        <div className="banner error" role="alert">
          {error}
        </div>
      )}
      {status && (
        <>
          {!status.done && (
            <div className="scan-progress">
              <progress max={100} value={status.percent} />
            </div>
          )}
          {status.stopped && <p className="problem error">{status.stopped}</p>}
          {(status.sequential.length > 0 || status.random) && (
            <table className="file-table speed-table">
              <tbody>
                {status.sequential.map((s) => (
                  <tr key={s.place}>
                    <td>{PLACE[s.place]}, 1 MiB at a time</td>
                    <td className="number">{formatSize(s.bytesPerSecond)}/s</td>
                  </tr>
                ))}
                {status.random && (
                  <tr>
                    <td>Random places, 4 KiB at a time</td>
                    <td className="number">
                      {formatNumber(status.random.readsPerSecond)} reads/s · {formatSize(status.random.bytesPerSecond)}
                      /s ·{" "}
                      {status.random.averageMicros < 100
                        ? `${status.random.averageMicros} µs`
                        : `${(status.random.averageMicros / 1000).toFixed(2)} ms`}{" "}
                      each
                    </td>
                  </tr>
                )}
              </tbody>
            </table>
          )}
          {status.errors > 0 && (
            <p className="problem warning">
              {formatNumber(status.errors)} read{status.errors === 1 ? "" : "s"} failed. A surface test shows where.
            </p>
          )}
        </>
      )}
    </section>
  );
}
