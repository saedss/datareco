import { ReactNode, useEffect, useRef } from "react";
import { AppInfo } from "../api";
import { Command, MENUS } from "../commands";

type ModalProps = {
  title: string;
  onClose: () => void;
  children: ReactNode;
  /** Buttons at the bottom; a Close button when left out. */
  actions?: ReactNode;
};

/** A dialog over the app: Esc or a click outside closes it. */
export function Modal({ title, onClose, children, actions }: ModalProps) {
  const box = useRef<HTMLDivElement>(null);
  useEffect(() => {
    box.current?.querySelector<HTMLElement>("button, input")?.focus();
    const key = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("keydown", key);
    return () => window.removeEventListener("keydown", key);
  }, [onClose]);
  return (
    <div className="overlay" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className="modal" role="dialog" aria-modal="true" aria-label={title} ref={box}>
        <h2>{title}</h2>
        <div className="modal-body">{children}</div>
        <div className="button-row modal-actions">{actions ?? <button onClick={onClose}>Close</button>}</div>
      </div>
    </div>
  );
}

/** Every shortcut, by menu. */
export function ShortcutsDialog({ commands, onClose }: { commands: Command[]; onClose: () => void }) {
  return (
    <Modal title="Keyboard shortcuts" onClose={onClose}>
      <p className="muted small">
        Alt, then the arrow keys, moves through the menus. Ctrl+K finds any action by name.
      </p>
      <div className="shortcut-groups">
        {MENUS.map((menu) => {
          const keyed = commands.filter((c) => c.menu === menu && c.shortcut);
          if (keyed.length === 0) return null;
          return (
            <section key={menu}>
              <h3>{menu}</h3>
              <dl className="shortcut-list">
                {keyed.map((c) => (
                  <div key={c.id}>
                    <dt>
                      <kbd>{c.shortcut}</kbd>
                    </dt>
                    <dd>{c.label}</dd>
                  </div>
                ))}
              </dl>
            </section>
          );
        })}
      </div>
    </Modal>
  );
}

export function AboutDialog({ info, onClose }: { info: AppInfo | null; onClose: () => void }) {
  return (
    <Modal title="About DataReco" onClose={onClose}>
      <p>
        <strong>
          {info?.name ?? "DataReco"} {info?.version}
        </strong>
        <br />
        A disk editor, data recovery tool and partition manager.
      </p>
      <p className="muted small">
        {info ? `${info.os}/${info.arch} · ${info.elevated ? "running as administrator" : "not running as administrator"}` : ""}
        <br />
        Table backups and move journals are kept in %LOCALAPPDATA%\DataReco.
      </p>
    </Modal>
  );
}

type ConfirmProps = {
  title: string;
  children: ReactNode;
  confirm: string;
  onConfirm: () => void;
  onClose: () => void;
};

/** Asks before doing something, such as closing a disk with tasks running. */
export function ConfirmDialog({ title, children, confirm, onConfirm, onClose }: ConfirmProps) {
  return (
    <Modal
      title={title}
      onClose={onClose}
      actions={
        <>
          <button onClick={onClose}>Cancel</button>
          <button
            className="danger"
            onClick={() => {
              onClose();
              onConfirm();
            }}
          >
            {confirm}
          </button>
        </>
      }
    >
      {children}
    </Modal>
  );
}
