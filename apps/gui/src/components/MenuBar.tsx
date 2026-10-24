import { KeyboardEvent as ReactKeyboardEvent, ReactNode, useEffect, useRef, useState } from "react";
import { Command, MENUS, MenuName } from "../commands";

type Props = {
  commands: Command[];
  /** Shown at the right end: the action search, the administrator status. */
  children?: ReactNode;
};

/** The menu bar: File, Edit, View, Disk, Partition, Recovery, Tools, Help.
 *  Click or Alt to open; arrow keys move; Enter picks; Esc closes. */
export function MenuBar({ commands, children }: Props) {
  const [open, setOpen] = useState<MenuName | null>(null);
  const bar = useRef<HTMLDivElement>(null);
  const buttons = useRef(new Map<MenuName, HTMLButtonElement>());

  // A click anywhere else closes the menu.
  useEffect(() => {
    if (!open) return;
    const close = (e: MouseEvent) => {
      if (!bar.current?.contains(e.target as Node)) setOpen(null);
    };
    window.addEventListener("mousedown", close);
    return () => window.removeEventListener("mousedown", close);
  }, [open]);

  // Pressing and releasing Alt on its own goes to the menu bar, as in Windows.
  useEffect(() => {
    let alone = false;
    const down = (e: KeyboardEvent) => {
      alone = e.key === "Alt" && !e.ctrlKey && !e.shiftKey;
    };
    const up = (e: KeyboardEvent) => {
      if (e.key === "Alt" && alone) {
        e.preventDefault();
        buttons.current.get("File")?.focus();
      }
      alone = false;
    };
    window.addEventListener("keydown", down, true);
    window.addEventListener("keyup", up, true);
    return () => {
      window.removeEventListener("keydown", down, true);
      window.removeEventListener("keyup", up, true);
    };
  }, []);

  function focusItem(menu: HTMLElement | null, step: number) {
    if (!menu) return;
    const items = [...menu.querySelectorAll<HTMLButtonElement>("button[role=menuitem]:not(:disabled)")];
    if (items.length === 0) return;
    const at = items.indexOf(document.activeElement as HTMLButtonElement);
    const next = at < 0 ? (step > 0 ? 0 : items.length - 1) : (at + step + items.length) % items.length;
    items[next].focus();
  }

  function openMenu(name: MenuName, focusFirst: boolean) {
    setOpen(name);
    if (focusFirst) {
      requestAnimationFrame(() => focusItem(bar.current?.querySelector(`[data-menu="${name}"]`) ?? null, 1));
    }
  }

  function step(name: MenuName, by: number) {
    const next = MENUS[(MENUS.indexOf(name) + by + MENUS.length) % MENUS.length];
    buttons.current.get(next)?.focus();
    if (open) openMenu(next, true);
  }

  function onButtonKey(name: MenuName, e: ReactKeyboardEvent) {
    if (e.key === "ArrowDown" || e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      openMenu(name, true);
    } else if (e.key === "ArrowRight") {
      e.preventDefault();
      step(name, 1);
    } else if (e.key === "ArrowLeft") {
      e.preventDefault();
      step(name, -1);
    } else if (e.key === "Escape") {
      setOpen(null);
      (e.target as HTMLElement).blur();
    }
  }

  function onMenuKey(name: MenuName, e: ReactKeyboardEvent<HTMLDivElement>) {
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      focusItem(e.currentTarget, e.key === "ArrowDown" ? 1 : -1);
    } else if (e.key === "ArrowRight" || e.key === "ArrowLeft") {
      e.preventDefault();
      step(name, e.key === "ArrowRight" ? 1 : -1);
    } else if (e.key === "Escape") {
      e.preventDefault();
      setOpen(null);
      buttons.current.get(name)?.focus();
    }
  }

  function run(command: Command) {
    setOpen(null);
    command.run();
  }

  return (
    <div className="menubar" ref={bar}>
      <span className="menubar-app">DataReco</span>
      <nav className="menubar-menus" aria-label="Menu bar">
        {MENUS.map((name) => {
          const items = commands.filter((c) => c.menu === name);
          return (
            <div key={name} className="menubar-item">
              <button
                ref={(el) => {
                  if (el) buttons.current.set(name, el);
                }}
                className={open === name ? "menubar-button open" : "menubar-button"}
                aria-haspopup="menu"
                aria-expanded={open === name}
                onClick={() => (open === name ? setOpen(null) : openMenu(name, false))}
                onMouseEnter={() => open && open !== name && setOpen(name)}
                onKeyDown={(e) => onButtonKey(name, e)}
              >
                {name}
              </button>
              {open === name && (
                <div className="menu" role="menu" aria-label={name} data-menu={name} onKeyDown={(e) => onMenuKey(name, e)}>
                  {items.map((command, i) => (
                    <div key={command.id} className="menu-group">
                      {i > 0 && items[i - 1].group !== command.group && <div className="menu-separator" role="separator" />}
                      <button
                        role="menuitem"
                        className={command.danger ? "menu-entry danger-text" : "menu-entry"}
                        disabled={!!command.unavailable}
                        title={command.unavailable ?? command.hint}
                        onClick={() => run(command)}
                      >
                        <span>{command.label}</span>
                        {command.shortcut && <span className="menu-shortcut">{command.shortcut}</span>}
                      </button>
                    </div>
                  ))}
                </div>
              )}
            </div>
          );
        })}
      </nav>
      <div className="menubar-right">{children}</div>
    </div>
  );
}
