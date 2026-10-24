import { ReactNode, createContext, useCallback, useContext, useEffect, useMemo, useRef, useState } from "react";

/** The tabs of a disk's (or image's) workspace. */
export type View = "overview" | "files" | "recover" | "partitions" | "backup" | "health" | "hex";

/** `paused` can carry on; `stopped` can only be started again. */
export type ActivityState = "running" | "paused" | "stopped" | "finished" | "failed";

/** A long task as the Activity panel shows it. */
export type ActivityReport = {
  /** What it is: "Surface test", "Image into disk1.img". */
  label: string;
  /** How far it is, in words. */
  detail: string;
  /** 0–100, or null when it can't say. */
  percent: number | null;
  state: ActivityState;
  /** The tab it runs in, to go and see it. */
  view: View;
};

export type Activity = ActivityReport & { id: string; sourceId: string };

/** What the Activity panel can ask a task to do. */
export type ActivityActions = { pause?: () => void; resume?: () => void; stop?: () => void };

/** The part that never changes, so reporting doesn't re-render reporters. */
type Reporter = {
  publish: (id: string, sourceId: string, report: ActivityReport | null) => void;
  dismiss: (id: string) => void;
  actions: React.MutableRefObject<Map<string, ActivityActions>>;
};

const ReporterContext = createContext<Reporter | null>(null);
const ListContext = createContext<Activity[]>([]);

function same(a: Activity, b: ActivityReport): boolean {
  return (
    a.label === b.label &&
    a.detail === b.detail &&
    a.percent === b.percent &&
    a.state === b.state &&
    a.view === b.view
  );
}

/** Holds every long task's latest report, from whichever disk it runs on. */
export function ActivityProvider({ children }: { children: ReactNode }) {
  const [entries, setEntries] = useState<Map<string, Activity>>(new Map());
  // A finished task the person dismissed stays dismissed until it changes.
  const dismissed = useRef(new Map<string, string>());
  const actions = useRef(new Map<string, ActivityActions>());

  const publish = useCallback((id: string, sourceId: string, report: ActivityReport | null) => {
    setEntries((current) => {
      if (!report) {
        if (!current.has(id)) return current;
        const next = new Map(current);
        next.delete(id);
        return next;
      }
      const signature = `${report.state}|${report.label}`;
      if (dismissed.current.get(id) === signature) return current;
      dismissed.current.delete(id);
      const old = current.get(id);
      if (old && old.sourceId === sourceId && same(old, report)) return current;
      const next = new Map(current);
      next.set(id, { ...report, id, sourceId });
      return next;
    });
  }, []);

  const dismiss = useCallback((id: string) => {
    setEntries((current) => {
      const entry = current.get(id);
      if (!entry) return current;
      dismissed.current.set(id, `${entry.state}|${entry.label}`);
      const next = new Map(current);
      next.delete(id);
      return next;
    });
  }, []);

  const reporter = useMemo(() => ({ publish, dismiss, actions }), [publish, dismiss]);
  const list = useMemo(() => [...entries.values()], [entries]);
  return (
    <ReporterContext.Provider value={reporter}>
      <ListContext.Provider value={list}>{children}</ListContext.Provider>
    </ReporterContext.Provider>
  );
}

/** Reports a component's long task to the Activity panel while `report`
 *  isn't null; it's taken off when the component goes. */
export function useActivity(id: string, sourceId: string, report: ActivityReport | null, actions: ActivityActions = {}) {
  const reporter = useContext(ReporterContext);
  // The latest actions, kept without publishing again.
  useEffect(() => {
    reporter?.actions.current.set(id, actions);
  });
  // Published again only when what it says changes (`key` stands for `report`).
  const key = report ? JSON.stringify(report) : "";
  useEffect(() => {
    reporter?.publish(id, sourceId, key ? (JSON.parse(key) as ActivityReport) : null);
  }, [reporter, key, id, sourceId]);
  useEffect(
    () => () => {
      reporter?.publish(id, sourceId, null);
      reporter?.actions.current.delete(id);
    },
    [reporter, id, sourceId],
  );
}

export function useActivities(): Activity[] {
  return useContext(ListContext);
}

const ORDER: Record<ActivityState, number> = { running: 0, paused: 1, failed: 2, stopped: 3, finished: 4 };

const STATE_TEXT: Record<ActivityState, string> = {
  running: "Running",
  paused: "Paused",
  stopped: "Stopped",
  failed: "Stopped with an error",
  finished: "Finished",
};

type PanelProps = {
  /** "Disk 1", for a source id. */
  sourceName: (sourceId: string) => string;
  onShow: (activity: Activity) => void;
  onClose: () => void;
};

/** Every running, paused and finished task, on any disk. */
export function ActivityPanel({ sourceName, onShow, onClose }: PanelProps) {
  const reporter = useContext(ReporterContext);
  const activities = [...useContext(ListContext)].sort((a, b) => ORDER[a.state] - ORDER[b.state]);
  return (
    <section className="activity-panel" aria-label="Activity">
      <div className="activity-head">
        <h2>Activity</h2>
        <span className="muted small">
          Copies, searches, tests and moves, on every disk. They keep running while you look at something else.
        </span>
        <button className="link-button" onClick={onClose}>
          Hide (Ctrl+J)
        </button>
      </div>
      {activities.length === 0 && <p className="muted small activity-empty">Nothing has run yet.</p>}
      <ul className="activity-list">
        {activities.map((a) => {
          const act = reporter?.actions.current.get(a.id) ?? {};
          const width = a.percent ?? (a.state === "finished" ? 100 : 0);
          return (
            <li key={a.id} className={`activity ${a.state}`}>
              <span className="activity-what">
                <strong>{sourceName(a.sourceId)}</strong> · {a.label}
              </span>
              <span className="activity-bar" aria-hidden="true">
                <span style={{ width: `${Math.max(0, Math.min(100, width))}%` }} />
              </span>
              <span className="activity-detail small">
                <span className={`activity-state ${a.state}`}>{STATE_TEXT[a.state]}</span>
                {a.percent !== null && a.state !== "finished" && ` · ${a.percent.toFixed(a.percent < 10 ? 1 : 0)}%`}
                {a.detail && ` · ${a.detail}`}
              </span>
              <span className="activity-actions">
                {a.state === "running" && act.pause && <button onClick={act.pause}>Pause</button>}
                {a.state === "paused" && act.resume && <button onClick={act.resume}>Carry on</button>}
                {(a.state === "running" || a.state === "paused") && act.stop && (
                  <button className="stop" onClick={act.stop}>
                    Stop
                  </button>
                )}
                <button onClick={() => onShow(a)}>Show</button>
                {(a.state === "finished" || a.state === "failed" || a.state === "stopped") && (
                  <button className="link-button" onClick={() => reporter?.dismiss(a.id)}>
                    Dismiss
                  </button>
                )}
              </span>
            </li>
          );
        })}
      </ul>
    </section>
  );
}
