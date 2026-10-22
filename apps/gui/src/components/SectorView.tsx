import { KeyboardEvent, useCallback, useEffect, useRef, useState } from "react";
import { api, CommandError, SectorData, SourceInfo, toCommandError } from "../api";
import { formatNumber, formatSize } from "../format";
import { HexView } from "./HexView";

type Props = {
  sourceId: string;
  title: string;
  subtitle: string;
  onRestartAsAdmin: () => void;
};

/** One source: its size, sector navigation, and the current sector as hex. */
export function SectorView({ sourceId, title, subtitle, onRestartAsAdmin }: Props) {
  const [info, setInfo] = useState<SourceInfo | null>(null);
  const [lba, setLba] = useState(0);
  const [draft, setDraft] = useState("0");
  const [sector, setSector] = useState<SectorData | null>(null);
  const [error, setError] = useState<CommandError | null>(null);
  const [loading, setLoading] = useState(true);
  // Answers to old requests are ignored, so fast clicking never shows a stale sector.
  const latestRequest = useRef(0);

  useEffect(() => {
    const request = ++latestRequest.current;
    api
      .openSource(sourceId)
      .then((opened) => request === latestRequest.current && setInfo(opened))
      .catch((e) => {
        if (request !== latestRequest.current) return;
        setError(toCommandError(e));
        setLoading(false);
      });
  }, [sourceId]);

  useEffect(() => {
    if (!info) return;
    const request = ++latestRequest.current;
    setLoading(true);
    api
      .readSectors(info.id, lba, 1)
      .then((data) => {
        if (request !== latestRequest.current) return;
        setSector(data);
        setError(null);
      })
      .catch((e) => request === latestRequest.current && setError(toCommandError(e)))
      .finally(() => request === latestRequest.current && setLoading(false));
  }, [info, lba]);

  const lastSector = info ? info.sectorCount - 1 : 0;

  const go = useCallback(
    (target: number) => {
      const next = Math.min(Math.max(0, Math.floor(target)), lastSector);
      setLba(next);
      setDraft(String(next));
    },
    [lastSector],
  );

  /** Accepts decimal or 0x-prefixed hex; anything else snaps back. */
  function submitDraft() {
    const text = draft.trim().toLowerCase();
    const value = text.startsWith("0x") ? parseInt(text.slice(2), 16) : Number(text);
    if (text !== "" && Number.isFinite(value)) go(value);
    else setDraft(String(lba));
  }

  function onKeyDown(event: KeyboardEvent) {
    if (event.key === "PageDown") go(lba + 1);
    else if (event.key === "PageUp") go(lba - 1);
    else if (event.ctrlKey && event.key === "Home") go(0);
    else if (event.ctrlKey && event.key === "End") go(lastSector);
    else return;
    event.preventDefault();
  }

  return (
    <section className="sector-view" tabIndex={-1} onKeyDown={onKeyDown}>
      <header className="view-header">
        <div>
          <h1>{title}</h1>
          <p className="muted">{subtitle}</p>
        </div>
        {info && (
          <p className="muted view-size">
            {formatSize(info.size)} · {formatNumber(info.sectorCount)} sectors of {info.sectorSize} bytes
          </p>
        )}
      </header>

      {error?.kind === "access_denied" ? (
        <AdminNeeded onRestart={onRestartAsAdmin} />
      ) : error ? (
        <div className="banner error" role="alert">
          {error.message}
        </div>
      ) : null}

      {info && (
        <>
          <div className="toolbar">
            <button onClick={() => go(0)} disabled={lba === 0}>
              First
            </button>
            <button onClick={() => go(lba - 1)} disabled={lba === 0}>
              ‹ Previous
            </button>
            <label className="sector-input">
              Sector
              <input
                value={draft}
                onChange={(e) => setDraft(e.target.value)}
                onKeyDown={(e) => e.key === "Enter" && submitDraft()}
                onBlur={submitDraft}
                inputMode="numeric"
                spellCheck={false}
                aria-label="Sector number"
              />
            </label>
            <button onClick={() => go(lba + 1)} disabled={lba >= lastSector}>
              Next ›
            </button>
            <button onClick={() => go(lastSector)} disabled={lba >= lastSector}>
              Last
            </button>
            <span className="muted">last sector: {formatNumber(lastSector)}</span>
            {loading && <span className="muted">Reading…</span>}
          </div>

          {sector && sector.unreadable.length > 0 && (
            <div className="banner warning" role="alert">
              Sector {sector.unreadable.join(", ")} couldn't be read. Its bytes are shown as zeros.
            </div>
          )}
          {sector && <HexView offset={sector.lba * info.sectorSize} bytes={sector.data} />}
          <p className="hint muted">Tip: Page Up and Page Down move between sectors.</p>
        </>
      )}
    </section>
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
