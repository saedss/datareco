import { FormEvent, useCallback, useEffect, useRef, useState } from "react";
import { ByteData, CommandError, Operation, RawStatus, ScanStatus, SourceInfo, TemplateList, api, toCommandError } from "../api";
import { formatNumber, formatSize, hex, parseHexBytes, parseNumber, utf16Bytes, utf8Bytes } from "../format";
import { BookmarksPanel } from "./BookmarksPanel";
import { CopyView } from "./CopyView";
import { DiskView } from "./DiskView";
import { FileBrowser } from "./FileBrowser";
import { BYTES_PER_ROW, HexGrid, Range } from "./HexGrid";
import { PartitionsPanel } from "./PartitionsPanel";
import { ScanView } from "./ScanView";
import { StructurePanel } from "./StructurePanel";

type Props = {
  sourceId: string;
  title: string;
  subtitle: string;
  templates: TemplateList | null;
  onReloadTemplates: () => void;
  onRestartAsAdmin: () => void;
  /** Opens an image file (a finished copy) in the editor. */
  onOpenImage: (path: string) => void;
};

type View = "sectors" | "disk" | "files" | "scan" | "copy";

/** The disk editor (view only): sectors as hex with navigation, search and
 *  structures; a map of the disk with its health; the files on its volumes;
 *  a scan for lost volumes; or copying it into an image file or onto another
 *  disk. */
export function DiskEditor({
  sourceId,
  title,
  subtitle,
  templates,
  onReloadTemplates,
  onRestartAsAdmin,
  onOpenImage,
}: Props) {
  const [info, setInfo] = useState<SourceInfo | null>(null);
  const [error, setError] = useState<CommandError | null>(null);
  const [topRow, setTopRow] = useState(0);
  const [rows, setRows] = useState(32);
  const [cursor, setCursor] = useState(0);
  const [selection, setSelection] = useState<Range | null>(null);
  const [data, setData] = useState<ByteData | null>(null);
  const [status, setStatus] = useState<string | null>(null);
  const [tab, setTab] = useState<"structure" | "partitions" | "bookmarks">("structure");
  const [view, setView] = useState<View>("sectors");
  // The file browser and scan view start only when first shown (NTFS reads
  // its whole MFT), then stay, so what they show survives switching views.
  const [filesOpened, setFilesOpened] = useState(false);
  const [scanOpened, setScanOpened] = useState(false);
  const [copyOpened, setCopyOpened] = useState(false);
  const [diskOpened, setDiskOpened] = useState(false);
  // Partition table changes waiting for Apply, and a count of the changes
  // written, so views that read the table read it again.
  const [ops, setOps] = useState<Operation[]>([]);
  const [tableVersion, setTableVersion] = useState(0);
  const [scan, setScan] = useState<ScanStatus | null>(null);
  const [raw, setRaw] = useState<RawStatus | null>(null);
  const [browse, setBrowse] = useState<{ key: string; nonce: number } | null>(null);
  const latestRead = useRef(0);

  const showView = useCallback((next: View) => {
    setView(next);
    if (next === "files") setFilesOpened(true);
    if (next === "scan") setScanOpened(true);
    if (next === "copy") setCopyOpened(true);
    if (next === "disk") setDiskOpened(true);
  }, []);

  useEffect(() => {
    api
      .openSource(sourceId)
      .then(setInfo)
      .catch((e) => setError(toCommandError(e)));
  }, [sourceId]);

  // Read what's on screen, plus one row so the cursor values at the last row work.
  useEffect(() => {
    if (!info) return;
    const request = ++latestRead.current;
    api
      .readBytes(info.id, topRow * BYTES_PER_ROW, (rows + 1) * BYTES_PER_ROW)
      .then((bytes) => {
        if (request !== latestRead.current) return;
        setData(bytes);
        setError(null);
      })
      .catch((e) => request === latestRead.current && setError(toCommandError(e)));
  }, [info, topRow, rows, tableVersion]);

  const size = info?.size ?? 0;
  const maxTop = Math.max(0, Math.ceil(size / BYTES_PER_ROW) - rows);

  /** Moves the cursor, scrolling only as much as needed to show it. */
  const moveCursor = useCallback(
    (offset: number) => {
      if (size === 0) return;
      const target = clamp(offset, 0, size - 1);
      const row = Math.floor(target / BYTES_PER_ROW);
      setCursor(target);
      setTopRow((top) => (row < top ? row : row >= top + rows ? row - rows + 1 : top));
    },
    [size, rows],
  );

  /** Moves the cursor and highlights a range; scrolls it near the top if it's off screen. */
  const reveal = useCallback(
    (offset: number, range: Range | null) => {
      if (size === 0) return;
      const target = clamp(offset, 0, size - 1);
      const row = Math.floor(target / BYTES_PER_ROW);
      setCursor(target);
      setSelection(range);
      setTopRow((top) => (row >= top && row < top + rows ? top : clamp(row - 2, 0, maxTop)));
    },
    [size, rows, maxTop],
  );

  if (error?.kind === "access_denied") {
    return (
      <section className="editor">
        <EditorHeader title={title} subtitle={subtitle} info={info} view={null} onView={showView} pending={0} />
        <AdminNeeded onRestart={onRestartAsAdmin} />
      </section>
    );
  }

  return (
    <section className="editor">
      <EditorHeader title={title} subtitle={subtitle} info={info} view={info ? view : null} onView={showView} pending={ops.length} />
      {error && (
        <div className="banner error" role="alert">
          {error.message}
        </div>
      )}
      {info && (
        <>
          <div className="view-pane" hidden={view !== "sectors"}>
            <div className="toolbar">
              <GoTo info={info} onGo={(offset) => reveal(offset, null)} />
              <Search
                info={info}
                cursor={cursor}
                onFound={(offset, length) => reveal(offset, { start: offset, length })}
                onStatus={setStatus}
              />
            </div>
            <div className="editor-body">
              <HexGrid
                size={info.size}
                sectorSize={info.sectorSize}
                topRow={topRow}
                data={data}
                cursor={cursor}
                selection={selection}
                onCursor={(offset) => {
                  setSelection(null);
                  moveCursor(offset);
                }}
                onScroll={setTopRow}
                onRowsChange={setRows}
              />
              <aside className="side-panel">
                <div className="tabs" role="tablist">
                  <button role="tab" aria-selected={tab === "structure"} onClick={() => setTab("structure")}>
                    Structure
                  </button>
                  <button role="tab" aria-selected={tab === "partitions"} onClick={() => setTab("partitions")}>
                    Partitions
                  </button>
                  <button role="tab" aria-selected={tab === "bookmarks"} onClick={() => setTab("bookmarks")}>
                    Bookmarks
                  </button>
                </div>
                {/* All panels stay mounted, so a running search or a decoded
                    structure survives switching tabs. */}
                <div className="panel-body" hidden={tab !== "structure"}>
                  <StructurePanel
                    sourceId={info.id}
                    sectorSize={info.sectorSize}
                    cursor={cursor}
                    templates={templates}
                    onReloadTemplates={onReloadTemplates}
                    onSelectRange={(range) => reveal(range.start, range)}
                  />
                </div>
                <div className="panel-body" hidden={tab !== "partitions"}>
                  <PartitionsPanel
                    key={tableVersion}
                    sourceId={info.id}
                    sectorSize={info.sectorSize}
                    onJump={(offset) => reveal(offset, { start: offset, length: info.sectorSize })}
                    onRestore={(op, needsTable) => {
                      setOps((current) => {
                        const initialized = current.some((o) => o.op === "initialize");
                        const init: Operation[] = needsTable && !initialized ? [{ op: "initialize", style: "mbr" }] : [];
                        return [...current, ...init, op];
                      });
                      showView("disk");
                    }}
                  />
                </div>
                <div className="panel-body" hidden={tab !== "bookmarks"}>
                  <BookmarksPanel
                    sourceId={info.id}
                    sectorSize={info.sectorSize}
                    cursor={cursor}
                    onJump={(offset) => reveal(offset, null)}
                  />
                </div>
              </aside>
            </div>
            <CursorInfo info={info} cursor={cursor} data={data} selection={selection} status={status} />
          </div>
          {diskOpened && (
            <div className="view-pane" hidden={view !== "disk"}>
              <DiskView
                sourceId={info.id}
                sourceName={info.name}
                sectorSize={info.sectorSize}
                ops={ops}
                onOps={setOps}
                onTableChanged={() => setTableVersion((v) => v + 1)}
                onShowInSectors={(offset) => {
                  setView("sectors");
                  reveal(offset, { start: offset, length: info.sectorSize });
                }}
              />
            </div>
          )}
          {filesOpened && (
            <div className="view-pane" hidden={view !== "files"}>
              <FileBrowser
                key={tableVersion}
                sourceId={info.id}
                sectorSize={info.sectorSize}
                scan={scan}
                raw={raw}
                request={browse}
                onShowInSectors={(offset, length) => {
                  setView("sectors");
                  reveal(offset, { start: offset, length: Math.max(1, Math.min(length, info.size - offset)) });
                }}
              />
            </div>
          )}
          {scanOpened && (
            <div className="view-pane" hidden={view !== "scan"}>
              <ScanView
                sourceId={info.id}
                sectorSize={info.sectorSize}
                status={scan}
                onStatus={setScan}
                raw={raw}
                onRaw={setRaw}

                onBrowse={(key) => {
                  setBrowse((current) => ({ key, nonce: (current?.nonce ?? 0) + 1 }));
                  showView("files");
                }}
                onShowInSectors={(offset) => {
                  setView("sectors");
                  reveal(offset, { start: offset, length: info.sectorSize });
                }}
              />
            </div>
          )}
          {copyOpened && (
            <div className="view-pane" hidden={view !== "copy"}>
              <CopyView
                sourceId={info.id}
                sourceName={info.name}
                size={info.size}
                onOpenImage={onOpenImage}
                onRestartAsAdmin={onRestartAsAdmin}
              />
            </div>
          )}
        </>
      )}
    </section>
  );
}

type HeaderProps = {
  title: string;
  subtitle: string;
  info: SourceInfo | null;
  /** The view shown, or null to hide the switch. */
  view: View | null;
  onView: (view: View) => void;
  /** Partition table changes waiting for Apply. */
  pending: number;
};

function EditorHeader({ title, subtitle, info, view, onView, pending }: HeaderProps) {
  return (
    <header className="view-header">
      <div>
        <h1>{title}</h1>
        {subtitle && (
          <p className="muted view-subtitle" title={subtitle}>
            {subtitle}
          </p>
        )}
      </div>
      <div className="view-header-right">
        {view && (
          <div className="view-switch" role="tablist" aria-label="View">
            <button role="tab" aria-selected={view === "sectors"} onClick={() => onView("sectors")}>
              Sectors
            </button>
            <button role="tab" aria-selected={view === "disk"} onClick={() => onView("disk")}>
              Disk
            </button>
            <button role="tab" aria-selected={view === "files"} onClick={() => onView("files")}>
              Files
            </button>
            <button role="tab" aria-selected={view === "scan"} onClick={() => onView("scan")}>
              Scan
            </button>
            <button role="tab" aria-selected={view === "copy"} onClick={() => onView("copy")}>
              Copy
            </button>

          </div>
        )}
        {info && (
          <p className="muted view-size">
            {formatSize(info.size)} · {formatNumber(info.sectorCount)} sectors of {info.sectorSize} bytes
          </p>
        )}
        {pending > 0 ? (
          <span
            className="badge pending-badge"
            title="Partition table changes are waiting in the Disk view. Nothing is written until you apply them and type the disk's name."
          >
            {pending} change{pending === 1 ? "" : "s"} not written
          </span>
        ) : (
          <span
            className="badge"
            title="DataReco writes only when you say so: partition table changes once you apply them, formatting, wiping, and copies onto another disk, each after you type the disk's name."
          >
            Read-only
          </span>
        )}
      </div>
    </header>
  );
}

function GoTo({ info, onGo }: { info: SourceInfo; onGo: (offset: number) => void }) {
  const [mode, setMode] = useState<"sector" | "offset">("sector");
  const [text, setText] = useState("");
  const [invalid, setInvalid] = useState(false);

  function go(event: FormEvent) {
    event.preventDefault();
    const value = parseNumber(text);
    const offset = value === null ? null : mode === "sector" ? value * info.sectorSize : value;
    if (offset === null || offset >= info.size) {
      setInvalid(true);
      return;
    }
    setInvalid(false);
    onGo(offset);
  }

  return (
    <form className="toolbar-group" onSubmit={go}>
      <span className="toolbar-label">Go to</span>
      <select value={mode} onChange={(e) => setMode(e.target.value as "sector" | "offset")} aria-label="Go to what">
        <option value="sector">Sector</option>
        <option value="offset">Offset</option>
      </select>
      <input
        className={invalid ? "invalid" : ""}
        value={text}
        onChange={(e) => {
          setText(e.target.value);
          setInvalid(false);
        }}
        placeholder={mode === "sector" ? "e.g. 2048" : "e.g. 0x100000"}
        spellCheck={false}
        aria-label={mode === "sector" ? "Sector number" : "Byte offset"}
        title={invalid ? "Not a number on this disk (decimal, or hex starting with 0x)" : undefined}
      />
      <button type="submit">Go</button>
    </form>
  );
}

type SearchMode = "text" | "utf16" | "hex";

type SearchProps = {
  info: SourceInfo;
  cursor: number;
  onFound: (offset: number, length: number) => void;
  onStatus: (status: string | null) => void;
};

/** Searches from the cursor, a step at a time so it can show progress and stop. */
function Search({ info, cursor, onFound, onStatus }: SearchProps) {
  const [mode, setMode] = useState<SearchMode>("text");
  const [text, setText] = useState("");
  const [ignoreCase, setIgnoreCase] = useState(true);
  const [running, setRunning] = useState(false);
  const stopped = useRef(false);
  const lastFound = useRef<number | null>(null);

  async function findNext(event: FormEvent) {
    event.preventDefault();
    if (running) {
      stopped.current = true;
      return;
    }
    const pattern = mode === "hex" ? parseHexBytes(text) : mode === "utf16" ? utf16Bytes(text) : utf8Bytes(text);
    if (!pattern || pattern.length === 0) {
      onStatus(mode === "hex" ? "Enter whole hex bytes, e.g. 55 AA." : "Enter something to find.");
      return;
    }
    // Start after the match we're on, otherwise at the cursor.
    let from = lastFound.current === cursor ? cursor + 1 : cursor;
    const startedAt = from;
    stopped.current = false;
    setRunning(true);
    try {
      while (!stopped.current) {
        const progress = await api.find(info.id, pattern, from, ignoreCase && mode !== "hex");
        if (progress.status === "found") {
          lastFound.current = progress.offset;
          onFound(progress.offset, pattern.length);
          onStatus(`Found at offset 0x${hex(progress.offset, 1)} (sector ${formatNumber(Math.floor(progress.offset / info.sectorSize))}).`);
          return;
        }
        if (progress.status === "notFound") {
          onStatus(`Not found between offset 0x${hex(startedAt, 1)} and the end.`);
          return;
        }
        from = progress.next;
        onStatus(`Searching… ${formatSize(from)} of ${formatSize(info.size)}`);
      }
      onStatus(`Search stopped at offset 0x${hex(from, 1)}.`);
    } catch (e) {
      onStatus(`Search failed: ${toCommandError(e).message}`);
    } finally {
      setRunning(false);
    }
  }

  return (
    <form className="toolbar-group" onSubmit={findNext}>
      <span className="toolbar-label">Find</span>
      <select value={mode} onChange={(e) => setMode(e.target.value as SearchMode)} aria-label="Find what">
        <option value="text">Text</option>
        <option value="utf16">Text (UTF-16)</option>
        <option value="hex">Hex bytes</option>
      </select>
      <input
        value={text}
        onChange={(e) => setText(e.target.value)}
        placeholder={mode === "hex" ? "e.g. 55 AA" : "e.g. NTFS"}
        spellCheck={false}
        aria-label="Search for"
      />
      <label className="checkbox">
        <input
          type="checkbox"
          checked={ignoreCase}
          disabled={mode === "hex"}
          onChange={(e) => setIgnoreCase(e.target.checked)}
        />
        Ignore case
      </label>
      <button type="submit">{running ? "Stop" : "Find next"}</button>
    </form>
  );
}

type CursorInfoProps = {
  info: SourceInfo;
  cursor: number;
  data: ByteData | null;
  selection: Range | null;
  status: string | null;
};

/** Where the cursor is, and the bytes there read as numbers. */
function CursorInfo({ info, cursor, data, selection, status }: CursorInfoProps) {
  const bytes = data && cursor >= data.offset ? data.data.slice(cursor - data.offset, cursor - data.offset + 8) : [];
  const view = new DataView(Uint8Array.from(bytes).buffer);
  const values = [
    bytes.length >= 1 && `u8 ${view.getUint8(0)}`,
    bytes.length >= 2 && `u16 ${view.getUint16(0, true)}`,
    bytes.length >= 4 && `u32 ${view.getUint32(0, true)}`,
    bytes.length >= 8 && `u64 ${view.getBigUint64(0, true)}`,
  ].filter(Boolean);
  const sector = Math.floor(cursor / info.sectorSize);
  return (
    <div className="cursor-info">
      <span>
        Offset 0x{hex(cursor, 1)} ({formatNumber(cursor)}) · sector {formatNumber(sector)} + {cursor % info.sectorSize}
      </span>
      {values.length > 0 && <span className="muted">{values.join(" · ")} (little-endian)</span>}
      {selection && <span className="muted">{formatNumber(selection.length)} bytes highlighted</span>}
      {status && <span className="cursor-status">{status}</span>}
    </div>
  );
}

function AdminNeeded({ onRestart }: { onRestart: () => void }) {
  return (
    <div className="notice">
      <h2>Administrator rights needed</h2>
      <p>
        Windows only lets programs read disks and volumes directly when they run as administrator.
        DataReco only reads: it never writes to your disks.
      </p>
      <button className="primary" onClick={onRestart}>
        Restart as administrator
      </button>
    </div>
  );
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(Math.max(value, min), max);
}
