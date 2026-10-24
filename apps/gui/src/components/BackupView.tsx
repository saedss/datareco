import { useEffect, useState } from "react";
import { BackupTask } from "../commands";
import { ClonePartitionPanel, CloneDiskPanel } from "./CloneForms";
import { CopyView } from "./CopyView";

type Props = {
  sourceId: string;
  sourceName: string;
  size: number;
  sectorSize: number;
  isImage: boolean;
  /** The disk Windows runs from. */
  windows: boolean;
  /** Asked for from a menu or another tab. */
  request: { task: BackupTask; startLba?: number; nonce: number } | null;
  /** Why nothing should be copied now (a move left unfinished). */
  blocked: string | null;
  onOpenImage: (path: string) => void;
  onRestartAsAdmin: () => void;
};

type Card = { task: BackupTask; title: string; text: string };

function cards(isImage: boolean, windows: boolean): Card[] {
  return [
    {
      task: "image",
      title: isImage ? "Copy into another image file" : "Create a disk image",
      text: "An exact copy of every sector, saved to a file: a backup to restore later, or to recover from instead of the disk. Works on failing drives too.",
    },
    isImage
      ? {
          task: "rescue",
          title: "Restore this image onto a disk",
          text: "Writes the image back onto a disk, sector for sector. Erases that disk.",
        }
      : {
          task: "rescue",
          title: "Rescue a failing drive onto another disk",
          text: "Copies every sector onto another disk, good parts first, then going back over the bad ones. Erases that disk.",
        },
    windows
      ? {
          task: "clone",
          title: "Move Windows to a new disk",
          text: "Copies the disk Windows runs from onto another disk, used space only, ready to start from it.",
        }
      : {
          task: "clone",
          title: isImage ? "Clone onto a disk" : "Clone onto another disk",
          text: "Copies the partitions onto another disk, used space only, so a smaller disk will do. Erases that disk.",
        },
    {
      task: "partition",
      title: "Copy one partition",
      text: "Copies a partition into free space on another disk, and adds it to that disk's partition table.",
    },
  ];
}

/** The Back up & clone tab: images, rescue copies, clones and partition copies. */
export function BackupView(props: Props) {
  const { sourceId, isImage, windows, request } = props;
  const [task, setTask] = useState<BackupTask | null>(null);
  // Each kind of copy stays once opened, so one running keeps running.
  const [opened, setOpened] = useState<Set<BackupTask>>(new Set());
  const [startLba, setStartLba] = useState<number | null>(null);

  function pick(next: BackupTask) {
    setTask(next);
    setOpened((current) => (current.has(next) ? current : new Set(current).add(next)));
  }

  useEffect(() => {
    if (!request) return;
    pick(request.task);
    if (request.startLba !== undefined) setStartLba(request.startLba);
  }, [request]);

  const all = cards(isImage, windows);
  const card = (t: BackupTask) => all.find((c) => c.task === t)!;

  return (
    <div className="disk-view">
      <p className="tab-intro">
        Copies of this {isImage ? "image" : "disk"}. {isImage ? "The image" : "This disk"} is only read; anything
        written to another disk is shown first and needs its name typed.
      </p>
      <div className="task-cards" role="group" aria-label="What to copy">
        {all.map((c) => (
          <button
            key={c.task}
            className={task === c.task ? "task-card selected" : "task-card"}
            aria-pressed={task === c.task}
            onClick={() => pick(c.task)}
          >
            <strong>{c.title}</strong>
            <span className="muted small">{c.text}</span>
          </button>
        ))}
      </div>

      {opened.has("image") && (
        <section className="disk-section" hidden={task !== "image"}>
          <div className="disk-section-head">
            <h2>{card("image").title}</h2>
          </div>
          <CopyView
            sourceId={sourceId}
            sourceName={props.sourceName}
            size={props.size}
            kind="image"
            onOpenImage={props.onOpenImage}
            onRestartAsAdmin={props.onRestartAsAdmin}
          />
        </section>
      )}
      {opened.has("rescue") && (
        <section className="disk-section" hidden={task !== "rescue"}>
          <div className="disk-section-head">
            <h2>{card("rescue").title}</h2>
          </div>
          <CopyView
            sourceId={sourceId}
            sourceName={props.sourceName}
            size={props.size}
            kind="disk"
            onOpenImage={props.onOpenImage}
            onRestartAsAdmin={props.onRestartAsAdmin}
          />
        </section>
      )}
      {opened.has("clone") && (
        <section className="disk-section" hidden={task !== "clone"}>
          <CloneDiskPanel sourceId={sourceId} blocked={props.blocked} />
        </section>
      )}
      {opened.has("partition") && (
        <section className="disk-section" hidden={task !== "partition"}>
          <div className="disk-section-head">
            <h2>{card("partition").title}</h2>
          </div>
          <ClonePartitionPanel
            sourceId={sourceId}
            sectorSize={props.sectorSize}
            startLba={startLba}
            blocked={props.blocked}
          />
        </section>
      )}
    </div>
  );
}
