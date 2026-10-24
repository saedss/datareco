import { useCallback, useEffect, useRef, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { api, AppInfo, DeviceList, TemplateList, toCommandError } from "./api";
import { ActivityPanel, ActivityProvider, useActivities } from "./activity";
import { Intent, buildCommands, matches } from "./commands";
import { CommandPalette } from "./components/CommandPalette";
import { AboutDialog, ConfirmDialog, ShortcutsDialog } from "./components/Dialogs";
import { DeviceSidebar, Selection, findSelection, imageSelection, runsWindows } from "./components/DeviceSidebar";
import { DiskEditor } from "./components/DiskEditor";
import { MenuBar } from "./components/MenuBar";
import "./App.css";

const IMAGE_FILTERS = [
  { name: "Disk images", extensions: ["img", "dd", "raw", "bin", "ima", "iso"] },
  { name: "All files", extensions: ["*"] },
];

export default function App() {
  return (
    <ActivityProvider>
      <Shell />
    </ActivityProvider>
  );
}

type Dialog = "shortcuts" | "about" | "close" | "exit" | "restart" | null;

/** What closing a disk, or DataReco, would cut short. */
type Stake = { tasks: number; changes: number };

/** Says what closing loses, for the dialogs that ask first. */
function StakeText({ stake }: { stake: Stake }) {
  const { tasks, changes } = stake;
  if (tasks === 0 && changes === 0) return <p>Nothing is running any more, and no changes are waiting.</p>;
  return (
    <>
      {tasks > 0 && (
        <p>
          {tasks === 1 ? "A task is" : `${tasks} tasks are`} still running or paused, and will stop. Copies and saved
          searches can carry on later from their map or file, and a move from its journal; a wipe or a clone stopped
          halfway leaves that disk half done.
        </p>
      )}
      {changes > 0 && (
        <p>
          {changes === 1 ? "A partition table change" : `${changes} partition table changes`} you haven't applied will
          be lost. Nothing has been written for {changes === 1 ? "it" : "them"}.
        </p>
      )}
    </>
  );
}

function Shell() {
  const [info, setInfo] = useState<AppInfo | null>(null);
  const [engineError, setEngineError] = useState<string | null>(null);
  const [devices, setDevices] = useState<DeviceList | null>(null);
  const [devicesError, setDevicesError] = useState<string | null>(null);
  const [images, setImages] = useState<string[]>([]);
  // Every disk, volume or image opened so far stays open, so what runs on
  // it keeps running while another is shown.
  const [opened, setOpened] = useState<Selection[]>([]);
  const [currentId, setCurrentId] = useState<string | null>(null);
  const [requests, setRequests] = useState<Record<string, Intent & { nonce: number }>>({});
  const [restartError, setRestartError] = useState<string | null>(null);
  const [templates, setTemplates] = useState<TemplateList | null>(null);
  const [activityOpen, setActivityOpen] = useState(false);
  const [searchOpen, setSearchOpen] = useState(false);
  const [dialog, setDialog] = useState<Dialog>(null);
  // Partition table changes waiting for Apply, by source.
  const [pending, setPending] = useState<Record<string, number>>({});
  const nonce = useRef(0);
  const activities = useActivities();

  const notePending = useCallback((id: string, count: number) => {
    setPending((current) => ((current[id] ?? 0) === count ? current : { ...current, [id]: count }));
  }, []);

  /** What closing `id` (or, with null, every disk) would cut short. */
  const stake = (id: string | null): Stake => ({
    tasks: activities.filter(
      (a) => (id === null || a.sourceId === id) && (a.state === "running" || a.state === "paused"),
    ).length,
    changes:
      id === null ? Object.values(pending).reduce((sum, n) => sum + n, 0) : (pending[id] ?? 0),
  });
  const atStake = (s: Stake) => s.tasks + s.changes > 0;

  // Closing the window (its X, Alt+F4, the taskbar) asks first too.
  const askOnClose = useRef(false);
  askOnClose.current = atStake(stake(null));
  useEffect(() => {
    let unlisten: (() => void) | null = null;
    let gone = false;
    try {
      getCurrentWindow()
        .onCloseRequested((event) => {
          if (!askOnClose.current) return;
          event.preventDefault();
          setDialog("exit");
        })
        .then((stop) => (gone ? stop() : (unlisten = stop)))
        .catch(() => {});
    } catch {
      // Not in the app's window (a plain browser): nothing to ask.
    }
    return () => {
      gone = true;
      unlisten?.();
    };
  }, []);

  useEffect(() => {
    api.listTemplates().then(setTemplates).catch(() => setTemplates(null));
  }, []);

  const reloadTemplates = useCallback(() => {
    api.reloadTemplates().then(setTemplates).catch(() => {});
  }, []);

  const refreshDevices = useCallback(() => {
    setDevicesError(null);
    api
      .listDevices()
      .then(setDevices)
      .catch((e) => setDevicesError(toCommandError(e).message));
  }, []);

  useEffect(() => {
    api
      .appInfo()
      .then(setInfo)
      .catch((e) => setEngineError(String(e)));
    refreshDevices();
  }, [refreshDevices]);

  const select = useCallback((selection: Selection) => {
    setOpened((current) => (current.some((s) => s.id === selection.id) ? current : [...current, selection]));
    setCurrentId(selection.id);
  }, []);

  /** Asks a disk's workspace to show a tab (and what's in it). */
  const go = useCallback((intent: Intent, id: string | null) => {
    if (!id) return;
    setCurrentId(id);
    setRequests((current) => ({ ...current, [id]: { ...intent, nonce: ++nonce.current } }));
  }, []);

  // Show the source passed with --open: images right away, disks and volumes
  // once the device list has loaded (for their names).
  const startSource = info?.startSource ?? null;
  const startShown = useRef(false);
  useEffect(() => {
    if (!startSource || startShown.current) return;
    if (startSource.startsWith("image:")) {
      const path = startSource.slice("image:".length);
      setImages((current) => (current.includes(path) ? current : [...current, path]));
      select(imageSelection(path));
      startShown.current = true;
    } else if (devices) {
      select(findSelection(startSource, devices) ?? { id: startSource, title: startSource, subtitle: "" });
      startShown.current = true;
    }
  }, [startSource, devices, select]);

  async function openImage() {
    const path = await open({ title: "Open a disk image", multiple: false, directory: false, filters: IMAGE_FILTERS });
    if (typeof path !== "string") return;
    showImage(path);
  }

  function showImage(path: string) {
    setImages((current) => (current.includes(path) ? current : [...current, path]));
    select(imageSelection(path));
  }

  /** Restarting closes DataReco, so it asks first when that loses something. */
  function restartAsAdmin() {
    if (atStake(stake(null))) {
      setDialog("restart");
      return;
    }
    restart();
  }

  function restart() {
    setRestartError(null);
    api.restartAsAdmin(currentId).catch((e) => setRestartError(toCommandError(e).message));
  }

  function closeCurrent(force = false) {
    if (!currentId) return;
    if (!force && atStake(stake(currentId))) {
      setDialog("close");
      return;
    }
    const id = currentId;
    setOpened((current) => current.filter((s) => s.id !== id));
    setRequests(({ [id]: _, ...rest }) => rest);
    setPending(({ [id]: _, ...rest }) => rest);
    setCurrentId(null);
  }

  function exit(force = false) {
    if (!force && atStake(stake(null))) {
      setDialog("exit");
      return;
    }
    // destroy, not close: close would ask again.
    getCurrentWindow()
      .destroy()
      .catch(() => {});
  }

  const current = opened.find((s) => s.id === currentId) ?? null;
  const commands = buildCommands({
    source: current
      ? {
          id: current.id,
          title: current.title,
          kind: current.id.startsWith("image:") ? "image" : current.id.startsWith("volume:") ? "volume" : "disk",
          windows: runsWindows(current.id, devices),
        }
      : null,
    elevated: info?.elevated ?? false,
    go: (intent) => go(intent, currentId),
    home: () => setCurrentId(null),
    openImage,
    close: () => closeCurrent(),
    exit: () => exit(),
    restartAsAdmin,
    refresh: refreshDevices,
    toggleActivity: () => setActivityOpen((o) => !o),
    openSearch: () => setSearchOpen(true),
    showShortcuts: () => setDialog("shortcuts"),
    showAbout: () => setDialog("about"),
  });

  // Keyboard shortcuts, everywhere.
  const latest = useRef(commands);
  latest.current = commands;
  useEffect(() => {
    const key = (e: KeyboardEvent) => {
      const command = latest.current.find((c) => c.shortcut && c.id !== "exit" && matches(c.shortcut, e));
      if (!command) return;
      // Ctrl+F and F5 mean something to the web view too; here they're ours.
      e.preventDefault();
      if (!command.unavailable) command.run();
    };
    window.addEventListener("keydown", key);
    return () => window.removeEventListener("keydown", key);
  }, []);

  const running = activities.filter((a) => a.state === "running").length;
  const sourceName = (id: string) =>
    opened.find((s) => s.id === id)?.title ?? images.find((p) => `image:${p}` === id) ?? id;

  return (
    <div className="shell">
      <MenuBar commands={commands}>
        <button className="menubar-search" onClick={() => setSearchOpen(true)}>
          <span>Search actions</span>
          <kbd>Ctrl+K</kbd>
        </button>
        {info &&
          (info.elevated ? (
            <span className="admin-pill ok" title="DataReco can read your disks">
              Administrator
            </span>
          ) : (
            <button className="admin-pill" onClick={restartAsAdmin} title="Reading disks needs administrator rights">
              Limited access · Restart as administrator
            </button>
          ))}
      </MenuBar>

      <div className="shell-body">
        <DeviceSidebar
          devices={devices}
          devicesError={devicesError}
          images={images}
          selectedId={currentId}
          elevated={info?.elevated ?? false}
          onSelect={select}
          onHome={() => setCurrentId(null)}
          onOpenImage={openImage}
          onRefresh={refreshDevices}
          onRestartAsAdmin={restartAsAdmin}
        />

        <main className="workspace">
          {currentId === null && (
            <Home
              elevated={info?.elevated ?? true}
              onSearch={() => setSearchOpen(true)}
              onOpenImage={openImage}
              onRestartAsAdmin={restartAsAdmin}
            />
          )}
          {opened.map((s) => (
            <div key={s.id} className="workspace-pane" hidden={s.id !== currentId}>
              <DiskEditor
                sourceId={s.id}
                title={s.title}
                subtitle={s.subtitle}
                windows={runsWindows(s.id, devices)}
                templates={templates}
                request={requests[s.id] ?? null}
                onReloadTemplates={reloadTemplates}
                onRestartAsAdmin={restartAsAdmin}
                onOpenImage={showImage}
                onPending={(count) => notePending(s.id, count)}
              />
            </div>
          ))}
        </main>
      </div>

      {activityOpen && (
        <ActivityPanel
          sourceName={sourceName}
          onShow={(a) => go({ view: a.view }, a.sourceId)}
          onClose={() => setActivityOpen(false)}
        />
      )}

      <footer className="statusbar">
        {engineError ? (
          <span className="error-text">Engine error: {engineError}</span>
        ) : (
          <>
            <span>{info ? `${info.name} ${info.version}` : "Connecting to engine…"}</span>
            <span className="status-safe">Safe: nothing is written until you confirm</span>
            {info && <span>{info.elevated ? "Administrator" : "Not running as administrator"}</span>}
            {restartError && <span className="error-text">Couldn't restart as administrator: {restartError}</span>}
          </>
        )}
        <span className="status-spacer" />
        <button className="status-button" onClick={() => setActivityOpen((o) => !o)}>
          Activity{running > 0 ? `: ${running} running` : activities.length > 0 ? `: ${activities.length}` : ""} (Ctrl+J)
        </button>
        <button className="status-button" onClick={() => setSearchOpen(true)}>
          Search actions (Ctrl+K)
        </button>
      </footer>

      {searchOpen && <CommandPalette commands={commands} onClose={() => setSearchOpen(false)} />}
      {dialog === "shortcuts" && <ShortcutsDialog commands={commands} onClose={() => setDialog(null)} />}
      {dialog === "about" && <AboutDialog info={info} onClose={() => setDialog(null)} />}
      {dialog === "close" && (
        <ConfirmDialog
          title={`Close ${current?.title ?? "it"}?`}
          confirm="Close it"
          onConfirm={() => closeCurrent(true)}
          onClose={() => setDialog(null)}
        >
          <StakeText stake={stake(currentId)} />
        </ConfirmDialog>
      )}
      {dialog === "exit" && (
        <ConfirmDialog title="Exit DataReco?" confirm="Exit" onConfirm={() => exit(true)} onClose={() => setDialog(null)}>
          <StakeText stake={stake(null)} />
        </ConfirmDialog>
      )}
      {dialog === "restart" && (
        <ConfirmDialog
          title="Restart as administrator?"
          confirm="Restart"
          onConfirm={restart}
          onClose={() => setDialog(null)}
        >
          <p>DataReco closes and opens again with administrator rights.</p>
          <StakeText stake={stake(null)} />
        </ConfirmDialog>
      )}
    </div>
  );
}

type HomeProps = {
  elevated: boolean;
  onSearch: () => void;
  onOpenImage: () => void;
  onRestartAsAdmin: () => void;
};

function Home({ elevated, onSearch, onOpenImage, onRestartAsAdmin }: HomeProps) {
  return (
    <div className="home">
      <h1>DataReco</h1>
      <p className="home-lead">
        Pick a disk, a partition under it, or an image file on the left to see what's on it and what you can do with
        it.
      </p>
      {!elevated && (
        <div className="banner warning home-admin">
          Windows lets programs read disks only with administrator rights. Image files work without.{" "}
          <button onClick={onRestartAsAdmin}>Restart as administrator</button>
        </div>
      )}
      <div className="home-hints">
        <div>
          <strong>Every action is in the menus</strong>
          <span className="muted">and Ctrl+K finds any of them by name: try "recover", "image" or "format".</span>
        </div>
        <div>
          <strong>Safe by default</strong>
          <span className="muted">DataReco only reads disks until you confirm a change and type the disk's name.</span>
        </div>
        <div>
          <strong>Several things at once</strong>
          <span className="muted">
            Copies, searches and tests keep running while you look at other disks. Activity (Ctrl+J) lists them.
          </span>
        </div>
      </div>
      <div className="button-row">
        <button className="primary" onClick={onSearch}>
          Search actions (Ctrl+K)
        </button>
        <button onClick={onOpenImage}>Open a disk image…</button>
      </div>
    </div>
  );
}
