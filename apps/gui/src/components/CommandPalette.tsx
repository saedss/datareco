import { useEffect, useRef, useState } from "react";
import { Command, score } from "../commands";

type Props = {
  commands: Command[];
  onClose: () => void;
};

/** Finds any action by name (Ctrl+K): type, pick with the arrows, Enter. */
export function CommandPalette({ commands, onClose }: Props) {
  const [query, setQuery] = useState("");
  const [active, setActive] = useState(0);
  const input = useRef<HTMLInputElement>(null);
  const list = useRef<HTMLUListElement>(null);

  useEffect(() => input.current?.focus(), []);

  const shown = commands
    .map((command) => ({ command, score: score(command, query) }))
    .filter((r) => r.score > 0)
    // Usable ones first, then the best matches.
    .sort((a, b) => Number(!!a.command.unavailable) - Number(!!b.command.unavailable) || b.score - a.score)
    .slice(0, 12)
    .map((r) => r.command);

  useEffect(() => {
    list.current?.querySelector(`[data-index="${active}"]`)?.scrollIntoView({ block: "nearest" });
  }, [active]);

  function run(command: Command | undefined) {
    if (!command || command.unavailable) return;
    onClose();
    command.run();
  }

  return (
    <div className="overlay" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className="palette" role="dialog" aria-label="Search actions">
        <input
          ref={input}
          className="palette-input"
          value={query}
          placeholder="Search actions, for example: image, recover, format"
          aria-label="Search actions"
          spellCheck={false}
          onChange={(e) => {
            setQuery(e.target.value);
            setActive(0);
          }}
          onKeyDown={(e) => {
            if (e.key === "ArrowDown") {
              e.preventDefault();
              setActive((a) => Math.min(a + 1, shown.length - 1));
            } else if (e.key === "ArrowUp") {
              e.preventDefault();
              setActive((a) => Math.max(a - 1, 0));
            } else if (e.key === "Enter") {
              e.preventDefault();
              run(shown[active]);
            } else if (e.key === "Escape") {
              e.preventDefault();
              onClose();
            }
          }}
        />
        <ul className="palette-list" ref={list} role="listbox" aria-label="Actions">
          {shown.length === 0 && <li className="palette-empty muted">No action matches “{query}”.</li>}
          {shown.map((command, i) => (
            <li key={command.id} role="option" aria-selected={i === active} data-index={i}>
              <button
                className={i === active ? "palette-item active" : "palette-item"}
                disabled={!!command.unavailable}
                onMouseEnter={() => setActive(i)}
                onClick={() => run(command)}
              >
                <span className="palette-label">
                  {command.label}
                  {(command.unavailable ?? command.hint) && (
                    <span className="palette-hint">{command.unavailable ?? command.hint}</span>
                  )}
                </span>
                <span className="palette-meta">{command.shortcut ?? command.menu}</span>
              </button>
            </li>
          ))}
        </ul>
      </div>
    </div>
  );
}
