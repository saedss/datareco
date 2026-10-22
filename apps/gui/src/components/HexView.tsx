import { hex, printable } from "../format";

const BYTES_PER_ROW = 16;
const COLUMN_HEADER = Array.from({ length: BYTES_PER_ROW }, (_, i) => hex(i, 2));

type Props = {
  /** Byte offset of `bytes[0]` from the start of the source. */
  offset: number;
  bytes: number[];
};

/** Offset, 16 bytes of hex and their text, per row. Zero bytes are dimmed. */
export function HexView({ offset, bytes }: Props) {
  const rows = [];
  for (let start = 0; start < bytes.length; start += BYTES_PER_ROW) {
    const row = bytes.slice(start, start + BYTES_PER_ROW);
    rows.push(
      <div className="hex-row" key={start}>
        <span className="hex-offset">{hex(offset + start, 12)}</span>
        <span className="hex-bytes">
          {row.map((byte, i) => (
            <span key={i} className={byte === 0 ? "hex-byte zero" : "hex-byte"}>
              {hex(byte, 2)}
            </span>
          ))}
        </span>
        <span className="hex-text">{row.map(printable).join("")}</span>
      </div>,
    );
  }
  return (
    <div className="hex" role="table" aria-label="Sector contents">
      <div className="hex-row hex-header" aria-hidden="true">
        <span className="hex-offset">offset</span>
        <span className="hex-bytes">
          {COLUMN_HEADER.map((label) => (
            <span key={label} className="hex-byte">
              {label}
            </span>
          ))}
        </span>
        <span className="hex-text">text</span>
      </div>
      {rows}
    </div>
  );
}
