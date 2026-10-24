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

type Dialog = "shortcuts" | "about" | "close" | "exit" | null;

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
  const nonce = useRef(0);
  const activities = useActivities();

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

  function restartAsAdmin() {
    setRestartError(null);
    api.restartAsAdmin(currentId).catch((e) => setRestartError(toCommandError(e).message));
  }

  const busy = (id: string | null) =>
    activities.some((a) => (id === null || a.sourceId === id) && (a.state === "running" || a.state === "paused"));

  function closeCurrent(force = false) {
    if (!currentId) return;
    if (!force && busy(currentId)) {
      setDialog("close");
      return;
    }
    const id = currentId;
    setOpened((current) => current.filter((s) => s.id !== id));
    setRequests(({ [id]: _, ...rest }) => rest);
    setCurrentId(null);
  }

  function exit(force = false) {
    if (!force && busy(null)) {
      setDialog("exit");
      return;
    }
    getCurrentWindow()
      .close()
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
          confirm="Close and stop them"
          onConfirm={() => closeCurrent(true)}
          onClose={() => setDialog(null)}
        >
          <p>
            Tasks are still running or paused on it. Closing stops them; a copy or search that keeps a map or saved file
            can carry on later from that.
          </p>
        </ConfirmDialog>
      )}
      {dialog === "exit" && (
        <ConfirmDialog
          title="Exit DataReco?"
          confirm="Exit and stop them"
          onConfirm={() => exit(true)}
          onClose={() => setDialog(null)}
        >
          <p>
            Tasks are still running or paused. Exiting stops them; a copy or search that keeps a map or saved file can
            carry on later from that.
          </p>
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
