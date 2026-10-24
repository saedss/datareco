import { useEffect, useState } from "react";
import { DiskMap, api } from "../api";
import { HealthSection, SpeedSection, SurfaceSection, segmentName } from "./DiskView";

type Props = {
  sourceId: string;
  sectorSize: number;
  isImage: boolean;
  onShowInSectors: (offset: number) => void;
};

/** The Health tab: the drive's own health record, a surface test and a read
 *  speed test. Each only reads. */
export function HealthView({ sourceId, sectorSize, isImage, onShowInSectors }: Props) {
  const [map, setMap] = useState<DiskMap | null>(null);
  useEffect(() => {
    api
      .diskMap(sourceId)
      .then(setMap)
      .catch(() => setMap(null));
  }, [sourceId]);
  const partitions =
    map?.segments
      .filter((s) => s.kind === "partition" && s.partition !== null)
      .map((s) => ({ number: s.partition as number, name: `${s.partition}: ${segmentName(s, map.table)}` })) ?? [];

  return (
    <div className="disk-view">
      <p className="tab-intro">
        {isImage
          ? "An image file has no drive to ask about its health, but its sectors can still be read through to find any that can't."
          : "How the drive itself is doing: what it records about its own health, how well every sector reads, and how fast. These only read; nothing is written."}
      </p>
      {!isImage && <HealthSection sourceId={sourceId} />}
      <SurfaceSection
        sourceId={sourceId}
        sectorSize={sectorSize}
        partitions={partitions}
        onShowInSectors={onShowInSectors}
      />
      {!isImage && <SpeedSection sourceId={sourceId} />}
    </div>
  );
}
