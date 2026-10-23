import { useEffect, useRef, useState } from "react";
import { TemplateList, TemplateNode, TemplateSummary, api, toCommandError } from "../api";
import { formatNumber, hex } from "../format";
import { Range } from "./HexGrid";

type Props = {
  sourceId: string;
  sectorSize: number;
  cursor: number;
  templates: TemplateList | null;
  onReloadTemplates: () => void;
  onSelectRange: (range: Range) => void;
};

type Decoded = { node: TemplateNode; templateId: string };

/** Suggests structures for the cursor's sector and shows decoded fields. */
export function StructurePanel({ sourceId, sectorSize, cursor, templates, ...props }: Props) {
  const sectorStart = cursor - (cursor % sectorSize);
  const [suggestions, setSuggestions] = useState<TemplateSummary[]>([]);
  const [chosen, setChosen] = useState("");
  const [decoded, setDecoded] = useState<Decoded | null>(null);
  const [active, setActive] = useState<TemplateNode | null>(null);
  const [error, setError] = useState<string | null>(null);
  const latestDetect = useRef(0);

  useEffect(() => {
    const request = ++latestDetect.current;
    api
      .detectTemplates(sourceId, sectorStart)
      .then((found) => request === latestDetect.current && setSuggestions(found))
      .catch(() => request === latestDetect.current && setSuggestions([]));
  }, [sourceId, sectorStart]);

  const list = templates?.templates ?? [];
  const groups = [...groupBy(list, (t) => t.group || "Other")].sort(
    ([a], [b]) => groupRank(a) - groupRank(b) || a.localeCompare(b),
  );
  // Until the user picks one, the dropdown follows the best suggestion.
  const chosenId = chosen || suggestions[0]?.id || groups[0]?.[1][0]?.id || "";

  async function decode(templateId: string, offset: number) {
    setError(null);
    try {
      const node = await api.decodeTemplate(sourceId, templateId, offset);
      setDecoded({ node, templateId });
      setActive(null);
      props.onSelectRange({ start: node.offset, length: node.size });
    } catch (e) {
      setError(toCommandError(e).message);
    }
  }

  function select(node: TemplateNode) {
    setActive(node);
    props.onSelectRange({ start: node.offset, length: Math.max(1, node.size) });
  }

  return (
    <div className="structure-panel">
      <section className="panel-section">
        <h3>At sector {formatNumber(sectorStart / sectorSize)}</h3>
        {suggestions.length === 0 ? (
          <p className="muted small">No known structure starts here.</p>
        ) : (
          <div className="chips">
            {suggestions.map((s) => (
              <button key={s.id} className="chip" title={s.description} onClick={() => decode(s.id, sectorStart)}>
                {s.name}
              </button>
            ))}
          </div>
        )}
      </section>

      <section className="panel-section decode-at-cursor">
        <select value={chosenId} onChange={(e) => setChosen(e.target.value)} aria-label="Template">
          {groups.map(([group, items]) => (
            <optgroup key={group} label={group}>
              {items.map((t) => (
                <option key={t.id} value={t.id}>
                  {t.name}
                </option>
              ))}
            </optgroup>
          ))}
        </select>
        <button onClick={() => decode(chosenId, cursor)} disabled={!chosenId}>
          Decode at cursor
        </button>
      </section>

      {error && <div className="banner error">{error}</div>}

      {decoded && (
        <section className="tree" aria-label="Decoded structure">
          <div className="tree-title">
            <div>
              <strong>{decoded.node.name}</strong>
              <span className="muted small">
                {" "}
                at 0x{hex(decoded.node.offset, 1)} · {formatNumber(decoded.node.size)} bytes
              </span>
            </div>
            <button className="link-button" onClick={() => setDecoded(null)}>
              Close
            </button>
          </div>
          {decoded.node.value && <p className="muted small tree-summary">{decoded.node.value}</p>}
          {decoded.node.children.map((child, i) => (
            <TreeRow key={`${decoded.templateId}-${i}`} node={child} depth={0} active={active} onSelect={select} />
          ))}
        </section>
      )}

      <footer className="panel-footer">
        <span className="muted small">Your templates:</span>
        <button className="link-button" onClick={() => api.openTemplatesFolder().catch(() => {})}>
          Open folder
        </button>
        <button className="link-button" onClick={props.onReloadTemplates}>
          Reload
        </button>
        {templates && templates.problems.length > 0 && (
          <details className="problems">
            <summary className="error-text">
              {templates.problems.length} problem{templates.problems.length === 1 ? "" : "s"} loading templates
            </summary>
            <ul>
              {templates.problems.map((problem) => (
                <li key={problem}>{problem}</li>
              ))}
            </ul>
          </details>
        )}
      </footer>
    </div>
  );
}

type RowProps = {
  node: TemplateNode;
  depth: number;
  active: TemplateNode | null;
  onSelect: (node: TemplateNode) => void;
};

function TreeRow({ node, depth, active, onSelect }: RowProps) {
  // Small structures start open; long arrays (GPT entries, directories) start closed.
  const [open, setOpen] = useState(depth === 0 && node.children.length <= 16);
  const hasChildren = node.children.length > 0;
  const classes = ["tree-row"];
  if (node.problem) classes.push("problem");
  if (node === active) classes.push("active");
  return (
    <>
      <div
        className={classes.join(" ")}
        style={{ paddingLeft: 6 + depth * 14 }}
        onClick={() => onSelect(node)}
        title={node.description ?? undefined}
      >
        <span
          className="tree-toggle"
          onClick={(e) => {
            e.stopPropagation();
            setOpen(!open);
          }}
        >
          {hasChildren ? (open ? "▾" : "▸") : ""}
        </span>
        <span className="tree-name">{node.name}</span>
        <span className="tree-value">
          {node.value}
          {node.note && <span className="tree-note">{node.note}</span>}
        </span>
      </div>
      {open &&
        node.children.map((child, i) => (
          <TreeRow key={i} node={child} depth={depth + 1} active={active} onSelect={onSelect} />
        ))}
    </>
  );
}

/** Built-in groups in the order people meet them on a disk; others after. */
const GROUP_ORDER = ["Partition tables", "NTFS", "FAT", "exFAT", "ext"];

function groupRank(group: string): number {
  const rank = GROUP_ORDER.indexOf(group);
  return rank === -1 ? GROUP_ORDER.length : rank;
}

function groupBy<T>(items: T[], key: (item: T) => string): Map<string, T[]> {
  const groups = new Map<string, T[]>();
  for (const item of items) {
    const name = key(item);
    groups.set(name, [...(groups.get(name) ?? []), item]);
  }
  return groups;
}
