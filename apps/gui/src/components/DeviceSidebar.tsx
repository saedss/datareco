import { DeviceList, PhysicalDisk, Volume, driveLetter, sourceId } from "../api";
import { formatSize } from "../format";

/** What is open in the main view. */
export type Selection = { id: string; title: string; subtitle: string };

type Props = {
  devices: DeviceList | null;
  devicesError: string | null;
  images: string[];
  selectedId: string | null;
  elevated: boolean;
  onSelect: (selection: Selection) => void;
  onOpenImage: () => void;
  onRefresh: () => void;
  onRestartAsAdmin: () => void;
};

export function DeviceSidebar(props: Props) {
  const { devices, devicesError, images, selectedId, elevated, onSelect } = props;
  return (
    <aside className="sidebar">
      <div className="sidebar-scroll">
        <div className="sidebar-heading">
          <h2 className="sidebar-title">Physical disks</h2>
          <button className="link-button" onClick={props.onRefresh} title="Look for disks again">
            Refresh
          </button>
        </div>
        {devicesError && <p className="sidebar-empty error-text">{devicesError}</p>}
        {!devices && !devicesError && <p className="sidebar-empty">Looking for disks…</p>}
        {devices?.disks.length === 0 && <p className="sidebar-empty">No disks found.</p>}
        {devices?.disks.map((disk) => {
          const selection = diskSelection(disk);
          return (
            <DeviceRow
              key={selection.id}
              selected={selection.id === selectedId}
              onClick={() => onSelect(selection)}
              title={`Disk ${disk.number}`}
              detail={disk.model}
              meta={`${formatSize(disk.size)} · ${disk.bus}${disk.removable ? " · removable" : ""}`}
            />
          );
        })}

        <h2 className="sidebar-title">Volumes</h2>
        {devices?.volumes.length === 0 && <p className="sidebar-empty">No volumes found.</p>}
        {devices?.volumes.map((volume) => {
          const selection = volumeSelection(volume);
          return (
            <DeviceRow
              key={selection.id}
              selected={selection.id === selectedId}
              onClick={() => onSelect(selection)}
              title={driveLetter(volume) ?? volume.label ?? "No drive letter"}
              detail={[volume.label, volume.fileSystem].filter(Boolean).join(" · ") || "Unknown file system"}
              meta={volume.size === null ? "" : formatSize(volume.size)}
            />
          );
        })}

        <h2 className="sidebar-title">Image files</h2>
        {images.map((path) => {
          const selection = imageSelection(path);
          return (
            <DeviceRow
              key={selection.id}
              selected={selection.id === selectedId}
              onClick={() => onSelect(selection)}
              title={selection.title}
              detail={path}
              meta=""
            />
          );
        })}
        <button className="open-image" onClick={props.onOpenImage}>
          Open image file…
        </button>
      </div>

      {!elevated && (
        <div className="sidebar-footer">
          <p>Reading disks and volumes needs administrator rights. Image files work without.</p>
          <button onClick={props.onRestartAsAdmin}>Restart as administrator</button>
        </div>
      )}
    </aside>
  );
}

type RowProps = {
  title: string;
  detail: string;
  meta: string;
  selected: boolean;
  onClick: () => void;
};

function DeviceRow({ title, detail, meta, selected, onClick }: RowProps) {
  return (
    <button className={selected ? "device selected" : "device"} onClick={onClick} aria-pressed={selected}>
      <span className="device-line">
        <span className="device-title">{title}</span>
        <span className="device-meta">{meta}</span>
      </span>
      <span className="device-detail" title={detail}>
        {detail}
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
    title: letter ? `Volume ${letter}` : volume.label ?? "Volume without a drive letter",
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
