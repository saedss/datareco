import { useCallback, useEffect, useRef, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { api, AppInfo, DeviceList, TemplateList, toCommandError } from "./api";
import { DeviceSidebar, Selection, findSelection, imageSelection } from "./components/DeviceSidebar";
import { DiskEditor } from "./components/DiskEditor";
import "./App.css";

const IMAGE_FILTERS = [
  { name: "Disk images", extensions: ["img", "dd", "raw", "bin", "ima", "iso"] },
  { name: "All files", extensions: ["*"] },
];

function App() {
  const [info, setInfo] = useState<AppInfo | null>(null);
  const [engineError, setEngineError] = useState<string | null>(null);
  const [devices, setDevices] = useState<DeviceList | null>(null);
  const [devicesError, setDevicesError] = useState<string | null>(null);
  const [images, setImages] = useState<string[]>([]);
  const [selection, setSelection] = useState<Selection | null>(null);
  const [restartError, setRestartError] = useState<string | null>(null);
  const [templates, setTemplates] = useState<TemplateList | null>(null);

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

  // Show the source passed with --open: images right away, disks and volumes
  // once the device list has loaded (for their names).
  const startSource = info?.startSource ?? null;
  const startShown = useRef(false);
  useEffect(() => {
    if (!startSource || startShown.current) return;
    if (startSource.startsWith("image:")) {
      const path = startSource.slice("image:".length);
      setImages((current) => (current.includes(path) ? current : [...current, path]));
      setSelection(imageSelection(path));
      startShown.current = true;
    } else if (devices) {
      setSelection(findSelection(startSource, devices) ?? { id: startSource, title: startSource, subtitle: "" });
      startShown.current = true;
    }
  }, [startSource, devices]);

  async function openImage() {
    const path = await open({ title: "Open disk image", multiple: false, directory: false, filters: IMAGE_FILTERS });
    if (typeof path !== "string") return;
    showImage(path);
  }

  function showImage(path: string) {
    setImages((current) => (current.includes(path) ? current : [...current, path]));
    setSelection(imageSelection(path));
  }

  function restartAsAdmin() {
    setRestartError(null);
    api.restartAsAdmin(selection?.id ?? null).catch((e) => setRestartError(toCommandError(e).message));
  }

  return (
    <div className="shell">
      <DeviceSidebar
        devices={devices}
        devicesError={devicesError}
        images={images}
        selectedId={selection?.id ?? null}
        elevated={info?.elevated ?? false}
        onSelect={setSelection}
        onOpenImage={openImage}
        onRefresh={refreshDevices}
        onRestartAsAdmin={restartAsAdmin}
      />

      <main className="workspace">
        {selection ? (
          <DiskEditor
            key={selection.id}
            sourceId={selection.id}
            title={selection.title}
            subtitle={selection.subtitle}
            templates={templates}
            onReloadTemplates={reloadTemplates}
            onRestartAsAdmin={restartAsAdmin}
            onOpenImage={showImage}
          />
        ) : (
          <div className="welcome">
            <h1>DataReco</h1>
            <p>Pick a disk, volume or image file to look inside it.</p>
            <span
              className="badge"
              title="DataReco writes to a disk only when you ask it to and type the disk's name: copying onto it, applying partition table changes, formatting or wiping."
            >
              Read-only mode
            </span>
          </div>
        )}
      </main>

      <footer className="statusbar">
        {engineError ? (
          <span className="error-text">Engine error: {engineError}</span>
        ) : info ? (
          <span>
            {info.name} {info.version} · {info.os}/{info.arch} ·{" "}
            {info.elevated ? "Administrator" : "Not administrator"}
          </span>
        ) : (
          <span>Connecting to engine…</span>
        )}
        {restartError && <span className="error-text"> · Couldn't restart as administrator: {restartError}</span>}
      </footer>
    </div>
  );
}

export default App;
