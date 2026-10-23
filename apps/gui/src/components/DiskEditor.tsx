import { FormEvent, useCallback, useEffect, useRef, useState } from "react";
import { ByteData, CommandError, SourceInfo, TemplateList, api, toCommandError } from "../api";
import { formatNumber, formatSize, hex, parseHexBytes, parseNumber, utf16Bytes, utf8Bytes } from "../format";
import { BookmarksPanel } from "./BookmarksPanel";
import { BYTES_PER_ROW, HexGrid, Range } from "./HexGrid";
import { StructurePanel } from "./StructurePanel";

type Props = {
  sourceId: string;
  title: string;
  subtitle: string;
  templates: TemplateList | null;
  onReloadTemplates: () => void;
  onRestartAsAdmin: () => void;
};

/** The disk editor (view only): hex grid, navigation, search and structures. */
export function DiskEditor({ sourceId, title, subtitle, templates, onReloadTemplates, onRestartAsAdmin }: Props) {
  const [info, setInfo] = useState<SourceInfo | null>(null);
  const [error, setError] = useState<CommandError | null>(null);
  const [topRow, setTopRow] = useState(0);
  const [rows, setRows] = useState(32);
  const [cursor, setCursor] = useState(0);
  const [selection, setSelection] = useState<Range | null>(null);
  const [data, setData] = useState<ByteData | null>(null);
  const [status, setStatus] = useState<string | null>(null);
  const [tab, setTab] = useState<"structure" | "bookmarks">("structure");
  const latestRead = useRef(0);

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
  }, [info, topRow, rows]);

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
        <EditorHeader title={title} subtitle={subtitle} info={info} />
        <AdminNeeded onRestart={onRestartAsAdmin} />
      </section>
    );
  }

  return (
    <section className="editor">
      <EditorHeader title={title} subtitle={subtitle} info={info} />
      {error && (
        <div className="banner error" role="alert">
          {error.message}
        </div>
      )}
      {info && (
        <>
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
                <button role="tab" aria-selected={tab === "bookmarks"} onClick={() => setTab("bookmarks")}>
                  Bookmarks
                </button>
              </div>
              <div className="panel-body">
                {tab === "structure" ? (
                  <StructurePanel
                    sourceId={info.id}
                    sectorSize={info.sectorSize}
                    cursor={cursor}
                    templates={templates}
                    onReloadTemplates={onReloadTemplates}
                    onSelectRange={(range) => reveal(range.start, range)}
                  />
                ) : (
                  <BookmarksPanel
                    sourceId={info.id}
                    sectorSize={info.sectorSize}
                    cursor={cursor}
                    onJump={(offset) => reveal(offset, null)}
                  />
                )}
              </div>
            </aside>
          </div>
          <CursorInfo info={info} cursor={cursor} data={data} selection={selection} status={status} />
        </>
      )}
    </section>
  );
}

function EditorHeader({ title, subtitle, info }: { title: string; subtitle: string; info: SourceInfo | null }) {
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
        {info && (
          <p className="muted view-size">
            {formatSize(info.size)} · {formatNumber(info.sectorCount)} sectors of {info.sectorSize} bytes
          </p>
        )}
        <span className="badge" title="Nothing is ever written to your disks in this mode">
          Read-only
        </span>
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
