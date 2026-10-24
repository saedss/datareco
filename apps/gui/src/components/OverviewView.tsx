import { useEffect, useState } from "react";
import { DiskMap, api, driveLetter, toCommandError } from "../api";
import { View } from "../activity";
import { TAB_NAMES } from "../commands";
import { formatSize } from "../format";
import { DiskBar, SegmentTable, segmentName, tableSummary } from "./DiskView";

type Props = {
  sourceId: string;
  sectorSize: number;
  isImage: boolean;
  /** The disk Windows runs from. */
  windows: boolean;
  onGo: (view: View) => void;
  onShowInSectors: (offset: number) => void;
};

/** What each tab is for, in a line. */
function purposes(isImage: boolean, windows: boolean): [View, string][] {
  return [
    ["files", "See the files on its volumes, deleted ones too, and save them to another disk."],
    ["recover", "Find lost drives and lost partitions, or files by their type when nothing else works."],
    ["partitions", "Create, delete, format, move and resize partitions. Nothing is written until you confirm."],
    [
      "backup",
      isImage
        ? "Copy this image into another file, or restore it onto a disk."
        : windows
          ? "Back it up to an image, rescue it if it's failing, or move Windows to a new disk."
          : "Back it up to an image, rescue it if it's failing, or clone it onto another disk.",
    ],
    ["health", isImage ? "Read every sector, to find any that can't be read." : "Its health record, a surface test and its read speed."],
    ["hex", "Every byte, with structures decoded, search and bookmarks. For experts."],
  ];
}

/** The Overview tab: the disk at a glance, and where to go next. */
export function OverviewView({ sourceId, sectorSize, isImage, windows, onGo, onShowInSectors }: Props) {
  const [map, setMap] = useState<DiskMap | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<number | null>(null);

  useEffect(() => {
    api
      .diskMap(sourceId)
      .then(setMap)
      .catch((e) => setError(toCommandError(e).message));
  }, [sourceId]);

  const segment = map && selected !== null ? map.segments[selected] : null;
  const volume = segment?.volume ?? null;

  return (
    <div className="disk-view">
      <section className="disk-section">
        <div className="disk-section-head">
          <h2>Overview</h2>
        </div>
        {error && <p className="problem error">{error}</p>}
        {!map && !error && <p className="muted small">Reading the partition table…</p>}
        {map && (
          <>
            <p className="muted small">{tableSummary(map.table)}</p>
            <DiskBar map={map} sectorSize={sectorSize} selected={selected} onSelect={setSelected} />
            <SegmentTable map={map} sectorSize={sectorSize} selected={selected} onSelect={setSelected} />
            {map.table.problems.map((problem) => (
              <p key={problem.message} className={`problem ${problem.severity}`}>
                {problem.message}
              </p>
            ))}
            {segment && (
              <div className="overview-selected">
                <strong>{segmentName(segment, map.table)}</strong>
                <span className="muted">
                  {formatSize(segment.sectors * sectorSize)}
                  {volume?.fileSystem && ` · ${volume.fileSystem}`}
                  {volume && driveLetter(volume) && volume.free !== null && ` · ${formatSize(volume.free)} free`}
                </span>
                <span className="button-row">
                  <button onClick={() => onShowInSectors(segment.startLba * sectorSize)}>Show in the hex editor</button>
                  {segment.kind !== "unusable" && <button onClick={() => onGo("partitions")}>Change it in Partitions</button>}
                </span>
              </div>
            )}
          </>
        )}
      </section>

      <section className="disk-section">
        <div className="disk-section-head">
          <h2>What you can do here</h2>
        </div>
        <div className="purpose-grid">
          {purposes(isImage, windows).map(([view, text]) => (
            <button key={view} className="purpose" onClick={() => onGo(view)}>
              <strong>{TAB_NAMES[view]}</strong>
              <span className="muted small">{text}</span>
            </button>
          ))}
        </div>
      </section>
    </div>
  );
}
