import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import "./App.css";

/** Mirrors `AppInfo` in src-tauri/src/main.rs. */
type AppInfo = {
  name: string;
  version: string;
  os: string;
  arch: string;
};

type EngineState =
  | { kind: "connecting" }
  | { kind: "ready"; info: AppInfo }
  | { kind: "error"; message: string };

function App() {
  const [engine, setEngine] = useState<EngineState>({ kind: "connecting" });

  useEffect(() => {
    invoke<AppInfo>("app_info")
      .then((info) => setEngine({ kind: "ready", info }))
      .catch((err) => setEngine({ kind: "error", message: String(err) }));
  }, []);

  return (
    <div className="shell">
      <aside className="sidebar">
        <h2 className="sidebar-title">Devices</h2>
        <p className="sidebar-empty">Disk access isn't built yet.</p>
      </aside>

      <main className="workspace">
        <div className="welcome">
          <h1>DataReco</h1>
          <p>Disk editor and data recovery</p>
          <span className="badge" title="Nothing is ever written to your disks in this mode">
            Read-only mode
          </span>
        </div>
      </main>

      <footer className="statusbar">
        <StatusText engine={engine} />
      </footer>
    </div>
  );
}

function StatusText({ engine }: { engine: EngineState }) {
  switch (engine.kind) {
    case "connecting":
      return <span>Connecting to engine…</span>;
    case "error":
      return <span className="status-error">Engine error: {engine.message}</span>;
    case "ready": {
      const { name, version, os, arch } = engine.info;
      return (
        <span>
          {name} {version} · {os}/{arch}
        </span>
      );
    }
  }
}

export default App;
