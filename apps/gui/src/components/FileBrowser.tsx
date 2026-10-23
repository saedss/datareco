import { KeyboardEvent, useEffect, useMemo, useState } from "react";
import { EntryId, Extent, FileEntry, FsVolumeInfo, VolumeChoice, api, toCommandError } from "../api";
import { formatNumber, formatSize, formatTime, hex } from "../format";

type Props = {
  sourceId: string;
  sectorSize: number;
  /** Shows a byte range of the source in the sector view. */
  onShowInSectors: (offset: number, length: number) => void;
};

type Folder = { id: EntryId; name: string };

/** Bytes read for a text or hex preview. */
const PREVIEW_BYTES = 64 * 1024;
/** Images up to this size are shown. */
const MAX_IMAGE_BYTES = 16 * 1024 * 1024;
const IMAGE_TYPES: Record<string, string> = {
  png: "image/png",
  jpg: "image/jpeg",
  jpeg: "image/jpeg",
  gif: "image/gif",
  bmp: "image/bmp",
  webp: "image/webp",
  ico: "image/x-icon",
};

/** Browses the FAT, exFAT and NTFS volumes of a source, deleted files included. */
export function FileBrowser({ sourceId, sectorSize, onShowInSectors }: Props) {
  const [volumes, setVolumes] = useState<VolumeChoice[] | null>(null);
  const [volumeOffset, setVolumeOffset] = useState<number | null>(null);
  const [volume, setVolume] = useState<FsVolumeInfo | null>(null);
  const [path, setPath] = useState<Folder[]>([]);
  const [entries, setEntries] = useState<FileEntry[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [showDeleted, setShowDeleted] = useState(true);
  const [selected, setSelected] = useState<FileEntry | null>(null);
  const [reload, setReload] = useState(0);

  useEffect(() => {
    api
      .listVolumes(sourceId)
      .then((list) => {
        setVolumes(list);
        const first = list.find((v) => v.readable);
        if (first) setVolumeOffset(first.offset);
      })
      .catch((e) => setError(toCommandError(e).message));
  }, [sourceId]);

  useEffect(() => {
    if (volumeOffset === null) return;
    let cancelled = false;
    setVolume(null);
    setPath([]);
    setError(null);
    api
      .openFileSystem(sourceId, volumeOffset)
      .then((opened) => {
        if (cancelled) return;
        setVolume(opened.info);
        setPath([{ id: opened.root, name: opened.info.label ?? opened.info.name }]);
      })
      .catch((e) => !cancelled && setError(toCommandError(e).message));
    return () => {
      cancelled = true;
    };
  }, [sourceId, volumeOffset]);

  const folder = path.length > 0 ? path[path.length - 1] : null;
  useEffect(() => {
    if (!folder || volumeOffset === null) return;
    let cancelled = false;
    setEntries(null);
    setSelected(null);
    setError(null);
    api
      .listDirectory(sourceId, volumeOffset, folder.id)
      .then((list) => !cancelled && setEntries(list))
      .catch((e) => !cancelled && setError(toCommandError(e).message));
    return () => {
      cancelled = true;
    };
  }, [sourceId, volumeOffset, folder, reload]);

  const shown = useMemo(
    () => (entries ?? []).filter((e) => showDeleted || !e.deleted).sort(compareEntries),
    [entries, showDeleted],
  );
  const deletedCount = entries?.filter((e) => e.deleted).length ?? 0;

  function open(entry: FileEntry) {
    if (entry.isDir) setPath((current) => [...current, { id: entry.id, name: entry.name }]);
  }

  function onRowKey(event: KeyboardEvent, entry: FileEntry) {
    if (event.key === "Enter") open(entry);
    if (event.key === "Backspace" && path.length > 1) setPath((current) => current.slice(0, -1));
  }

  if (volumes && !volumes.some((v) => v.readable)) {
    return (
      <div className="notice">
        <h2>No volume to browse</h2>
        <p>
          {volumes.length === 0
            ? "No file system was found here. If the partition table is damaged, look for lost partitions in the sector view's Partitions tab."
            : `DataReco reads FAT, exFAT and NTFS so far; this source has ${volumes.map((v) => v.name).join(", ")}.`}
        </p>
      </div>
    );
  }

  return (
    <div className="file-browser">
      <div className="toolbar">
        <label className="toolbar-group">
          <span className="toolbar-label">Volume</span>
          <select
            value={volumeOffset ?? ""}
            onChange={(e) => setVolumeOffset(Number(e.target.value))}
            disabled={!volumes}
          >
            {!volumes && <option value="">Reading the partition table…</option>}
            {volumes?.map((v) => (
              <option key={v.offset} value={v.offset} disabled={!v.readable}>
                {volumeText(v)}
              </option>
            ))}
          </select>
        </label>
        <label className="checkbox">
          <input type="checkbox" checked={showDeleted} onChange={(e) => setShowDeleted(e.target.checked)} />
          Show deleted
        </label>
        <button onClick={() => setReload((n) => n + 1)} disabled={!folder}>
          Reload
        </button>
      </div>

      <nav className="breadcrumbs" aria-label="Folder">
        <button disabled={path.length <= 1} onClick={() => setPath((current) => current.slice(0, -1))} title="Up one folder">
          ↑
        </button>
        {path.map((f, i) => (
          <span key={i} className="crumb">
            {i > 0 && <span className="muted">/</span>}
            <button
              className="link-button"
              disabled={i === path.length - 1}
              onClick={() => setPath((current) => current.slice(0, i + 1))}
            >
              {f.name}
            </button>
          </span>
        ))}
      </nav>

      {error && (
        <div className="banner error" role="alert">
          {error}
        </div>
      )}

      <div className="files-body">
        <div className="file-list">
          {!entries && !error && (
            <p className="muted file-list-status">
              {volume?.kind === "ntfs"
                ? "Reading the folder… The first time, this reads the whole MFT, which can take a while on a big volume."
                : "Reading the folder…"}
            </p>
          )}
          {entries && (
            <table className="file-table">
              <thead>
                <tr>
                  <th>Name</th>
                  <th className="number">Size</th>
                  <th>Modified</th>
                  <th>Status</th>
                </tr>
              </thead>
              <tbody>
                {shown.map((entry) => (
                  <tr
                    key={`${JSON.stringify(entry.id)}-${entry.name}`}
                    tabIndex={0}
                    className={[
                      entry.deleted ? "deleted" : "",
                      selected && sameEntry(selected, entry) ? "selected" : "",
                    ].join(" ")}
                    onClick={() => setSelected(entry)}
                    onDoubleClick={() => open(entry)}
                    onKeyDown={(e) => onRowKey(e, entry)}
                    onFocus={() => setSelected(entry)}
                  >
                    <td className="file-name">
                      <span className="file-icon" aria-hidden>
                        {entry.isDir ? "📁" : "📄"}
                      </span>
                      {entry.name}
                    </td>
                    <td className="number">{entry.isDir ? "" : formatSize(entry.size)}</td>
                    <td>{entry.modified !== null ? formatTime(entry.modified, isFat(volume)) : ""}</td>
                    <td>{entry.deleted && <span className="pill deleted">Deleted</span>}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
          {entries && (
            <p className="muted small file-list-status">
              {formatNumber(shown.length)} item{shown.length === 1 ? "" : "s"}
              {deletedCount > 0 && ` · ${formatNumber(deletedCount)} deleted${showDeleted ? "" : " (hidden)"}`}
            </p>
          )}
        </div>

        <aside className="side-panel file-details">
          {selected && volume && volumeOffset !== null ? (
            <FileDetails
              key={`${JSON.stringify(selected.id)}-${selected.name}`}
              sourceId={sourceId}
              volumeOffset={volumeOffset}
              volume={volume}
              sectorSize={sectorSize}
              entry={selected}
              onOpen={() => open(selected)}
              onShowInSectors={onShowInSectors}
            />
          ) : (
            <p className="muted panel-section">
              {volume ? `${volume.name}${volume.label ? ` "${volume.label}"` : ""}, ${formatSize(volume.size)}, cluster size ${formatSize(volume.clusterSize)}. Select a file to see its details.` : "Opening the volume…"}
            </p>
          )}
        </aside>
      </div>
    </div>
  );
}

type DetailsProps = {
  sourceId: string;
  volumeOffset: number;
  volume: FsVolumeInfo;
  sectorSize: number;
  entry: FileEntry;
  onOpen: () => void;
  onShowInSectors: (offset: number, length: number) => void;
};

type Preview =
  | { kind: "image"; url: string }
  | { kind: "text"; text: string; truncated: boolean }
  | { kind: "hex"; bytes: Uint8Array };

function FileDetails({ sourceId, volumeOffset, volume, sectorSize, entry, onOpen, onShowInSectors }: DetailsProps) {
  const [stream, setStream] = useState<string | null>(null);
  const [extents, setExtents] = useState<Extent[] | null>(null);
  const [preview, setPreview] = useState<Preview | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  const size = stream === null ? entry.size : (entry.streams.find((s) => s.name === stream)?.size ?? 0);
  const asStored = isFat(volume);

  useEffect(() => {
    if (entry.isDir) return;
    let cancelled = false;
    let url: string | null = null;
    setExtents(null);
    setPreview(null);
    setProblem(null);
    api
      .fileExtents(sourceId, volumeOffset, entry.id, stream)
      .then((list) => !cancelled && setExtents(list))
      .catch(() => !cancelled && setExtents([]));
    const imageType = stream === null ? IMAGE_TYPES[extension(entry.name)] : undefined;
    const wanted = imageType && size <= MAX_IMAGE_BYTES ? size : Math.min(size, PREVIEW_BYTES);
    api
      .readFile(sourceId, volumeOffset, entry.id, stream, 0, wanted)
      .then((buffer) => {
        if (cancelled) return;
        const bytes = new Uint8Array(buffer);
        if (imageType && size <= MAX_IMAGE_BYTES) {
          url = URL.createObjectURL(new Blob([bytes], { type: imageType }));
          setPreview({ kind: "image", url });
        } else {
          setPreview(textOrHex(bytes, size));
        }
      })
      .catch((e) => !cancelled && setProblem(toCommandError(e).message));
    return () => {
      cancelled = true;
      if (url) URL.revokeObjectURL(url);
    };
  }, [sourceId, volumeOffset, entry, stream, size]);

  const stored = extents?.filter((e) => !e.sparse) ?? [];
  const first = stored[0];

  return (
    <div className="details">
      <section className="panel-section">
        <h3>{entry.isDir ? "Folder" : "File"}</h3>
        <p className="details-name">
          {entry.name} {entry.deleted && <span className="pill deleted">Deleted</span>}
        </p>
        {entry.note && <p className="problem warning">{entry.note}</p>}
        <dl className="facts">
          {!entry.isDir && (
            <>
              <dt>Size</dt>
              <dd>
                {formatSize(entry.size)} {entry.size >= 1024 && <span className="muted">({formatNumber(entry.size)} bytes)</span>}
              </dd>
            </>
          )}
          {entry.created !== null && (
            <>
              <dt>Created</dt>
              <dd>{formatTime(entry.created, asStored)}</dd>
            </>
          )}
          {entry.modified !== null && (
            <>
              <dt>Modified</dt>
              <dd>{formatTime(entry.modified, asStored)}</dd>
            </>
          )}
          {entry.accessed !== null && (
            <>
              <dt>Accessed</dt>
              <dd>{formatTime(entry.accessed, asStored)}</dd>
            </>
          )}
          {attributeText(entry.attributes) && (
            <>
              <dt>Attributes</dt>
              <dd>{attributeText(entry.attributes)}</dd>
            </>
          )}
        </dl>
        {entry.isDir && (
          <div className="button-row">
            <button onClick={onOpen}>Open folder</button>
          </div>
        )}
      </section>

      {entry.streams.length > 0 && (
        <section className="panel-section">
          <h3>Data streams</h3>
          <div className="chips">
            <button className={stream === null ? "stream-chip active" : "stream-chip"} onClick={() => setStream(null)}>
              Main data · {formatSize(entry.size)}
            </button>
            {entry.streams.map((s) => (
              <button
                key={s.name}
                className={stream === s.name ? "stream-chip active" : "stream-chip"}
                onClick={() => setStream(s.name)}
              >
                {s.name} · {formatSize(s.size)}
              </button>
            ))}
          </div>
        </section>
      )}

      {!entry.isDir && (
        <section className="panel-section">
          <h3>On the disk</h3>
          {!extents && <p className="muted small">Looking…</p>}
          {extents && stored.length === 0 && <p className="muted small">No data stored{size > 0 ? " (all sparse)" : ""}.</p>}
          {first && (
            <>
              <p>
                Starts at sector {formatNumber(Math.floor(first.offset / sectorSize))}{" "}
                <span className="muted">(offset 0x{hex(first.offset, 1)})</span>
                {stored.length > 1 && <>, in {formatNumber(stored.length)} pieces</>}
              </p>
              <div className="button-row">
                <button onClick={() => onShowInSectors(first.offset, first.length)}>Show in sectors</button>
              </div>
            </>
          )}
        </section>
      )}

      {!entry.isDir && (
        <section className="panel-section preview">
          <h3>Preview</h3>
          {problem && <p className="problem error">{problem}</p>}
          {!preview && !problem && <p className="muted small">Reading…</p>}
          {preview?.kind === "image" && <img src={preview.url} alt={entry.name} />}
          {preview?.kind === "text" && (
            <>
              <pre className="text-preview">{preview.text}</pre>
              {preview.truncated && <p className="muted small">Only the start is shown.</p>}
            </>
          )}
          {preview?.kind === "hex" && preview.bytes.length > 0 && (
            <pre className="text-preview hex-preview">{hexPreview(preview.bytes)}</pre>
          )}
          {preview && size === 0 && <p className="muted small">Empty.</p>}
        </section>
      )}
    </div>
  );
}

function volumeText(v: VolumeChoice): string {
  const where = v.partition === null ? "Whole disk" : `Partition ${v.partition}`;
  const label = v.label ? ` "${v.label}"` : "";
  const size = v.size !== null ? ` · ${formatSize(v.size)}` : "";
  return `${where} · ${v.name}${label}${size}${v.readable ? "" : " (not readable yet)"}`;
}

function isFat(volume: FsVolumeInfo | null): boolean {
  return volume?.kind === "fat12" || volume?.kind === "fat16" || volume?.kind === "fat32";
}

/** Folders first, then files, by name; "Lost files" last. */
function compareEntries(a: FileEntry, b: FileEntry): number {
  const rank = (e: FileEntry) => (e.id.kind === "orphans" ? 2 : e.isDir ? 0 : 1);
  return (
    rank(a) - rank(b) ||
    a.name.localeCompare(b.name, undefined, { sensitivity: "base", numeric: true }) ||
    Number(a.deleted) - Number(b.deleted)
  );
}

function sameEntry(a: FileEntry, b: FileEntry): boolean {
  return a.name === b.name && JSON.stringify(a.id) === JSON.stringify(b.id);
}

function extension(name: string): string {
  const dot = name.lastIndexOf(".");
  return dot < 0 ? "" : name.slice(dot + 1).toLowerCase();
}

const ATTRIBUTE_NAMES: [number, string][] = [
  [0x1, "Read-only"],
  [0x2, "Hidden"],
  [0x4, "System"],
  [0x20, "Archive"],
  [0x200, "Sparse"],
  [0x800, "Compressed"],
  [0x4000, "Encrypted"],
];

function attributeText(attributes: number): string {
  return ATTRIBUTE_NAMES.filter(([bit]) => attributes & bit)
    .map(([, name]) => name)
    .join(", ");
}

/** Text if the bytes look like UTF-8 text, else a hex dump of the start. */
function textOrHex(bytes: Uint8Array, size: number): Preview {
  const sample = bytes.subarray(0, 4096);
  const control = sample.filter((b) => b < 0x09 || (b > 0x0d && b < 0x20)).length;
  if (bytes.length > 0 && control === 0) {
    const text = new TextDecoder("utf-8", { fatal: false }).decode(bytes);
    if (!text.slice(0, 4096).includes("�")) {
      return { kind: "text", text, truncated: size > bytes.length };
    }
  }
  return { kind: "hex", bytes: bytes.subarray(0, 512) };
}

function hexPreview(bytes: Uint8Array): string {
  const lines = [];
  // 8 bytes a row fits the details panel.
  for (let i = 0; i < bytes.length; i += 8) {
    const row = Array.from(bytes.subarray(i, i + 8));
    const hexes = row.map((b) => hex(b, 2)).join(" ");
    const text = row.map((b) => (b >= 0x20 && b < 0x7f ? String.fromCharCode(b) : ".")).join("");
    lines.push(`${hex(i, 4)}  ${hexes.padEnd(23)}  ${text}`);
  }
  return lines.join("\n");
}
