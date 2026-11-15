import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { ArrowDownToLine, Check, CircleAlert, FolderOpen, Globe, HardDrive, List, Pause, Play, Plus, RefreshCw, Settings2, X } from "lucide-react";
import { useEffect, useRef, useState, type FormEvent, type ReactNode } from "react";
import { createRoot } from "react-dom/client";
import "./styles.css";

type Settings = { workingFolder: string; destinationFolder: string };
type Status = "queued" | "downloading" | "pausing" | "paused" | "verifying" | "ready" | "complete" | "failed";
type Job = {
  id: string;
  url: string;
  filename: string;
  status: Status;
  expectedSha256: string | null;
  sha256: string | null;
  error: string | null;
  bytes: number;
};
type Snapshot = { settings: Settings; jobs: Job[] };
type Filter = "all" | "complete" | "failed";
type Panel = "add" | "settings" | "browser";

const statusLabel: Record<Status, string> = {
  queued: "Queued",
  downloading: "Downloading",
  pausing: "Pausing",
  paused: "Paused",
  verifying: "Verifying",
  ready: "Saving to destination",
  complete: "Complete",
  failed: "Failed",
};

const filters = [
  { key: "all", label: "All downloads", icon: List },
  { key: "complete", label: "Completed", icon: Check },
  { key: "failed", label: "Failed", icon: CircleAlert },
] as const;

function describe(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}

function displaySize(bytes: number) {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  const unit = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length);
  return `${(bytes / 1024 ** unit).toFixed(1)} ${units[unit - 1]}`;
}

function suggestedName(input: string) {
  try {
    const part = new URL(input).pathname.split("/").pop();
    return part ? decodeURIComponent(part) : "download";
  } catch {
    return "";
  }
}

function Modal({ title, onClose, children }: { title: string; onClose: () => void; children: ReactNode }) {
  const dialog = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    dialog.current?.showModal();
    dialog.current?.querySelector<HTMLInputElement>("[data-initial-focus]")?.focus();
  }, []);

  return (
    <dialog ref={dialog} aria-labelledby="dialog-title" onClose={onClose}>
      <div className="dialog-heading">
        <h2 id="dialog-title">{title}</h2>
        <button className="icon-button" aria-label="Close" onClick={() => dialog.current?.close()}><X size={18} /></button>
      </div>
      {children}
    </dialog>
  );
}

function FormActions({ busy, error, onClose, label }: { busy: boolean; error: string; onClose: () => void; label: string }) {
  return (
    <>
      {error && <p className="error" role="alert">{error}</p>}
      <div className="form-actions">
        <button type="button" className="button secondary" onClick={onClose}>Cancel</button>
        <button type="submit" className="button primary" disabled={busy}>{busy ? "Saving…" : label}</button>
      </div>
    </>
  );
}

function AddDownload({ onClose, onSave }: { onClose: () => void; onSave: () => Promise<void> }) {
  const [url, setUrl] = useState("");
  const [filename, setFilename] = useState<string | null>(null);
  const [sha256, setSha256] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  async function submit(event: FormEvent) {
    event.preventDefault();
    setBusy(true);
    setError("");
    try {
      await invoke("add_download", { url, filename: filename ?? suggestedName(url), sha256 });
      await onSave();
      onClose();
    } catch (failure) {
      setError(describe(failure));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Modal title="New download" onClose={onClose}>
      <form onSubmit={submit}>
        <label>Download URL<input data-initial-focus type="url" placeholder="https://example.com/file.zip" value={url} onChange={event => setUrl(event.target.value)} required /></label>
        <label>Filename<input value={filename ?? suggestedName(url)} onChange={event => setFilename(event.target.value)} required /></label>
        <label>Expected SHA-256 <span>Optional</span><input spellCheck={false} autoComplete="off" value={sha256} onChange={event => setSha256(event.target.value)} placeholder="Checksum provided by the publisher" /></label>
        <FormActions busy={busy} error={error} onClose={onClose} label="Add download" />
      </form>
    </Modal>
  );
}

function SettingsForm({ settings, onClose, onSave }: { settings: Settings; onClose: () => void; onSave: () => Promise<void> }) {
  const [paths, setPaths] = useState(settings);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  async function choose(key: keyof Settings) {
    try {
      const folder = await open({ directory: true, defaultPath: paths[key] || undefined });
      if (folder) setPaths(previous => ({ ...previous, [key]: folder }));
    } catch (failure) {
      setError(describe(failure));
    }
  }

  async function submit(event: FormEvent) {
    event.preventDefault();
    setBusy(true);
    setError("");
    try {
      await invoke("save_settings", { settings: paths });
      await onSave();
      onClose();
    } catch (failure) {
      setError(describe(failure));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Modal title="Storage settings" onClose={onClose}>
      <form onSubmit={submit}>
        <p className="muted">Folder changes apply to newly added downloads.</p>
        {([["workingFolder", "In-progress folder"], ["destinationFolder", "Completed downloads folder"]] as const).map(([key, label]) => (
          <div className="field" key={key}>
            <label htmlFor={key}>{label}</label>
            <div className="folder-input">
              <input id={key} value={paths[key]} onChange={event => setPaths({ ...paths, [key]: event.target.value })} required />
              <button type="button" className="button secondary" onClick={() => void choose(key)} aria-label={`Browse for ${label.toLowerCase()}`}><FolderOpen size={16} /> Browse</button>
            </div>
          </div>
        ))}
        <FormActions busy={busy} error={error} onClose={onClose} label="Save settings" />
      </form>
    </Modal>
  );
}

function BrowserSettings({ onClose }: { onClose: () => void }) {
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState("");
  const [error, setError] = useState("");

  async function connect(browser: string, label: string) {
    setBusy(true);
    setError("");
    setMessage("");
    try {
      await invoke("connect_browser", { browser });
      setMessage(`${label} is registered. Open the extension to check the connection.`);
    } catch (failure) {
      setError(describe(failure));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Modal title="Browser integration" onClose={onClose}>
      <div className="browser-help">
        <p>Load the Outernet browser extension, then connect your browser below.</p>
        <p className="muted">In the browser’s Extensions page, enable Developer mode and choose Load unpacked. Select the extracted Outernet extension folder.</p>
        <div className="browser-actions">
          {([["chrome", "Chrome"], ["edge", "Edge"], ["chromium", "Chromium"]] as const).map(([browser, label]) => (
            <button key={browser} type="button" className="button secondary" disabled={busy} onClick={() => void connect(browser, label)}>{label}</button>
          ))}
        </div>
        {message && <p role="status">{message}</p>}
        {error && <p className="error" role="alert">{error}</p>}
      </div>
    </Modal>
  );
}

function App() {
  const [state, setState] = useState<Snapshot>();
  const [filter, setFilter] = useState<Filter>("all");
  const [panel, setPanel] = useState<Panel | null>(null);
  const [loadError, setLoadError] = useState("");
  const [actionError, setActionError] = useState("");
  const latestRequest = useRef(0);

  async function refresh() {
    const revision = ++latestRequest.current;
    try {
      const updated = await invoke<Snapshot>("snapshot");
      if (revision === latestRequest.current) {
        setState(updated);
        setLoadError("");
      }
    } catch (failure) {
      if (revision === latestRequest.current) setLoadError(describe(failure));
    }
  }

  useEffect(() => {
    void refresh();
    const timer = window.setInterval(() => void refresh(), 700);
    return () => window.clearInterval(timer);
  }, []);

  async function jobAction(command: "retry_download" | "pause_download", id: string) {
    setActionError("");
    try {
      await invoke(command, { id });
      await refresh();
    } catch (failure) {
      setActionError(describe(failure));
    }
  }

  const jobs = state?.jobs ?? [];
  const visible = jobs.filter(job => filter === "all" || job.status === filter);
  const heading = filters.find(item => item.key === filter)?.label;
  const hostname = (url: string) => new URL(url).hostname;
  const closePanel = () => setPanel(null);

  return (
    <div className="layout">
      <aside className="sidebar">
        <div className="brand"><span className="brand-icon"><ArrowDownToLine size={21} /></span> Outernet</div>
        <nav aria-label="Download filters">
          {filters.map(({ icon: Icon, label, key }) => (
            <button key={key} className={filter === key ? "nav-button selected" : "nav-button"} aria-pressed={filter === key} onClick={() => setFilter(key)}>
              <Icon size={17} /> {label}
              <span className="count">{key === "all" ? jobs.length : jobs.filter(job => job.status === key).length}</span>
            </button>
          ))}
        </nav>
        <div className="sidebar-tools">
        <button className="nav-button" onClick={() => setPanel("browser")}><Globe size={17} /> Browser integration</button>
        <button className="nav-button" disabled={!state} onClick={() => setPanel("settings")}><Settings2 size={17} /> Settings</button>
        </div>
      </aside>
      <main>
        <header>
          <h1>{heading}</h1>
          <button className="button primary" disabled={!state} onClick={() => setPanel("add")}><Plus size={17} /> Add download</button>
        </header>
        {loadError && <p className="error banner" role="alert">{loadError}</p>}
        {actionError && <p className="error banner" role="alert">{actionError}</p>}
        <section className="download-list" aria-label={heading}>
          <table>
            <thead><tr><th>File</th><th>Status</th><th>Size</th><th><span className="sr-only">Actions</span></th></tr></thead>
            <tbody>
              {visible.map(job => (
                <tr key={job.id}>
                  <td><strong className="filename" title={job.filename}>{job.filename}</strong><small>{hostname(job.url)}</small>{job.error && <small className="error">{job.error}</small>}</td>
                  <td><span className={`badge ${job.status}`}>{statusLabel[job.status]}</span>
                    {job.status === "complete" && job.expectedSha256 && <small>SHA-256 verified</small>}</td>
                  <td className="file-size">{job.bytes > 0 || job.status === "complete" ? displaySize(job.bytes) : "—"}</td>
                  <td>
                    {(job.status === "queued" || job.status === "downloading") && (
                      <button className="icon-button" title="Pause" aria-label={`Pause ${job.filename}`} onClick={() => void jobAction("pause_download", job.id)}><Pause size={16} /></button>
                    )}
                    {(job.status === "paused" || job.status === "failed") && (
                      <button className="icon-button" title={job.status === "paused" ? "Resume" : "Retry"}
                        aria-label={`${job.status === "paused" ? "Resume" : "Retry"} ${job.filename}`}
                        onClick={() => void jobAction("retry_download", job.id)}>
                        {job.status === "paused" ? <Play size={16} /> : <RefreshCw size={16} />}
                      </button>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
          {!visible.length && (
            <div className="empty">
              <span className="empty-icon"><ArrowDownToLine size={26} /></span>
              <h2>{state ? `No ${filter === "all" ? "" : `${filter === "complete" ? "completed" : "failed"} `}downloads` : loadError ? "Downloads unavailable" : "Loading downloads…"}</h2>
              {state && filter === "all" && <p>Use Add download to start a transfer from a link.</p>}
            </div>
          )}
        </section>
        {state && <footer><HardDrive size={15} /><button title={state.settings.workingFolder} onClick={() => setPanel("settings")}>In progress · {state.settings.workingFolder}</button></footer>}
      </main>
      {panel === "browser" && <BrowserSettings onClose={closePanel} />}
      {panel === "add" && <AddDownload onClose={closePanel} onSave={refresh} />}
      {panel === "settings" && state && <SettingsForm settings={state.settings} onClose={closePanel} onSave={refresh} />}
    </div>
  );
}

createRoot(document.getElementById("root")!).render(<App />);
