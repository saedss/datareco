import { Operation, RawStatus, ScanStatus } from "../api";
import { View } from "../activity";
import { LostPartitionSearch } from "./PartitionsPanel";
import { RawScanPanel } from "./RawScanPanel";
import { ScanView } from "./ScanView";

type Props = {
  sourceId: string;
  sectorSize: number;
  scan: ScanStatus | null;
  onScan: (status: ScanStatus) => void;
  raw: RawStatus | null;
  onRaw: (status: RawStatus) => void;
  /** Opens what a search found in Browse files, by its volume key. */
  onBrowse: (key: string) => void;
  onShowInSectors: (offset: number) => void;
  /** Puts a found partition back, as a pending change in Partitions. */
  onRestore: (op: Operation, needsTable: boolean) => void;
  onGo: (view: View) => void;
};

/** The Recover data tab: every way to find data that's gone, easiest first.
 *  They only read the disk. */
export function RecoverView(props: Props) {
  const { sourceId, sectorSize } = props;
  return (
    <div className="disk-view">
      <p className="tab-intro">
        Ways to find data that's gone, easiest first. They only read this disk; what they find opens in Browse files,
        where you save it to another disk.
      </p>
      <p className="tip">
        Deleted a few files from a drive that still works? They're probably still listed in{" "}
        <button className="link-button inline" onClick={() => props.onGo("files")}>
          Browse files
        </button>
        , struck through: no search needed.
      </p>

      <section className="disk-section">
        <div className="disk-section-head">
          <h2>Search for lost drives</h2>
        </div>
        <p className="muted small">
          For a drive that shows as empty or RAW, asks to be formatted, or was formatted by mistake.
        </p>
        <ScanView
          sourceId={sourceId}
          sectorSize={sectorSize}
          status={props.scan}
          onStatus={props.onScan}
          onBrowse={props.onBrowse}
          onShowInSectors={props.onShowInSectors}
        />
      </section>

      <section className="disk-section">
        <div className="disk-section-head">
          <h2>Find lost partitions</h2>
        </div>
        <p className="muted small">
          For partitions that disappeared from the disk, for example after its partition table was wiped. One found
          can be put back in the table, as a change you review in Partitions.
        </p>
        <LostPartitionSearch
          sourceId={sourceId}
          sectorSize={sectorSize}
          onJump={props.onShowInSectors}
          onRestore={props.onRestore}
        />
      </section>

      <section className="disk-section">
        <div className="disk-section-head">
          <h2>Find files by type</h2>
        </div>
        <RawScanPanel sourceId={sourceId} status={props.raw} onStatus={props.onRaw} onBrowse={props.onBrowse} />
      </section>
    </div>
  );
}
