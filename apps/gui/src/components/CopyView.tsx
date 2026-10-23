import { useEffect, useRef, useState } from "react";
import { open, save } from "@tauri-apps/plugin-dialog";
import {
  CommandError,
  CopyStatus,
  CopyTarget,
  CopyTotals,
  DiskChoice,
  DiskTargetCheck,
  MapInfo,
  api,
  toCommandError,
} from "../api";
import { formatSize } from "../format";

type Props = {
  sourceId: string;
  /** "Disk 1", for messages. */
  sourceName: string;
  size: number;
  /** Opens a finished image file in the editor. */
  onOpenImage: (path: string) => void;
  onRestartAsAdmin: () => void;
};

const IMAGE_FILES = [{ name: "Disk images", extensions: ["img"] }];
const MAP_FILES = [{ name: "Copy maps", extensions: ["map"] }];

/** Copies the source into an image file or onto another disk, with a map to carry on later. */
export function CopyView({ sourceId, sourceName, size, onOpenImage, onRestartAsAdmin }: Props) {
  const [kind, setKind] = useState<"image" | "disk">("image");
  const [imagePath, setImagePath] = useState("");
  const [diskNumber, setDiskNumber] = useState<number | null>(null);
  const [mapPath, setMapPath] = useState("");
  const [skip, setSkip] = useState(true);
  const [retries, setRetries] = useState(1);
  const [choices, setChoices] = useState<DiskChoice[] | null>(null);
  const [choicesError, setChoicesError] = useState<string | null>(null);
  /** What the chosen map says, when it's of a copy started before. */
  const [resume, setResume] = useState<MapInfo | null>(null);
  const [status, setStatus] = useState<CopyStatus | null>(null);
  const [running, setRunning] = useState(false);
  const [error, setError] = useState<CommandError | null>(null);
  const [confirming, setConfirming] = useState<DiskTargetCheck | null>(null);
  const [typed, setTyped] = useState("");
  /** The confirmation typed for a disk, reused when carrying on the same copy after a pause. */
  const confirmed = useRef<{ number: number; text: string } | null>(null);
  const stopping = useRef(false);

  function loadChoices() {
    setChoicesError(null);
    api
      .copyDiskChoices(sourceId)
      .then(setChoices)
      .catch((e) => setChoicesError(toCommandError(e).message));
  }

  useEffect(() => {
    if (kind === "disk" && !choices) loadChoices();
  }, [kind]);

  const target: CopyTarget | null =
    kind === "image"
      ? imagePath
        ? { kind: "image", path: imagePath }
        : null
      : diskNumber !== null
        ? { kind: "disk", number: diskNumber }
        : null;

  /** A new target or map means a new copy: forget the last one. */
  function changed() {
    setStatus(null);
    setResume(null);
    setError(null);
  }

  /** Looks at a map file the person picked; it may be of a copy started before. */
  async function pickMap(path: string) {
    setMapPath(path);
    try {
      setResume(await api.readCopyMap(path));
    } catch {
      setResume(null);
    }
  }

  async function chooseImage() {
    const path = await save({
      title: "Save the copy as",
      defaultPath: sourceId.startsWith("image:") ? `${fileStem(sourceId)}-copy.img` : `${fileStem(sourceId)}.img`,
      filters: IMAGE_FILES,
    });
    if (!path) return;
    changed();
    setImagePath(path);
    await pickMap(mapFor(path));
  }

  async function chooseMap() {
    const name =
      kind === "disk" && diskNumber !== null ? `${fileStem(sourceId)}-to-disk${diskNumber}.map` : "copy.map";
    const path = await save({ title: "Keep the copy's map in", defaultPath: name, filters: MAP_FILES });
    if (!path) return;
    changed();
    await pickMap(path);
  }

  async function openMap() {
    const path = await open({
      title: "Carry on a copy from its map",
      multiple: false,
      directory: false,
      filters: MAP_FILES,
    });
    if (typeof path !== "string") return;
    changed();
    try {
      const info = await api.readCopyMap(path);
      setMapPath(path);
      setResume(info);
      const from = parseTarget(info.target);
      if (from?.kind === "image") {
        setKind("image");
        setImagePath(from.path);
      } else if (from?.kind === "disk") {
        setKind("disk");
        setDiskNumber(from.number);
      }
    } catch (e) {
      setError(toCommandError(e));
    }
  }

  function start() {
    if (!target) return;
    if (target.kind === "image") {
      run(target, null);
      return;
    }
    const again = confirmed.current?.number === target.number ? confirmed.current.text : null;
    if (again) {
      run(target, again);
      return;
    }
    const check = choices?.find((c) => c.number === target.number)?.check;
    if (!check) {
      setError({ kind: "refused", message: "The copy can't go onto that disk. Pick another one." });
      return;
    }
    setTyped("");
    setConfirming(check);
  }

  function confirm() {
    if (!confirming || !target) return;
    confirmed.current = { number: confirming.number, text: typed };
    setConfirming(null);
    run(target, typed);
  }

  async function run(to: CopyTarget, confirmation: string | null) {
    stopping.current = false;
    setRunning(true);
    setError(null);
    try {
      let current = await api.startCopy(sourceId, to, mapPath, retries, skip, confirmation);
      setStatus(current);
      setResume(null);
      while (current.running && !stopping.current) {
        current = await api.continueCopy(sourceId);
        setStatus(current);
      }
      if (current.running) {
        const stopped = await api.stopCopy(sourceId);
        if (stopped) setStatus(stopped);
      }
    } catch (e) {
      setError(toCommandError(e));
      await api.stopCopy(sourceId).catch(() => null);
    } finally {
      setRunning(false);
    }
  }

  const progress = status?.progress;
  const totals: CopyTotals | null = progress?.totals ?? resume?.totals ?? null;
  const picture = status?.picture ?? resume?.picture ?? null;
  const finished = progress?.finished ?? resume?.finished ?? false;
  const begun = totals !== null && (totals.copied > 0 || totals.bad > 0);
  const startLabel = !begun ? "Start copy" : finished ? "Try unreadable sectors again" : "Carry on";
  const canStart = !running && target !== null && mapPath !== "" && !(finished && retries === 0);

  return (
    <div className="scan-view copy-view">
      {!status && !resume && (
        <div className="notice">
          <h2>Copy this {sourceId.startsWith("image:") ? "image" : "disk"}</h2>
          <p>
            Copies every sector into an image file or onto another disk, in passes made for failing drives: a fast
            first pass that skips past trouble, then backwards, then the edges and insides of what failed, then
            retries. Work on the copy instead of the drive: each read of a failing drive can be its last.
          </p>
          {sourceId.startsWith("volume:") && (
            <p className="warning-text">
              Programs can change a volume while it's copied, so the copy may not be consistent. Copy it while
              nothing uses it, or copy its whole disk.
            </p>
          )}
        </div>
      )}

      <fieldset className="copy-form" disabled={running}>
        <div className="copy-row">
          <span className="copy-label">Copy to</span>
          <label>
            <input
              type="radio"
              name="copy-kind"
              checked={kind === "image"}
              onChange={() => {
                setKind("image");
                changed();
                setMapPath(imagePath ? mapFor(imagePath) : "");
              }}
            />{" "}
            An image file
          </label>
          <label>
            <input
              type="radio"
              name="copy-kind"
              checked={kind === "disk"}
              onChange={() => {
                setKind("disk");
                changed();
                setMapPath("");
              }}
            />{" "}
            Another disk, erasing it
          </label>
        </div>

        {kind === "image" ? (
          <div className="copy-row">
            <span className="copy-label">Image file</span>
            <FilePath path={imagePath} />
            <button onClick={chooseImage}>Choose…</button>
          </div>
        ) : (
          <div className="copy-row copy-row-top">
            <span className="copy-label">Disk</span>
            <div className="disk-choices">
              {choicesError && <p className="problem error">{choicesError}</p>}
              {!choices && !choicesError && <p className="muted small">Looking for disks…</p>}
              {choices?.map((c) => (
                <label key={c.number} className={c.check ? "disk-choice" : "disk-choice unavailable"}>
                  <input
                    type="radio"
                    name="copy-disk"
                    disabled={!c.check}
                    checked={diskNumber === c.number}
                    onChange={() => {
                      setDiskNumber(c.number);
                      changed();
                      setMapPath("");
                    }}
                  />
                  <span>
                    <strong>{c.name}</strong> · {c.model} · {formatSize(c.size)}
                    <span className="muted small disk-choice-note">
                      {c.problem ??
                        (c.check && c.check.volumes.length > 0
                          ? `Holds ${c.check.volumes.join(", ")}`
                          : "No volumes Windows knows about")}
                    </span>
                  </span>
                </label>
              ))}
              {choices && (
                <button className="link-button" onClick={loadChoices}>
                  Look for disks again
                </button>
              )}
            </div>
          </div>
        )}

        <div className="copy-row">
          <span className="copy-label">Map file</span>
          <FilePath path={mapPath} />
          <button onClick={chooseMap}>Choose…</button>
        </div>
        <p className="muted small copy-hint">
          The map records what was copied, so you can pause, close DataReco and carry on later (GNU ddrescue reads it
          too). Keep it, and the image, on a disk other than the one being copied.
        </p>

        <div className="copy-row">
          <span className="copy-label">Failing drive</span>
          <label>
            <input type="checkbox" checked={skip} onChange={(e) => setSkip(e.target.checked)} /> Skip past read errors
            first, and come back to them
          </label>
          <label>
            Retry unreadable sectors{" "}
            <input
              type="number"
              className="copy-retries"
              min={0}
              max={9}
              value={retries}
              onChange={(e) => setRetries(Math.max(0, Math.min(9, Number(e.target.value) || 0)))}
            />{" "}
            times
          </label>
        </div>
      </fieldset>

      <div className="toolbar copy-actions">
        <div className="toolbar-group">
          {running ? (
            <button onClick={() => (stopping.current = true)}>Pause</button>
          ) : (
            !(finished && (totals?.bad ?? 0) === 0) && (
              <button className="primary" disabled={!canStart} onClick={start}>
                {startLabel}
              </button>
            )
          )}
          <button disabled={running} onClick={openMap}>
            Carry on from a map…
          </button>
        </div>
      </div>

      {confirming && (
        <div className="confirm-erase" role="alertdialog" aria-labelledby="confirm-erase-title">
          <h2 id="confirm-erase-title">Erase {confirming.name}?</h2>
          <p>
            Everything on {confirming.name} ({confirming.model}, {formatSize(confirming.size)}) will be replaced by the
            copy of {sourceName}. This can't be undone.
          </p>
          {confirming.volumes.length > 0 && (
            <p>
              Its volumes will be closed and overwritten: <strong>{confirming.volumes.join(", ")}</strong>.
            </p>
          )}
          {confirming.warnings.map((warning) => (
            <p key={warning} className="warning-text">
              {warning}
            </p>
          ))}
          <label className="confirm-type">
            Type <code>{confirming.confirmation}</code> to confirm:{" "}
            <input
              autoFocus
              value={typed}
              onChange={(e) => setTyped(e.target.value)}
              aria-label={`Type ${confirming.confirmation} to confirm`}
            />
          </label>
          <div className="toolbar-group">
            <button
              className="danger"
              disabled={typed.trim().toLowerCase() !== confirming.confirmation}
              onClick={confirm}
            >
              Erase {confirming.name} and copy
            </button>
            <button onClick={() => setConfirming(null)}>Cancel</button>
          </div>
        </div>
      )}

      {error && (
        <div className="banner error" role="alert">
          {error.message}
          {error.kind === "access_denied" && (
            <>
              {" "}
              <button onClick={onRestartAsAdmin}>Restart as administrator</button>
            </>
          )}
        </div>
      )}

      {resume && !status && (
        <p className="small">
          {resume.source && resume.source !== sourceId && (
            <span className="warning-text">This map was made copying {resume.source}. </span>
          )}
          {resume.finished
            ? resume.totals.bad > 0
              ? "This copy is finished. You can try its unreadable sectors again."
              : "This copy is finished."
            : "This copy was paused. Carry on to copy the rest."}
        </p>
      )}

      {totals && (progress || resume) && (
        <div className="scan-progress">
          <progress max={Math.max(1, size)} value={totals.copied} />
          <p className="small">
            {progress
              ? running
                ? `${progress.description}… `
                : finished
                  ? "Finished: "
                  : "Paused: "
              : ""}
            {formatSize(totals.copied)} of {formatSize(size)} copied ({percent(totals.copied, size)}) ·{" "}
            {formatSize(totals.bad)} unreadable
            {totals.badAreas > 0 && ` in ${places(totals.badAreas)}`} · {formatSize(stillToRead(totals))} still to
            read
            {running && status?.rate ? ` · ${formatSize(status.rate)}/s` : ""}
          </p>
        </div>
      )}

      {picture && <CopyMap picture={picture} />}

      {status && finished && (
        <div className="found-volume copy-done">
          <p>
            <strong>
              {status.progress.totals.bad === 0
                ? `Copied all ${formatSize(size)}.`
                : `Copied ${formatSize(status.progress.totals.copied)} of ${formatSize(size)}.`}
            </strong>{" "}
            {status.progress.totals.bad > 0 &&
              `${formatSize(status.progress.totals.bad)} in ${places(status.progress.totals.badAreas)} couldn't be read; ${
                kind === "image" ? "they read as zeros in the image" : "they were filled with zeros"
              }. The map lists where.`}
          </p>
          <div className="toolbar-group">
            {kind === "image" && imagePath && (
              <button className="primary" onClick={() => onOpenImage(imagePath)}>
                Open the image
              </button>
            )}
            <button onClick={() => api.showFolder(folderOf(status.mapPath)).catch(() => {})}>
              Show the map's folder
            </button>
          </div>
        </div>
      )}
    </div>
  );
}

/** A file's name, then (dimmer) its folder; the whole path on hover. */
function FilePath({ path }: { path: string }) {
  if (!path) return <span className="copy-path muted">None chosen</span>;
  const slash = Math.max(path.lastIndexOf("\\"), path.lastIndexOf("/"));
  return (
    <span className="copy-path" title={path}>
      <strong>{path.slice(slash + 1)}</strong>
      {slash > 0 && <span className="muted"> in {path.slice(0, slash)}</span>}
    </span>
  );
}

const SQUARE: Record<string, string> = { "+": "copied", "?": "untried", "*": "trying", "/": "trying", "-": "bad" };

/** The map as squares, each the most worrying state in its part of the source. */
function CopyMap({ picture }: { picture: string }) {
  return (
    <div className="copy-map-box">
      <div className="copy-map" role="img" aria-label="Map of what was copied">
        {Array.from(picture).map((symbol, i) => (
          <span key={i} className={`sq ${SQUARE[symbol] ?? "untried"}`} />
        ))}
      </div>
      <div className="copy-legend small">
        <span>
          <span className="sq copied" /> Copied
        </span>
        <span>
          <span className="sq untried" /> Not read yet
        </span>
        <span>
          <span className="sq trying" /> Failed, being worked on
        </span>
        <span>
          <span className="sq bad" /> Unreadable
        </span>
      </div>
    </div>
  );
}

function stillToRead(totals: CopyTotals): number {
  return totals.untried + totals.untrimmed + totals.unscraped;
}

function percent(part: number, whole: number): string {
  return whole > 0 ? `${((part / whole) * 100).toFixed(1)}%` : "100%";
}

function places(count: number): string {
  return count === 1 ? "one place" : `${count} places`;
}

/** "C:\copies\disk1.img" → "C:\copies\disk1.map". */
function mapFor(image: string): string {
  const slash = Math.max(image.lastIndexOf("\\"), image.lastIndexOf("/"));
  const dot = image.lastIndexOf(".");
  const map = `${dot > slash ? image.slice(0, dot) : image}.map`;
  return map === image ? `${image}.map` : map;
}

function folderOf(path: string): string {
  const slash = Math.max(path.lastIndexOf("\\"), path.lastIndexOf("/"));
  return slash > 0 ? path.slice(0, slash) : path;
}

/** A file name for the source: "disk1", "E", or the image's name. */
function fileStem(sourceId: string): string {
  if (sourceId.startsWith("disk:")) return `disk${sourceId.slice("disk:".length)}`;
  if (sourceId.startsWith("volume:")) {
    return sourceId.slice("volume:".length).replace(/[^A-Za-z0-9]/g, "") || "volume";
  }
  const path = sourceId.slice("image:".length);
  const name = path.slice(Math.max(path.lastIndexOf("\\"), path.lastIndexOf("/")) + 1);
  const dot = name.lastIndexOf(".");
  return dot > 0 ? name.slice(0, dot) : name;
}

/** "image:D:\x.img" or "disk:2", from a map, as a copy target. */
function parseTarget(id: string | null): CopyTarget | null {
  if (!id) return null;
  if (id.startsWith("image:")) return { kind: "image", path: id.slice("image:".length) };
  const disk = /^disk:(\d+)$/.exec(id);
  return disk ? { kind: "disk", number: Number(disk[1]) } : null;
}
