import { useEffect, useRef, useState } from "react";
import { FileSystemInfo, FoundPartition, Operation, PartitionTable, ScanMode, SearchStatus, api, toCommandError } from "../api";
import { useActivity } from "../activity";
import { formatNumber, formatSize } from "../format";

type TableProps = {
  sourceId: string;
  sectorSize: number;
  /** Shows the first sector of a partition. */
  onJump: (offset: number) => void;
};

/** The partition table and its problems, for the hex editor's side panel. */
export function PartitionsPanel({ sourceId, sectorSize, onJump }: TableProps) {
  const [table, setTable] = useState<PartitionTable | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    api
      .readPartitions(sourceId)
      .then(setTable)
      .catch((e) => setError(toCommandError(e).message));
  }, [sourceId]);

  const jump = (lba: number) => onJump(lba * sectorSize);

  return (
    <div className="partitions-panel">
      <section className="panel-section">
        <h3>Partition table</h3>
        {error && <p className="problem error">{error}</p>}
        {!table && !error && <p className="muted small">Reading…</p>}
        {table && <p>{tableSummary(table)}</p>}
        {table?.problems.map((problem) => (
          <p key={problem.message} className={`problem ${problem.severity}`}>
            {problem.message}
          </p>
        ))}
      </section>

      {table && table.partitions.length > 0 && (
        <ul className="partition-list">
          {table.partitions.map((p) => (
            <li key={`${p.number}-${p.startLba}`}>
              <button className={p.kind === "extended" ? "partition extended" : "partition"} onClick={() => jump(p.startLba)}>
                <span className="partition-line">
                  <span>
                    <strong>{p.number}</strong> {p.typeName}
                    {p.name && ` "${p.name}"`}
                  </span>
                  <span className="muted">{formatSize(p.sectors * sectorSize)}</span>
                </span>
                <span className="muted small">
                  {p.kind === "extended" ? "Extended partition (holds logical ones)" : fileSystemText(p.fileSystem)} · LBA{" "}
                  {formatNumber(p.startLba)}
                  {p.bootable && " · bootable"}
                </span>
              </button>
            </li>
          ))}
        </ul>
      )}
      <p className="muted small panel-section">Lost partitions are searched for in Recover data.</p>
    </div>
  );
}

type SearchProps = TableProps & {
  /** Adds putting a found partition back in the table to the Partitions
   *  tab's pending changes; `needsTable` when the disk has none to add it to. */
  onRestore: (op: Operation, needsTable: boolean) => void;
};

/** The search for lost partitions, from boot sectors and other traces. */
export function LostPartitionSearch({ sourceId, sectorSize, onJump, onRestore }: SearchProps) {
  const [table, setTable] = useState<PartitionTable | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [search, setSearch] = useState<SearchStatus | null>(null);
  const [running, setRunning] = useState(false);
  const stopped = useRef(false);

  useEffect(() => {
    api
      .readPartitions(sourceId)
      .then(setTable)
      .catch(() => setTable(null));
    // Closing the disk stops a running search.
    return () => {
      stopped.current = true;
      api.stopPartitionSearch(sourceId).catch(() => {});
    };
  }, [sourceId]);

  async function run(mode: ScanMode) {
    stopped.current = false;
    setRunning(true);
    setError(null);
    try {
      let status = await api.startPartitionSearch(sourceId, mode);
      setSearch(status);
      while (!status.progress.done && !stopped.current) {
        status = await api.continuePartitionSearch(sourceId);
        setSearch(status);
      }
      if (stopped.current) await api.stopPartitionSearch(sourceId);
    } catch (e) {
      setError(toCommandError(e).message);
    } finally {
      setRunning(false);
    }
  }

  const jump = (lba: number) => onJump(lba * sectorSize);
  const p = search?.progress;
  useActivity(
    `${sourceId}:partsearch`,
    sourceId,
    p
      ? {
          label: search.mode === "full" ? "Deep search for lost partitions" : "Quick search for lost partitions",
          detail: p.done ? `${search.found.length} found` : `${formatSize(p.position)} of ${formatSize(p.total)}`,
          percent: p.total > 0 ? Math.round((p.position / p.total) * 1000) / 10 : null,
          state: error ? "failed" : running ? "running" : p.done ? "finished" : "stopped",
          view: "recover",
        }
      : null,
    { stop: () => (stopped.current = true) },
  );

  return (
    <div className="partitions-panel">
      <section className="panel-section">
        {error && <p className="problem error">{error}</p>}
        <div className="button-row">
          <button onClick={() => run("quick")} disabled={running} title="Where partitions usually start. Fast.">
            Quick search
          </button>
          <button onClick={() => run("full")} disabled={running} title="Every sector. Finds more; slow on big disks.">
            Deep search (every sector)
          </button>
          {running && (
            <button onClick={() => (stopped.current = true)} className="stop">
              Stop
            </button>
          )}
        </div>
        {search && (
          <>
            <progress value={search.progress.position} max={search.progress.total || 1} />
            <p className="muted small">{searchText(search, running)}</p>
          </>
        )}
      </section>

      {search && search.found.length > 0 && (
        <ul className="partition-list">
          {search.found.map((f) => (
            <li key={`${f.startLba}-${f.sectors}-${f.fileSystem?.kind}`}>
              <button className="partition" onClick={() => jump(f.startLba)}>
                <span className="partition-line">
                  <span>
                    <span className={`confidence ${f.confidence}`}>{f.confidence}</span> {fileSystemText(f.fileSystem)}
                  </span>
                  <span className="muted">{formatSize(f.sectors * sectorSize)}</span>
                </span>
                <span className="muted small">
                  LBA {formatNumber(f.startLba)}
                  {f.typeName && ` · ${f.typeName}`}
                  {f.name && ` "${f.name}"`} · {f.inTable ? "in the partition table" : "not in the partition table"}
                </span>
                <ul className="evidence">
                  {f.evidence.map((clue) => (
                    <li key={clue}>{clue}</li>
                  ))}
                </ul>
              </button>
              {!f.inTable && (
                <button
                  className="link-button restore-button"
                  title="Adds it to the pending changes in Partitions; nothing is written until you apply them"
                  onClick={() => onRestore(restoreOp(f), table?.kind === "none")}
                >
                  Put it back in the partition table…
                </button>
              )}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

/** Creating a partition where a found one is, typed for its file system. */
function restoreOp(found: FoundPartition): Operation {
  const kind = found.fileSystem?.kind;
  const typeCode =
    kind === "fat32"
      ? "fat32"
      : kind === "fat16" || kind === "fat12"
        ? "fat16"
        : kind === "ext2" || kind === "ext3" || kind === "ext4" || kind === "btrfs"
          ? "linux"
          : kind === "hfsplus" || kind === "apfs"
            ? kind
            : "ntfs";
  return { op: "create", startLba: found.startLba, sectors: found.sectors, typeCode, logical: false, name: null };
}

function tableSummary(table: PartitionTable): string {
  switch (table.kind) {
    case "mbr":
      return `MBR partition table, disk signature ${table.diskId}.`;
    case "gpt":
      return `GPT partition table, disk GUID ${table.diskId}.`;
    case "none":
      return table.wholeDisk
        ? `No partition table: this is one ${fileSystemText(table.wholeDisk)} volume.`
        : "No partition table.";
  }
}

function fileSystemText(fs: FileSystemInfo | null): string {
  if (!fs) return "No file system recognized";
  return fs.label ? `${fs.name} "${fs.label}"` : fs.name;
}

function searchText(search: SearchStatus, running: boolean): string {
  const { position, total, clues, done } = search.progress;
  const kind = search.mode === "full" ? "Deep" : "Quick";
  if (!done) {
    const where = `${formatSize(position)} of ${formatSize(total)}`;
    return running ? `${kind} search… ${where}, ${clues} clues so far.` : `${kind} search stopped at ${where}.`;
  }
  if (search.found.length === 0) {
    return search.mode === "quick"
      ? "Quick search done: nothing found. The deep search reads every sector and can find more."
      : "Deep search done: nothing found.";
  }
  return `${kind} search done: ${search.found.length} partition${search.found.length === 1 ? "" : "s"} found. Click one to see its first sector.`;
}
