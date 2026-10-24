import { DeviceList, PhysicalDisk, Volume, driveLetter, sourceId } from "../api";
import { formatSize } from "../format";

/** What is open in the main view. */
export type Selection = { id: string; title: string; subtitle: string };

type Props = {
  devices: DeviceList | null;
  devicesError: string | null;
  images: string[];
  /** The open disk, volume or image; null on Home. */
  selectedId: string | null;
  elevated: boolean;
  onSelect: (selection: Selection) => void;
  onHome: () => void;
  onOpenImage: () => void;
  onRefresh: () => void;
  onRestartAsAdmin: () => void;
};

/** Where the volume starts on `disk`, to put a disk's volumes in order. */
function offsetOn(volume: Volume, disk: number): number {
  return Math.min(...volume.extents.filter((e) => e.disk === disk).map((e) => e.offset));
}

/** The disks with their volumes under them, the drives on no disk DataReco
 *  sees (virtual drives), then the image files. */
export function DeviceSidebar(props: Props) {
  const { devices, devicesError, images, selectedId, elevated, onSelect } = props;
  const onDisks = new Set(devices?.disks.map((d) => d.number) ?? []);
  const others = devices?.volumes.filter((v) => !v.extents.some((e) => onDisks.has(e.disk))) ?? [];
  return (
    <aside className="sidebar" aria-label="Disks and image files">
      <div className="sidebar-scroll">
        <button className={selectedId === null ? "device home selected" : "device home"} onClick={props.onHome}>
          Home
        </button>

        <div className="sidebar-heading">
          <h2 className="sidebar-title">This PC</h2>
          <button className="link-button" onClick={props.onRefresh} title="Look for disks again (F5)">
            Refresh
          </button>
        </div>
        {devicesError && <p className="sidebar-empty error-text">{devicesError}</p>}
        {!devices && !devicesError && <p className="sidebar-empty">Looking for disks…</p>}
        {devices?.disks.length === 0 && <p className="sidebar-empty">No disks found.</p>}
        {devices?.disks.map((disk) => {
          const selection = diskSelection(disk);
          const volumes = devices.volumes
            .filter((v) => v.extents.some((e) => e.disk === disk.number))
            .sort((a, b) => offsetOn(a, disk.number) - offsetOn(b, disk.number));
          const windows = volumes.some((v) => driveLetter(v)?.toUpperCase() === "C:");
          return (
            <div key={selection.id} className="device-group">
              <DeviceRow
                selected={selection.id === selectedId}
                onClick={() => onSelect(selection)}
                title={`Disk ${disk.number}`}
                tag={windows ? "Windows" : null}
                detail={`${disk.model} · ${disk.bus}${disk.removable ? " · removable" : ""}`}
                meta={formatSize(disk.size)}
              />
              {volumes.length > 0 && (
                <div className="device-children" role="group" aria-label={`Partitions on Disk ${disk.number}`}>
                  {volumes.map((volume) => {
                    const child = volumeSelection(volume);
                    return (
                      <VolumeRow
                        key={child.id}
                        volume={volume}
                        selected={child.id === selectedId}
                        onClick={() => onSelect(child)}
                      />
                    );
                  })}
                </div>
              )}
            </div>
          );
        })}

        {others.length > 0 && (
          <>
            <h2 className="sidebar-title">Other drives</h2>
            {others.map((volume) => {
              const selection = volumeSelection(volume);
              return (
                <VolumeRow
                  key={selection.id}
                  volume={volume}
                  selected={selection.id === selectedId}
                  onClick={() => onSelect(selection)}
                />
              );
            })}
          </>
        )}

        <h2 className="sidebar-title">Image files</h2>
        {images.map((path) => {
          const selection = imageSelection(path);
          return (
            <DeviceRow
              key={selection.id}
              selected={selection.id === selectedId}
              onClick={() => onSelect(selection)}
              title={selection.title}
              tag={null}
              detail={path}
              meta=""
            />
          );
        })}
        <button className="open-image" onClick={props.onOpenImage}>
          Open an image file…
        </button>
      </div>

      {!elevated && (
        <div className="sidebar-footer">
          <p>Reading disks needs administrator rights; Windows asks for them. Image files work without.</p>
          <button onClick={props.onRestartAsAdmin}>Restart as administrator</button>
        </div>
      )}
    </aside>
  );
}

type RowProps = {
  title: string;
  tag: string | null;
  detail: string;
  meta: string;
  selected: boolean;
  onClick: () => void;
};

function DeviceRow({ title, tag, detail, meta, selected, onClick }: RowProps) {
  return (
    <button className={selected ? "device selected" : "device"} onClick={onClick} aria-pressed={selected}>
      <span className="device-line">
        <span className="device-title">
          {title}
          {tag && <span className="device-tag">{tag}</span>}
        </span>
        <span className="device-meta">{meta}</span>
      </span>
      <span className="device-detail" title={detail}>
        {detail}
      </span>
    </button>
  );
}

function VolumeRow({ volume, selected, onClick }: { volume: Volume; selected: boolean; onClick: () => void }) {
  const letter = driveLetter(volume);
  // Without a letter (the EFI and recovery partitions), it goes by its label
  // or file system.
  const name = letter
    ? [letter, volume.label].filter(Boolean).join(" ")
    : (volume.label ?? `${volume.fileSystem ?? "Unknown"} partition`);
  const detail = [volume.fileSystem, letter ? null : "no drive letter"].filter(Boolean).join(" · ");
  return (
    <button
      className={selected ? "device volume selected" : "device volume"}
      onClick={onClick}
      aria-pressed={selected}
      title={`${name}${detail ? ` · ${detail}` : ""}`}
    >
      <span className="device-line">
        <span className="device-name">{name}</span>
        <span className="device-meta">{volume.size === null ? "" : formatSize(volume.size)}</span>
      </span>
    </button>
  );
}

function diskSelection(disk: PhysicalDisk): Selection {
  return {
    id: sourceId.disk(disk),
    title: `Disk ${disk.number}`,
    subtitle: `${disk.model} · ${disk.bus}`,
  };
}

function volumeSelection(volume: Volume): Selection {
  const letter = driveLetter(volume);
  const details = [volume.label, volume.fileSystem].filter(Boolean).join(" · ");
  return {
    id: sourceId.volume(volume),
    title: letter ? `Volume ${letter}` : (volume.label ?? "Volume without a drive letter"),
    subtitle: letter ? details : [details, volume.path].filter(Boolean).join(" · "),
  };
}

/** The disk or volume with this source id, if it's in the list. */
export function findSelection(id: string, devices: DeviceList): Selection | null {
  const all = [...devices.disks.map(diskSelection), ...devices.volumes.map(volumeSelection)];
  return all.find((selection) => selection.id === id) ?? null;
}

export function imageSelection(path: string): Selection {
  return {
    id: sourceId.image(path),
    title: path.split(/[\\/]/).pop() ?? path,
    subtitle: path,
  };
}

/** Whether the source is the disk Windows runs from (it holds C:). */
export function runsWindows(id: string, devices: DeviceList | null): boolean {
  if (!devices || !id.startsWith("disk:")) return false;
  const number = Number(id.slice(5));
  return devices.volumes.some(
    (v) => driveLetter(v)?.toUpperCase() === "C:" && v.extents.some((e) => e.disk === number),
  );
}
