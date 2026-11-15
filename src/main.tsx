import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { ArrowDownToLine, CalendarClock, Check, CircleAlert, FolderOpen, Globe, HardDrive, List, Pause, Play, Plus, RefreshCw, Settings2, X } from "lucide-react";
import { useEffect, useRef, useState, type FormEvent, type ReactNode } from "react";
import { createRoot } from "react-dom/client";
import "./styles.css";

type Settings = {
  workingFolder: string;
  destinationFolder: string;
  speedLimitKib: number | null;
  downloadSlots: number;
  keepRunning: boolean;
  notifyWhenFinished: boolean;
};
type Priority = "low" | "normal" | "high";
type DailyWindow = { startMinute: number; endMinute: number };
type TransferProgress = { totalBytes: number | null; completedBytes: number; bytesPerSecond: number };
type Status = "queued" | "downloading" | "pausing" | "paused" | "verifying" | "ready" | "complete" | "failed";
type Job = {
  id: string;
  url: string;
  filename: string;
  status: Status;
  priority: Priority;
  expectedSha256: string | null;
  sha256: string | null;
  error: string | null;
  bytes: number;
  transfer?: TransferProgress;
};
type Snapshot = { settings: Settings; jobs: Job[]; run_after: number | null; daily_window: DailyWindow | null };
type Filter = "all" | "complete" | "failed";
type Panel = "add" | "settings" | "browser" | "schedule";

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
  if (bytes < 1024) return `${Math.round(bytes)} B`;
  const units = ["KiB", "MiB", "GiB", "TiB"];
  const unit = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length);
  return `${(bytes / 1024 ** unit).toFixed(1)} ${units[unit - 1]}`;
}

function remainingTime(seconds: number) {
  const rounded = Math.ceil(seconds);
  if (rounded < 60) return `${rounded}s`;
  if (rounded < 3600) return `${Math.floor(rounded / 60)}m ${rounded % 60}s`;
  const hours = Math.floor(rounded / 3600);
  if (hours < 24) return `${hours}h ${Math.floor((rounded % 3600) / 60)}m`;
  return `${Math.floor(hours / 24)}d ${hours % 24}h`;
}

function DownloadProgress({ transfer }: { transfer: TransferProgress }) {
  const { totalBytes, completedBytes, bytesPerSecond } = transfer;
  const known = totalBytes !== null && totalBytes > 0;
  const remaining = known ? Math.max(0, totalBytes - completedBytes) : 0;
  const percent = known ? Math.min(100, Math.floor(completedBytes / totalBytes * 100)) : null;
  const estimates = [
    percent !== null ? `${percent}%` : null,
    bytesPerSecond > 0 ? `${displaySize(bytesPerSecond)}/s` : null,
    remaining > 0 && bytesPerSecond > 0 ? `${remainingTime(remaining / bytesPerSecond)} left` : null,
  ].filter(Boolean);
  return (
    <div className="download-progress">
      {known && <progress aria-label="Download progress" value={completedBytes} max={totalBytes} />}
      {estimates.length > 0 && <small>{estimates.join(" · ")}</small>}
    </div>
  );
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
  const [limit, setLimit] = useState(settings.speedLimitKib?.toString() ?? "");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  async function choose(key: "workingFolder" | "destinationFolder") {
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
      await invoke("save_settings", { settings: { ...paths, speedLimitKib: limit ? Number(limit) : null } });
      await onSave();
      onClose();
    } catch (failure) {
      setError(describe(failure));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Modal title="Download settings" onClose={onClose}>
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
        <div className="field">
          <label htmlFor="speed-limit">Speed limit per download <span>(KiB/s)</span></label>
          <input id="speed-limit" type="number" min="1" max="1048576" step="1" inputMode="numeric"
            placeholder="Unlimited" value={limit} onChange={event => setLimit(event.target.value)} aria-describedby="speed-limit-help" />
          <small id="speed-limit-help">Applies when a transfer starts. Leave blank for unlimited speed.</small>
        </div>
        <label>Simultaneous downloads
          <input type="number" min="1" max="8" step="1" required value={paths.downloadSlots}
            onChange={event => setPaths({ ...paths, downloadSlots: event.target.valueAsNumber })} />
        </label>
        <label className="checkbox"><input type="checkbox" checked={paths.keepRunning}
          onChange={event => setPaths({ ...paths, keepRunning: event.target.checked })} /> Keep running in the system tray when closed</label>
        <label className="checkbox"><input type="checkbox" checked={paths.notifyWhenFinished}
          onChange={event => setPaths({ ...paths, notifyWhenFinished: event.target.checked })} /> Notify me when a download completes</label>
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
        <p className="muted">In Chrome, Edge or Chromium, use Load unpacked in Developer mode. In Firefox, load the Firefox package as a temporary add-on from about:debugging.</p>
        <div className="browser-actions">
          {([["chrome", "Chrome"], ["edge", "Edge"], ["chromium", "Chromium"], ["firefox", "Firefox"]] as const).map(([browser, label]) => (
            <button key={browser} type="button" className="button secondary" disabled={busy} onClick={() => void connect(browser, label)}>{label}</button>
          ))}
        </div>
        {message && <p role="status">{message}</p>}
        {error && <p className="error" role="alert">{error}</p>}
      </div>
    </Modal>
  );
}

function toLocalTime(epoch: number) {
  const time = new Date(epoch);
  return new Date(epoch - time.getTimezoneOffset() * 60_000).toISOString().slice(0, 16);
}

function formatTime(minute: number) {
  return `${String(Math.floor(minute / 60)).padStart(2, "0")}:${String(minute % 60).padStart(2, "0")}`;
}

function toMinutes(clock: string) {
  const [hours, minutes] = clock.split(":").map(Number);
  return hours * 60 + minutes;
}

function ScheduleForm({ runAfter, dailyWindow, onClose, onSave }: {
  runAfter: number | null; dailyWindow: DailyWindow | null; onClose: () => void; onSave: () => Promise<void>;
}) {
  const [date, setDate] = useState(runAfter && runAfter > Date.now() ? toLocalTime(runAfter) : "");
  const [start, setStart] = useState(dailyWindow ? formatTime(dailyWindow.startMinute) : "");
  const [end, setEnd] = useState(dailyWindow ? formatTime(dailyWindow.endMinute) : "");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  async function save(clear = false) {
    setBusy(true);
    setError("");
    try {
      const epoch = !clear && date ? new Date(date).getTime() : null;
      if (epoch !== null && (!Number.isFinite(epoch) || epoch <= Date.now())) throw new Error("Choose a future start time.");
      if (!clear && Boolean(start) !== Boolean(end)) throw new Error("Choose both daily times.");
      const window = !clear && start && end ? { startMinute: toMinutes(start), endMinute: toMinutes(end) } : null;
      await invoke("schedule_queue", { runAfter: epoch, dailyWindow: window });
      await onSave();
      onClose();
    } catch (failure) {
      setError(describe(failure));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Modal title="Queue schedule" onClose={onClose}>
      <form onSubmit={event => { event.preventDefault(); void save(); }}>
        <p className="muted">Set when new queued transfers can start. Keep Outernet running to start them on schedule.</p>
        <label>Hold until <span>Optional</span><input type="datetime-local" data-initial-focus value={date} onChange={event => setDate(event.target.value)} /></label>
        <div className="schedule-times">
          <label>Daily start<input type="time" value={start} onChange={event => setStart(event.target.value)} /></label>
          <label>Daily end<input type="time" value={end} onChange={event => setEnd(event.target.value)} /></label>
        </div>
        <small>Leave both daily times blank for all-day downloads.</small>
        <button type="button" className="button secondary" disabled={busy} onClick={() => void save(true)}>Clear schedule</button>
        <FormActions busy={busy} error={error} onClose={onClose} label="Save schedule" />
      </form>
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

  async function setPriority(id: string, priority: Priority) {
    setActionError("");
    try {
      await invoke("set_priority", { id, priority });
      await refresh();
    } catch (failure) {
      setActionError(describe(failure));
      await refresh();
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
        <button className="nav-button" disabled={!state} onClick={() => setPanel("schedule")}><CalendarClock size={17} /> Queue schedule</button>
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
        {state?.run_after && state.run_after > Date.now() && (
          <button className="button secondary schedule-notice" onClick={() => setPanel("schedule")}>
            <CalendarClock size={16} /> Queue starts at {new Date(state.run_after).toLocaleString()}
          </button>
        )}
        {state?.daily_window && (
          <button className="button secondary schedule-notice" onClick={() => setPanel("schedule")}>
            <CalendarClock size={16} /> Daily queue window {formatTime(state.daily_window.startMinute)}–{formatTime(state.daily_window.endMinute)}
          </button>
        )}
        <section className="download-list" aria-label={heading}>
          <table>
            <thead><tr><th>File</th><th>Status</th><th>Size</th><th><span className="sr-only">Actions</span></th></tr></thead>
            <tbody>
              {visible.map(job => (
                <tr key={job.id}>
                  <td>
                    <strong className="filename" title={job.filename}>{job.filename}</strong>
                    <small>{hostname(job.url)}</small>
                    {(job.status === "queued" || job.status === "paused" || job.status === "failed") && (
                      <label className="priority-control">Priority
                        <select value={job.priority} aria-label={`Priority for ${job.filename}`}
                          onChange={event => void setPriority(job.id, event.target.value as Priority)}>
                          <option value="low">Low</option>
                          <option value="normal">Normal</option>
                          <option value="high">High</option>
                        </select>
                      </label>
                    )}
                    {job.transfer && <DownloadProgress transfer={job.transfer} />}
                    {job.error && <small className="error">{job.error}</small>}
                  </td>
                  <td><span className={`badge ${job.status}`}>{statusLabel[job.status]}</span>
                    {job.status === "complete" && job.expectedSha256 && <small>SHA-256 verified</small>}</td>
                  <td className="file-size">
                    {job.bytes > 0 || job.transfer || job.status === "complete" ? displaySize(job.bytes) : "—"}
                    {job.transfer?.totalBytes != null && <small>of {displaySize(job.transfer.totalBytes)}</small>}
                  </td>
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
      {panel === "schedule" && state && <ScheduleForm runAfter={state.run_after} dailyWindow={state.daily_window} onClose={closePanel} onSave={refresh} />}
      {panel === "browser" && <BrowserSettings onClose={closePanel} />}
      {panel === "add" && <AddDownload onClose={closePanel} onSave={refresh} />}
      {panel === "settings" && state && <SettingsForm settings={state.settings} onClose={closePanel} onSave={refresh} />}
    </div>
  );
}

createRoot(document.getElementById("root")!).render(<App />);
