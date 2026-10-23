const UNITS = ["bytes", "KiB", "MiB", "GiB", "TiB", "PiB"];

/** 1536 → "1.5 KiB", like the command-line tool. */
export function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} bytes`;
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < UNITS.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toFixed(1)} ${UNITS[unit]}`;
}

/** 1953525168 → "1,953,525,168". */
export function formatNumber(value: number): string {
  return value.toLocaleString("en-US");
}

/** Lower-case hex, zero-padded to `width` digits. */
export function hex(value: number, width: number): string {
  return value.toString(16).padStart(width, "0");
}

/** The character shown for a byte in the text column. */
export function printable(byte: number): string {
  return byte >= 0x20 && byte < 0x7f ? String.fromCharCode(byte) : ".";
}

/** "2048", "0x800" or "0X800" → 2048; anything else → null. */
export function parseNumber(text: string): number | null {
  const t = text.trim().replace(/_/g, "").toLowerCase();
  if (/^0x[0-9a-f]+$/.test(t)) return parseInt(t.slice(2), 16);
  if (/^[0-9]+$/.test(t)) return Number(t);
  return null;
}

/** "55 AA" or "55aa" → [0x55, 0xAA]; null if it isn't whole hex bytes. */
export function parseHexBytes(text: string): number[] | null {
  const digits = text.replace(/\s+/g, "");
  if (digits.length === 0 || digits.length % 2 !== 0 || !/^[0-9a-fA-F]+$/.test(digits)) return null;
  const bytes = [];
  for (let i = 0; i < digits.length; i += 2) bytes.push(parseInt(digits.slice(i, i + 2), 16));
  return bytes;
}

/** Text as UTF-8 bytes (plain ASCII stays one byte per letter). */
export function utf8Bytes(text: string): number[] {
  return Array.from(new TextEncoder().encode(text));
}

/** Text as UTF-16 little-endian bytes, as Windows and NTFS store names. */
export function utf16Bytes(text: string): number[] {
  const bytes = [];
  for (let i = 0; i < text.length; i++) {
    const unit = text.charCodeAt(i);
    bytes.push(unit & 0xff, unit >> 8);
  }
  return bytes;
}
