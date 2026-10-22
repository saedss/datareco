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
