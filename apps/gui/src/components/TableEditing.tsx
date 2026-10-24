import { useEffect, useState } from "react";
import { BackupInfo, Operation, Segment, TableView, TypeChoice, UndoView, api, toCommandError } from "../api";
import { formatSize } from "../format";

/** A form open under the map. */
export type EditForm =
  | { kind: "create"; segment: Segment }
  | { kind: "type"; startLba: number; current: string }
  | { kind: "name"; startLba: number; current: string }
  | { kind: "apply" }
  | { kind: "undo" }
  | { kind: "format"; segment: Segment }
  /** `segment` null: the whole disk. */
  | { kind: "wipe"; segment: Segment | null }
  | { kind: "label"; segment: Segment }
  | { kind: "letter"; segment: Segment }
  | { kind: "move"; segment: Segment; align?: boolean }
  | { kind: "resize"; segment: Segment }
  | { kind: "resumeMove" };

type ActionsProps = {
  view: TableView;
  segment: Segment | null;
  sectorSize: number;
  disabled: boolean;
  /** Table changes waiting: formatting and wiping wait until they're written. */
  pending: number;
  /** A physical disk (Windows can resize its NTFS partitions). */
  isDisk: boolean;
  onOp: (op: Operation) => void;
  onForm: (form: EditForm) => void;
  /** Copies the partition to another disk. */
  onCopy: (segment: Segment) => void;
};

/** What can be done to the selected part of the disk, and to the whole disk. */
export function EditActions({ view, segment, sectorSize, disabled, pending, isDisk, onOp, onForm, onCopy }: ActionsProps) {
  const table = view.map.table;
  const partition = segment?.partition != null ? table.partitions.find((p) => p.number === segment.partition) : null;
  const extended = table.partitions.find((p) => p.kind === "extended");
  const emptyExtended =
    segment?.kind === "free" &&
    segment.logical &&
    extended &&
    !table.partitions.some((p) => p.kind === "logical");
  const mbr = table.kind === "mbr";
  const gpt = table.kind === "gpt";
  const button = (label: string, action: () => void, title?: string) => (
    <button key={label} disabled={disabled} onClick={action} title={title}>
      {label}
    </button>
  );

  const partitionActions = partition
    ? [
        button("Delete", () => onOp({ op: "delete", startLba: partition.startLba }), "Remove it from the table; its files stay on the disk"),
        partition.hidden
          ? button("Unhide", () => onOp({ op: "setHidden", startLba: partition.startLba, hidden: false }))
          : button("Hide", () => onOp({ op: "setHidden", startLba: partition.startLba, hidden: true }), "Windows won't give it a drive letter"),
        (gpt || partition.kind === "primary") &&
          (partition.bootable
            ? button(mbr ? "Make inactive" : "Unmark bootable", () =>
                onOp({ op: "setActive", startLba: partition.startLba, active: false }),
              )
            : button(mbr ? "Set active" : "Mark BIOS bootable", () =>
                onOp({ op: "setActive", startLba: partition.startLba, active: true }),
              )),
        button("Change type…", () => onForm({ kind: "type", startLba: partition.startLba, current: partition.typeCode })),
        gpt && button("Rename…", () => onForm({ kind: "name", startLba: partition.startLba, current: partition.name ?? "" })),
        mbr && partition.kind === "primary" && button("Make logical", () => onOp({ op: "makeLogical", startLba: partition.startLba })),
        mbr && partition.kind === "logical" && button("Make primary", () => onOp({ op: "makePrimary", startLba: partition.startLba })),
      ]
    : [];
  const freeActions =
    segment?.kind === "free" && table.kind !== "none"
      ? [
          button("New partition…", () => onForm({ kind: "create", segment })),
          emptyExtended &&
            extended &&
            button("Delete the empty extended partition", () => onOp({ op: "delete", startLba: extended.startLba })),
        ]
      : [];
  const diskActions =
    table.kind === "none"
      ? table.wholeDisk
        ? []
        : [
            button("Initialize as GPT", () => onOp({ op: "initialize", style: "gpt" }), "For disks of any size; Windows' choice"),
            button("Initialize as MBR", () => onOp({ op: "initialize", style: "mbr" }), "For old computers; up to 2 TiB"),
          ]
      : [
          mbr && button("Convert to GPT", () => onOp({ op: "convert", style: "gpt" }), "Keeps every partition"),
          gpt && button("Convert to MBR", () => onOp({ op: "convert", style: "mbr" }), "Keeps every partition (four at most)"),
        ];
  const undoable = view.backups.some((b) => !b.undone && !b.undoes);
  // Formatting, wiping, labels and letters act at once, on the disk as it
  // is: not while table changes are waiting.
  const volumeActions =
    pending === 0 && segment && (segment.kind === "partition" || segment.kind === "wholeDisk")
      ? [
          button("Format…", () => onForm({ kind: "format", segment })),
          button("Wipe…", () => onForm({ kind: "wipe", segment }), "Write over it so nothing can be recovered"),
          segment.volume && button("Change label…", () => onForm({ kind: "label", segment })),
          segment.volume && button("Change drive letter…", () => onForm({ kind: "letter", segment })),
        ]
      : [];
  const ntfs =
    partition?.fileSystem?.kind === "ntfs" || partition?.fileSystem?.kind === "refs" || segment?.volume?.fileSystem === "NTFS";
  const misaligned = segment?.kind === "partition" && (segment.startLba * sectorSize) % (1 << 20) !== 0;
  const placeActions =
    pending === 0 && segment?.kind === "partition"
      ? [
          button("Move…", () => onForm({ kind: "move", segment }), "Copy it block by block to another place on the disk"),
          misaligned && button("Align to 1 MiB…", () => onForm({ kind: "move", segment, align: true })),
          isDisk && ntfs && button("Resize…", () => onForm({ kind: "resize", segment }), "Shrink or grow it, through Windows"),
          isDisk &&
            button("Copy to another disk…", () => onCopy(segment), "Copy it into free space on another disk (Back up & clone)"),
        ]
      : [];
  return (
    <div className="edit-actions">
      {(partitionActions.some(Boolean) || freeActions.some(Boolean)) && (
        <div className="button-row">{[...partitionActions, ...freeActions]}</div>
      )}
      {volumeActions.some(Boolean) && <div className="button-row">{volumeActions}</div>}
      {placeActions.some(Boolean) && <div className="button-row">{placeActions}</div>}
      <div className="button-row">
        {diskActions}
        {undoable && button("Undo a change…", () => onForm({ kind: "undo" }), "Put back the table as it was before a change")}
        {pending === 0 && button("Wipe the whole disk…", () => onForm({ kind: "wipe", segment: null }))}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------- forms

type CreateProps = {
  view: TableView;
  segment: Segment;
  sectorSize: number;
  onOp: (op: Operation) => void;
  onClose: () => void;
};

/** A new partition in free space, starting on the first 1 MiB boundary. */
export function CreateForm({ view, segment, sectorSize, onOp, onClose }: CreateProps) {
  const gpt = view.map.table.kind === "gpt";
  const align = Math.max(1, Math.floor((1 << 20) / sectorSize));
  const [logical, setLogical] = useState(segment.logical);
  // A logical partition needs a free sector before it for its boot record.
  const first = segment.logical || (logical && !gpt) ? segment.startLba + 1 : segment.startLba;
  const start = Math.ceil(first / align) * align;
  const room = Math.max(0, segment.startLba + segment.sectors - start);
  const mib = (1 << 20) / sectorSize;
  const [size, setSize] = useState(String(Math.floor(room / mib)));
  const [unit, setUnit] = useState<"MiB" | "GiB">("MiB");
  const [typeCode, setTypeCode] = useState("ntfs");
  const [name, setName] = useState("");
  const choices: TypeChoice[] = gpt ? view.typesGpt : view.typesMbr;
  const wanted = Number(size) * (unit === "GiB" ? 1024 : 1) * mib;
  const sectors = Math.min(room, Math.floor(wanted));
  const valid = Number.isFinite(wanted) && sectors > 0;

  function create() {
    onOp({ op: "create", startLba: start, sectors, typeCode, logical: !gpt && logical, name: gpt && name.trim() ? name.trim() : null });
    onClose();
  }

  return (
    <div className="edit-form">
      <h3>New partition</h3>
      <p className="muted small">
        In the free space from sector {start.toLocaleString("en-US")}, up to {formatSize(room * sectorSize)}. It isn't
        formatted: Windows offers to format it once it's there.
      </p>
      <div className="edit-row">
        <label>
          Size{" "}
          <input className="size-input" value={size} onChange={(e) => setSize(e.target.value)} inputMode="decimal" />
        </label>
        <select value={unit} onChange={(e) => setUnit(e.target.value as "MiB" | "GiB")} aria-label="Unit">
          <option>MiB</option>
          <option>GiB</option>
        </select>
        <button className="link-button" onClick={() => { setUnit("MiB"); setSize(String(Math.floor(room / mib))); }}>
          All of it
        </button>
        {valid && wanted > room && <span className="muted small">Only {formatSize(room * sectorSize)} fits.</span>}
      </div>
      <div className="edit-row">
        <label>
          Type{" "}
          <select value={typeCode} onChange={(e) => setTypeCode(e.target.value)}>
            {choices.map((c) => (
              <option key={c.code} value={c.code}>
                {c.name}
              </option>
            ))}
          </select>
        </label>
        {!gpt && (
          <label className="checkbox" title="Inside the extended partition (made if there's none): for more than four partitions">
            <input type="checkbox" checked={logical} onChange={(e) => setLogical(e.target.checked)} /> Logical partition
          </label>
        )}
        {gpt && (
          <label>
            Name <input value={name} maxLength={36} onChange={(e) => setName(e.target.value)} placeholder="Basic data partition" />
          </label>
        )}
      </div>
      <div className="button-row">
        <button className="primary" disabled={!valid} onClick={create}>
          Add to the changes
        </button>
        <button onClick={onClose}>Cancel</button>
      </div>
    </div>
  );
}

type FieldProps = {
  form: Extract<EditForm, { kind: "type" | "name" }>;
  view: TableView;
  onOp: (op: Operation) => void;
  onClose: () => void;
};

/** A partition's new type, or (GPT) name. */
export function FieldForm({ form, view, onOp, onClose }: FieldProps) {
  const gpt = view.map.table.kind === "gpt";
  const choices = gpt ? view.typesGpt : view.typesMbr;
  const [value, setValue] = useState(form.kind === "type" ? (choices[0]?.code ?? "") : form.current);
  const [custom, setCustom] = useState("");
  function save() {
    if (form.kind === "type") {
      onOp({ op: "setType", startLba: form.startLba, typeCode: value === "custom" ? custom.trim() : value });
    } else {
      onOp({ op: "setName", startLba: form.startLba, name: value });
    }
    onClose();
  }
  return (
    <div className="edit-form">
      <h3>{form.kind === "type" ? "Change the type" : "Rename"}</h3>
      {form.kind === "type" ? (
        <>
          <p className="muted small">
            Now {form.current}. The type tells systems what's in a partition; the data isn't changed.
          </p>
          <div className="edit-row">
            <select value={value} onChange={(e) => setValue(e.target.value)} aria-label="Type">
              {choices.map((c) => (
                <option key={c.code} value={c.code}>
                  {c.name}
                </option>
              ))}
              <option value="custom">Other (a code)…</option>
            </select>
            {value === "custom" && (
              <input
                value={custom}
                onChange={(e) => setCustom(e.target.value)}
                placeholder={gpt ? "Type GUID" : "0x07"}
                aria-label="Type code"
              />
            )}
          </div>
        </>
      ) : (
        <div className="edit-row">
          <input value={value} maxLength={36} onChange={(e) => setValue(e.target.value)} aria-label="Name" />
        </div>
      )}
      <div className="button-row">
        <button className="primary" disabled={form.kind === "type" && value === "custom" && !custom.trim()} onClick={save}>
          Add to the changes
        </button>
        <button onClick={onClose}>Cancel</button>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------- pending changes

type PendingProps = {
  view: TableView;
  ops: Operation[];
  disabled: boolean;
  onRemove: (index: number) => void;
  onDiscard: () => void;
  onApply: () => void;
};

/** The changes waiting for Apply, each as the engine describes it. */
export function PendingChanges({ view, ops, disabled, onRemove, onDiscard, onApply }: PendingProps) {
  const { plan } = view;
  const ready = !plan.blocked && !plan.error && plan.sectorsWritten > 0 && view.confirmation !== null;
  return (
    <div className="pending">
      <h3>Changes waiting to be written ({ops.length})</h3>
      <p className="muted small">The map above shows the disk as it will be. Nothing is written until you apply them.</p>
      <ol>
        {ops.map((_, i) => {
          const step = plan.steps[i];
          const failed = plan.error?.step === i;
          return (
            <li key={i} className={failed ? "failed" : step ? "" : "not-tried"}>
              <div className="pending-line">
                <span>
                  {step?.description ??
                    (failed ? plan.error?.message : "Not tried: a change before it can't be done.")}
                </span>
                <button className="link-button" onClick={() => onRemove(i)} disabled={disabled}>
                  Remove
                </button>
              </div>
              {step?.warnings.map((w) => (
                <p key={w} className="muted small">
                  {w}
                </p>
              ))}
            </li>
          );
        })}
      </ol>
      <div className="button-row">
        <button className="primary" disabled={!ready || disabled} onClick={onApply}>
          Apply {ops.length === 1 ? "the change" : `${ops.length} changes`}…
        </button>
        <button onClick={onDiscard} disabled={disabled}>
          Discard all
        </button>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------- writing

type ApplyProps = {
  view: TableView;
  diskName: string;
  busy: boolean;
  error: string | null;
  onApply: (confirmation: string) => void;
  onCancel: () => void;
};

/** The last look before writing, and the disk's name to type. */
export function ApplyBox({ view, diskName, busy, error, onApply, onCancel }: ApplyProps) {
  const [typed, setTyped] = useState("");
  const confirmation = view.confirmation ?? "";
  const warnings = [...new Set(view.plan.steps.flatMap((s) => s.warnings))];
  return (
    <div className="confirm-erase">
      <h2>Write these changes to {diskName}?</h2>
      <ul>
        {view.plan.steps.map((s, i) => (
          <li key={i}>{s.description}</li>
        ))}
      </ul>
      {warnings.map((w) => (
        <p key={w} className="small">
          {w}
        </p>
      ))}
      {view.volumes.length > 0 && (
        <p className="small">
          Closed while the table is written (close any files open on them): <strong>{view.volumes.join(", ")}</strong>.
        </p>
      )}
      <p className="small">
        The {view.plan.sectorsWritten === 1 ? "sector" : `${view.plan.sectorsWritten} sectors`} being replaced are saved
        first, so Undo can put the old table back.
      </p>
      <label className="confirm-type">
        Type <code>{confirmation}</code> to confirm:{" "}
        <input value={typed} onChange={(e) => setTyped(e.target.value)} autoFocus spellCheck={false} />
      </label>
      {error && <p className="problem error">{error}</p>}
      <div className="button-row">
        <button
          className="danger"
          disabled={busy || typed.trim().toLowerCase() !== confirmation.toLowerCase()}
          onClick={() => onApply(typed)}
        >
          {busy ? "Writing…" : "Write the changes"}
        </button>
        <button onClick={onCancel} disabled={busy}>
          Cancel
        </button>
      </div>
    </div>
  );
}

type UndoProps = {
  sourceId: string;
  view: TableView;
  diskName: string;
  busy: boolean;
  error: string | null;
  onUndo: (backup: string, confirmation: string) => void;
  onCancel: () => void;
};

/** Putting back the table as it was before a change. */
export function UndoBox({ sourceId, view, diskName, busy, error, onUndo, onCancel }: UndoProps) {
  const choices = view.backups.filter((b) => !b.undone && !b.undoes);
  const [chosen, setChosen] = useState<BackupInfo | null>(choices[0] ?? null);
  const [preview, setPreview] = useState<UndoView | null>(null);
  const [previewError, setPreviewError] = useState<string | null>(null);
  const [typed, setTyped] = useState("");
  const confirmation = view.confirmation ?? "";

  useEffect(() => {
    setPreview(null);
    setPreviewError(null);
    if (!chosen) return;
    api
      .previewUndo(sourceId, chosen.path)
      .then(setPreview)
      .catch((e) => setPreviewError(toCommandError(e).message));
  }, [sourceId, chosen]);

  return (
    <div className="confirm-erase undo-box">
      <h2>Undo a change to {diskName}</h2>
      <ul className="backup-list">
        {choices.map((b) => (
          <li key={b.path}>
            <label>
              <input type="radio" checked={chosen?.path === b.path} onChange={() => setChosen(b)} />{" "}
              <strong>{new Date(b.created).toLocaleString()}</strong>: {b.changes.join("; ")}
            </label>
          </li>
        ))}
      </ul>
      {previewError && <p className="problem error">{previewError}</p>}
      {preview?.refusal && <p className="problem warning">{preview.refusal}</p>}
      {preview?.map && (
        <p className="small">
          Undoing it puts back {preview.map.table.partitions.filter((p) => p.kind !== "extended").length} partition
          {preview.map.table.partitions.filter((p) => p.kind !== "extended").length === 1 ? "" : "s"} as they were. The
          table it replaces is saved first.
        </p>
      )}
      <label className="confirm-type">
        Type <code>{confirmation}</code> to confirm:{" "}
        <input value={typed} onChange={(e) => setTyped(e.target.value)} spellCheck={false} />
      </label>
      {error && <p className="problem error">{error}</p>}
      <div className="button-row">
        <button
          className="danger"
          disabled={busy || !chosen || !preview?.map || typed.trim().toLowerCase() !== confirmation.toLowerCase()}
          onClick={() => chosen && onUndo(chosen.path, typed)}
        >
          {busy ? "Writing…" : "Undo it"}
        </button>
        <button onClick={onCancel} disabled={busy}>
          Cancel
        </button>
      </div>
    </div>
  );
}
