import { KeyboardEvent, PointerEvent, WheelEvent, useEffect, useRef, useState } from "react";
import { ByteData } from "../api";
import { formatNumber, hex, printable } from "../format";

export const BYTES_PER_ROW = 16;
/** Must match `.hex-row` height in App.css. */
const ROW_HEIGHT = 20;
const COLUMN_HEADER = Array.from({ length: BYTES_PER_ROW }, (_, i) => hex(i, 2));

/** A run of bytes to highlight. */
export type Range = { start: number; length: number };

type Props = {
  size: number;
  sectorSize: number;
  /** First row shown; row r covers bytes r*16 … r*16+15. */
  topRow: number;
  data: ByteData | null;
  cursor: number;
  selection: Range | null;
  onCursor: (offset: number) => void;
  onScroll: (topRow: number) => void;
  /** Called with how many rows fit, whenever that changes. */
  onRowsChange: (rows: number) => void;
};

/**
 * The hex and text view. Only the rows on screen are drawn, so it works the
 * same for a floppy image and a 20 TB disk; the scrollbar is its own, since a
 * browser can't make a page billions of rows tall.
 */
export function HexGrid({ size, sectorSize, topRow, data, cursor, selection, ...props }: Props) {
  const rowsRef = useRef<HTMLDivElement>(null);
  const [height, setHeight] = useState(0);
  const rows = Math.max(1, Math.floor(height / ROW_HEIGHT));
  const totalRows = Math.ceil(size / BYTES_PER_ROW);
  const maxTop = Math.max(0, totalRows - rows);
  // Wide enough for the largest offset on this source, at least 8 digits.
  const offsetDigits = Math.max(8, Math.max(0, size - 1).toString(16).length);
  const { onRowsChange } = props;

  useEffect(() => {
    const element = rowsRef.current;
    if (!element) return;
    const observer = new ResizeObserver(([entry]) => setHeight(entry.contentRect.height));
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  useEffect(() => onRowsChange(rows), [rows, onRowsChange]);

  const scrollTo = (row: number) => props.onScroll(Math.min(Math.max(0, Math.round(row)), maxTop));

  function onWheel(event: WheelEvent) {
    const rowsMoved = Math.max(1, Math.round(Math.abs(event.deltaY) / ROW_HEIGHT));
    scrollTo(topRow + Math.sign(event.deltaY) * rowsMoved);
  }

  function onKeyDown(event: KeyboardEvent) {
    const rowStart = cursor - (cursor % BYTES_PER_ROW);
    const targets: Record<string, number> = {
      ArrowLeft: cursor - 1,
      ArrowRight: cursor + 1,
      ArrowUp: cursor - BYTES_PER_ROW,
      ArrowDown: cursor + BYTES_PER_ROW,
      PageUp: cursor - rows * BYTES_PER_ROW,
      PageDown: cursor + rows * BYTES_PER_ROW,
      Home: event.ctrlKey ? 0 : rowStart,
      End: event.ctrlKey ? size - 1 : rowStart + BYTES_PER_ROW - 1,
    };
    const target = targets[event.key];
    if (target === undefined) return;
    event.preventDefault();
    props.onCursor(Math.min(Math.max(0, target), size - 1));
  }

  const unreadable = new Set(data?.unreadable ?? []);
  const lines = [];
  for (let r = 0; r < rows; r++) {
    const rowOffset = (topRow + r) * BYTES_PER_ROW;
    if (rowOffset >= size) break;
    const sectorStart = rowOffset % sectorSize === 0;
    const cells = [];
    const chars = [];
    for (let i = 0; i < BYTES_PER_ROW; i++) {
      const offset = rowOffset + i;
      if (offset >= size) break;
      const byte = byteAt(data, offset);
      const classes = [];
      if (byte === null) classes.push("loading");
      else if (byte === 0) classes.push("zero");
      if (offset === cursor) classes.push("cursor");
      if (selection && offset >= selection.start && offset < selection.start + selection.length) {
        classes.push("selected");
      }
      if (unreadable.has(Math.floor(offset / sectorSize))) classes.push("bad");
      const className = classes.join(" ");
      const select = () => props.onCursor(offset);
      cells.push(
        <span key={i} className={`hex-byte ${className}`} onMouseDown={select}>
          {byte === null ? "··" : hex(byte, 2)}
        </span>,
      );
      chars.push(
        <span key={i} className={`hex-char ${className}`} onMouseDown={select}>
          {byte === null ? " " : printable(byte)}
        </span>,
      );
    }
    lines.push(
      <div key={rowOffset} className={sectorStart ? "hex-row sector-start" : "hex-row"}>
        <span className="hex-sector">{sectorStart ? formatNumber(rowOffset / sectorSize) : ""}</span>
        <span className="hex-offset" style={{ width: `${offsetDigits}ch` }}>
          {hex(rowOffset, offsetDigits)}
        </span>
        <span className="hex-bytes">{cells}</span>
        <span className="hex-text">{chars}</span>
      </div>,
    );
  }

  return (
    <div className="hex-grid">
      <div className="hex-row hex-header" aria-hidden="true">
        <span className="hex-sector">sector</span>
        <span className="hex-offset" style={{ width: `${offsetDigits}ch` }}>
          offset
        </span>
        <span className="hex-bytes">
          {COLUMN_HEADER.map((label) => (
            <span key={label} className="hex-byte">
              {label}
            </span>
          ))}
        </span>
        <span className="hex-text">text</span>
      </div>
      <div className="hex-body">
        <div
          className="hex-rows"
          ref={rowsRef}
          tabIndex={0}
          onKeyDown={onKeyDown}
          onWheel={onWheel}
          role="grid"
          aria-label="Disk contents in hex and text. Arrow keys move the cursor."
        >
          {lines}
        </div>
        <Scrollbar
          height={height}
          topRow={topRow}
          maxTop={maxTop}
          rows={rows}
          totalRows={totalRows}
          onScroll={scrollTo}
        />
      </div>
    </div>
  );
}

function byteAt(data: ByteData | null, offset: number): number | null {
  if (!data || offset < data.offset || offset >= data.offset + data.data.length) return null;
  return data.data[offset - data.offset];
}

type ScrollbarProps = {
  height: number;
  topRow: number;
  maxTop: number;
  rows: number;
  totalRows: number;
  onScroll: (row: number) => void;
};

function Scrollbar({ height, topRow, maxTop, rows, totalRows, onScroll }: ScrollbarProps) {
  const thumbHeight = Math.min(height, Math.max(24, (height * rows) / Math.max(1, totalRows)));
  const travel = Math.max(1, height - thumbHeight);
  const thumbTop = maxTop === 0 ? 0 : (topRow / maxTop) * travel;
  const drag = useRef<{ y: number; top: number } | null>(null);

  function onTrackDown(event: PointerEvent<HTMLDivElement>) {
    const y = event.clientY - event.currentTarget.getBoundingClientRect().top;
    onScroll(y < thumbTop ? topRow - rows : topRow + rows);
  }

  function onThumbDown(event: PointerEvent<HTMLDivElement>) {
    event.stopPropagation();
    event.currentTarget.setPointerCapture(event.pointerId);
    drag.current = { y: event.clientY, top: topRow };
  }

  function onThumbMove(event: PointerEvent<HTMLDivElement>) {
    if (!drag.current) return;
    onScroll(drag.current.top + ((event.clientY - drag.current.y) / travel) * maxTop);
  }

  return (
    <div className="scrollbar" onPointerDown={onTrackDown} aria-hidden="true">
      <div
        className="scrollbar-thumb"
        style={{ top: thumbTop, height: thumbHeight }}
        onPointerDown={onThumbDown}
        onPointerMove={onThumbMove}
        onPointerUp={() => (drag.current = null)}
      />
    </div>
  );
}
