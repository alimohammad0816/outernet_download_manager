import { getCurrentWindow } from "@tauri-apps/api/window";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { ArrowDown, ArrowDownToLine, ArrowUp, CalendarClock, Check, ChevronDown, ChevronLeft, ChevronRight, Clock, File, FolderOpen, Globe, ListFilter, Layers, Link, Minus, MoreHorizontal, Pause, Play, Plus, RefreshCw, Settings2, Square, X } from "lucide-react";
import { useCallback, useEffect, useId, useLayoutEffect, useRef, useState, type FormEvent, type ReactNode } from "react";
import { createRoot } from "react-dom/client";
import "./styles.css";

type ProxyMode = "system" | "direct" | "custom";
type ProxySettings = { mode: "system" | "direct" } | { mode: "custom"; url: string };
type Settings = {
  proxy: ProxySettings;
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
  queueId: string;
  addedAt: number | null;
  format?: "file" | "hls" | "hls_live" | "dash" | "youtube";
  status: Status;
  priority: Priority;
  settings: Settings;
  expectedSha256: string | null;
  sha256: string | null;
  error: string | null;
  bytes: number;
  transfer?: TransferProgress;
};
type DownloadQueue = { id: string; name: string; slots: number; running: boolean; interface: string | null };
type NetworkInterface = { name: string; address: string };
type Snapshot = { settings: Settings; jobs: Job[]; queues: DownloadQueue[]; run_after: number | null; daily_window: DailyWindow | null };
type SortKey = "date" | "name" | "size" | "status" | "priority";
type Filter = "all" | "complete" | "failed";
const categories = {
  Video: /\.(mp4|m4v|mkv|mov|avi|webm|ts|m3u8|mpd)$/i,
  Audio: /\.(mp3|flac|wav|ogg|opus|m4a|aac)$/i,
  Images: /\.(png|jpg|jpeg|svg|webp|gif|avif|heic)$/i,
  Documents: /\.(pdf|txt|docx?|xlsx?|pptx?|csv|odt|epub)$/i,
  Archives: /\.(zip|rar|7z|tar|gz|bz2|xz|zst)$/i,
  Software: /\.(exe|msi|deb|rpm|appimage|apk|iso|dmg|pkg)$/i,
};

function fileCategory(filename: string) {
  return Object.entries(categories).find(([, extension]) => extension.test(filename))?.[0] ?? "Other";
}

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
  { key: "all", label: "All downloads" },
  { key: "complete", label: "Completed" },
  { key: "failed", label: "Failed" },
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

function Modal({ title, onClose, children, className, description, mark }: {
  title: string; onClose: () => void; children: ReactNode; className?: string; description?: string; mark?: ReactNode;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    dialog.current?.showModal();
    const initial = dialog.current?.querySelector<HTMLElement>("[data-initial-focus]")
      ?? dialog.current?.querySelector<HTMLElement>("#dialog-title");
    initial?.focus({ preventScroll: true });
  }, []);

  return (
    <dialog ref={dialog} className={className} aria-labelledby="dialog-title"
      aria-describedby={description ? "dialog-description" : undefined} onClose={onClose}>
      <div className="dialog-heading">
        <div className="dialog-title-group">
          {description && <span className="dialog-mark">{mark ?? <ArrowDownToLine size={20} />}</span>}
          <div><h2 id="dialog-title" tabIndex={-1}>{title}</h2>
            {description && <p id="dialog-description">{description}</p>}
          </div>
        </div>
        <button className="icon-button" aria-label="Close" onClick={() => dialog.current?.close()}><X size={18} /></button>
      </div>
      {children}
    </dialog>
  );
}

function FormActions({ busy, onClose, label }: { busy: boolean; onClose: () => void; label: string }) {
  return <div className="workspace-footer">
    <button type="button" className="button secondary" onClick={onClose}>Cancel</button>
    <button type="submit" className="button queue-submit" disabled={busy}><Check size={16} />{busy ? "Saving…" : label}</button>
  </div>;
}

function AddDownload({ queues, initialQueue, onClose, onSave }: {
  queues: DownloadQueue[]; initialQueue: string; onClose: () => void; onSave: () => Promise<void>;
}) {
  const [queueId, setQueueId] = useState(initialQueue);
  const [url, setUrl] = useState("");
  const [batch, setBatch] = useState(false);
  const [links, setLinks] = useState("");
  const [filename, setFilename] = useState<string | null>(null);
  const [sha256, setSha256] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  const entries = links.split(/\r?\n/).flatMap((value, index) => {
    const url = value.trim();
    if (!url) return [];
    try {
      const parsed = new URL(url);
      const valid = ["https:", "http:"].includes(parsed.protocol) && !parsed.username && !parsed.password;
      return [{ url, line: index + 1, valid, filename: suggestedName(url) || "download" }];
    } catch {
      return [{ url, line: index + 1, valid: false, filename: "" }];
    }
  });
  const invalid = entries.find(entry => !entry.valid);
  const issue = entries.length > 500 ? "Add up to 500 links at a time."
    : invalid ? `Line ${invalid.line} needs a valid HTTP or HTTPS link.` : "";
  const linkCount = entries.length === 1 ? "1 link" : `${entries.length} links`;

  function changeMode(multiple: boolean) {
    setBatch(multiple);
    setError("");
  }

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (busy || (batch && (!entries.length || issue))) return;
    setBusy(true);
    setError("");
    try {
      if (batch) {
        await invoke("add_download_batch", { links: entries.map(entry => [entry.url, entry.filename]), queueId });
      } else {
        await invoke("add_download", { url, filename: filename ?? suggestedName(url), sha256, queueId });
      }
      await onSave();
      onClose();
    } catch (failure) {
      setError(describe(failure));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Modal title="Add download" description="Enter one or more HTTP or HTTPS links." className="workspace-dialog" onClose={onClose}>
      <form className="workspace-form" onSubmit={submit}>
        <div className="workspace-body">
          <div className="download-mode" role="group" aria-label="Download mode">
            <button type="button" aria-pressed={!batch} disabled={busy} onClick={() => changeMode(false)}><Link size={15} /> Single link</button>
            <button type="button" aria-pressed={batch} disabled={busy} onClick={() => changeMode(true)}><Layers size={15} /> Multiple links</button>
          </div>
          {batch ? (
            <div className="batch-field">
              <div className="field-heading"><label htmlFor="batch-links">Download links</label><span id="batch-hint">One link per line</span></div>
              <div className="link-editor">
                <textarea id="batch-links" rows={5} value={links}
                  onChange={event => { setLinks(event.target.value); setError(""); }}
                  placeholder={"https://example.com/report.pdf\nhttps://example.com/archive.zip"}
                  spellCheck={false} autoComplete="off" aria-invalid={!!issue} aria-describedby="batch-hint batch-feedback" />
                <div className="editor-info" id="batch-feedback" aria-live="polite">
                  <span className={issue ? "error" : undefined}>{issue || (entries.length ? `${linkCount} detected` : "Paste links from your clipboard")}</span>
                  <span>{entries.length} / 500</span>
                </div>
              </div>
              {entries.length > 0 && !issue && (
                <ul className="link-preview" aria-label="Files to queue">
                  {entries.slice(0, 2).map(entry => <li key={entry.line}><ArrowDownToLine size={14} /><span title={entry.filename}>{entry.filename}</span></li>)}
                  {entries.length > 2 && <li className="preview-more">+{entries.length - 2} more</li>}
                </ul>
              )}
            </div>
          ) : (
            <div className="single-fields">
              <label>Download link<input data-initial-focus type="url" placeholder="https://example.com/file.zip" value={url} onChange={event => setUrl(event.target.value)} required /></label>
              <label>Filename<input value={filename ?? suggestedName(url)} onChange={event => setFilename(event.target.value)} required /></label>
              <label>SHA-256 <span>Optional</span><input spellCheck={false} autoComplete="off" value={sha256} onChange={event => setSha256(event.target.value)} placeholder="Publisher checksum" /></label>
            </div>
          )}
          <div className="queue-field"><span>Download queue</span>
            <ChoicePicker value={queueId} onChange={setQueueId} label="Download queue"
              options={queues.map(queue => [queue.id, queue.name])} />
          </div>
          {error && <p className="error" role="alert">{error}</p>}
        </div>
        <div className="workspace-footer">
          <button type="button" className="button secondary" onClick={onClose}>Cancel</button>
          <button type="submit" className="button queue-submit" disabled={busy || (batch ? !entries.length || !!issue : !url.trim())}>
            <ArrowDownToLine size={16} />{busy ? "Queuing…" : batch && entries.length ? `Queue ${linkCount}` : "Add to queue"}
          </button>
        </div>
      </form>
    </Modal>
  );
}

function ReplaceLink({ job, onClose, onSave }: { job: Job; onClose: () => void; onSave: () => Promise<void> }) {
  const [url, setUrl] = useState("");
  const [sha256, setSha256] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  async function submit(event: FormEvent) {
    event.preventDefault();
    setBusy(true);
    setError("");
    try {
      await invoke("replace_download_link", { id: job.id, url, sha256: sha256 || null });
      await onSave();
      onClose();
    } catch (failure) {
      setError(describe(failure));
    } finally {
      setBusy(false);
    }
  }

  return <Modal title="Replace download link" description={job.filename} className="workspace-dialog"
    mark={<Link size={20} />} onClose={onClose}>
    <form className="workspace-form" onSubmit={submit}>
      <div className="workspace-body single-fields">
        <label>New URL<input data-initial-focus type="url" value={url} onChange={event => setUrl(event.target.value)} required /></label>
        {!job.expectedSha256 && <label>Publisher SHA-256 <span>Optional</span>
          <input value={sha256} onChange={event => setSha256(event.target.value)} spellCheck={false} autoComplete="off" />
        </label>}
        <p className="muted">A publisher checksum allows verified parts to carry over when the size and strong ETag match.</p>
        {error && <p className="error" role="alert">{error}</p>}
      </div>
      <FormActions busy={busy} onClose={onClose} label="Replace and queue" />
    </form>
  </Modal>;
}

function UpdateControls() {
  const [enabled, setEnabled] = useState(false);
  const [version, setVersion] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState("");

  useEffect(() => {
    void invoke<boolean>("updates_enabled").then(setEnabled).catch(() => setEnabled(false));
  }, []);

  async function check() {
    setBusy(true);
    setVersion(null);
    setMessage("");
    try {
      const available = await invoke<string | null>("check_update");
      setVersion(available);
      setMessage(available ? `Version ${available} is available.` : "Outernet is up to date.");
    } catch (failure) {
      setMessage(describe(failure));
    } finally {
      setBusy(false);
    }
  }

  async function install() {
    setBusy(true);
    setMessage("Downloading and verifying the signed installer…");
    try {
      await invoke("install_update", { version });
    } catch (failure) {
      setMessage(describe(failure));
    } finally {
      setBusy(false);
    }
  }

  return enabled && (
    <div className="field">
      <button type="button" className="button secondary" onClick={() => void check()} disabled={busy}>
        Check for updates
      </button>
      {message && <small role="status">{message}</small>}
      {version && (
        <button type="button" className="button secondary" onClick={() => void install()} disabled={busy}>
          Install and restart
        </button>
      )}
    </div>
  );
}

function SettingsForm({ settings, onClose, onSave }: { settings: Settings; onClose: () => void; onSave: () => Promise<void> }) {
  const [paths, setPaths] = useState(settings);
  const [proxyMode, setProxyMode] = useState<ProxyMode>(settings.proxy.mode);
  const [proxyUrl, setProxyUrl] = useState(settings.proxy.mode === "custom" ? settings.proxy.url : "");
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
      const proxy: ProxySettings = proxyMode === "custom" ? { mode: proxyMode, url: proxyUrl.trim() } : { mode: proxyMode };
      await invoke("save_settings", { settings: { ...paths, proxy, speedLimitKib: limit ? Number(limit) : null } });
      await onSave();
      onClose();
    } catch (failure) {
      setError(describe(failure));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Modal title="Settings" description="Storage, transfers, network and desktop behavior." mark={<Settings2 size={20} />}
      className="workspace-dialog settings-dialog" onClose={onClose}>
      <form className="workspace-form" onSubmit={submit}>
        <div className="workspace-body">
          <section className="settings-section">
            <h3>Storage</h3>
            {([["workingFolder", "In-progress folder"], ["destinationFolder", "Completed downloads folder"]] as const).map(([key, label]) => (
              <div className="field" key={key}>
                <label htmlFor={key}>{label}</label>
                <div className="folder-input">
                  <input id={key} value={paths[key]} onChange={event => setPaths({ ...paths, [key]: event.target.value })} required />
                  <button type="button" className="button secondary" onClick={() => void choose(key)} aria-label={`Browse for ${label.toLowerCase()}`}><FolderOpen size={16} /> Browse</button>
                </div>
              </div>
            ))}
            <p className="muted">Folder changes apply to newly added downloads.</p>
          </section>
          <section className="settings-section">
            <h3>Transfers</h3>
            <label>Speed limit <span>KiB/s per download</span>
              <input type="number" min="1" max="1048576" step="1" inputMode="numeric" placeholder="Unlimited"
                value={limit} onChange={event => setLimit(event.target.value)} />
            </label>
          </section>
          <section className="settings-section">
            <h3>Network</h3>
            <fieldset className="proxy-field">
              <legend>Proxy</legend>
              <div className="proxy-modes">
                {([["system", "System proxy"], ["direct", "Direct"], ["custom", "Custom"]] as const).map(([mode, label]) => (
                  <label key={mode}>
                    <input type="radio" name="proxy-mode" value={mode} checked={proxyMode === mode}
                      onChange={() => setProxyMode(mode)} />
                    <span>{label}</span>
                  </label>
                ))}
              </div>
            </fieldset>
            {proxyMode === "custom" && (
              <label>Proxy URL
                <input type="url" autoComplete="off" spellCheck={false} placeholder="http://127.0.0.1:8080"
                  value={proxyUrl} onChange={event => setProxyUrl(event.target.value)} required />
              </label>
            )}
            <p className="muted">Applies to transfers started by Outernet. Browser-assisted transfers use the browser’s connection.</p>
          </section>
          <section className="settings-section">
            <h3>Desktop</h3>
            <label className="switch-row">Keep running in the system tray
              <input type="checkbox" role="switch" checked={paths.keepRunning}
                onChange={event => setPaths({ ...paths, keepRunning: event.target.checked })} />
            </label>
            <label className="switch-row">Notify me when a download completes
              <input type="checkbox" role="switch" checked={paths.notifyWhenFinished}
                onChange={event => setPaths({ ...paths, notifyWhenFinished: event.target.checked })} />
            </label>
            <UpdateControls />
          </section>
          {error && <p className="error" role="alert">{error}</p>}
        </div>
        <FormActions busy={busy} onClose={onClose} label="Save settings" />
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
    <Modal title="Browser integration" description="Register a browser to send files and video to Outernet."
      mark={<Globe size={20} />} className="workspace-dialog" onClose={onClose}>
      <div className="workspace-body">
        <div className="browser-list">
          {([["chrome", "Chrome"], ["edge", "Edge"], ["chromium", "Chromium"], ["firefox", "Firefox"]] as const).map(([browser, label]) => (
            <div className="browser-row" key={browser}>
              <span className="browser-name"><Globe size={16} />{label}</span>
              <button type="button" className="button secondary" disabled={busy} aria-label={`Connect ${label}`} onClick={() => void connect(browser, label)}>Connect</button>
            </div>
          ))}
        </div>
        {message && <p className="message" role="status">{message}</p>}
        {error && <p className="error" role="alert">{error}</p>}
        <details className="browser-help"><summary>Install the extension</summary>
          <p>In Chrome, Edge or Chromium, use <strong>Load unpacked</strong> in Developer mode. In Firefox, load the Firefox package as a temporary add-on from <code>about:debugging</code>.</p>
        </details>
      </div>
      <div className="workspace-footer"><button type="button" className="button secondary" onClick={onClose}>Done</button></div>
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

function ChoicePicker<T extends string>({ value, onChange, options, label, icon }: {
  value: T; onChange: (value: T) => void; options: ReadonlyArray<readonly [T, string]>; label: string; icon?: ReactNode;
}) {
  const [expanded, setExpanded] = useState(false);
  const root = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const id = useId();
  const selected = options.find(([key]) => key === value)?.[1];
  useEffect(() => {
    if (!expanded) return;
    function dismiss(event: PointerEvent) {
      if (event.target instanceof Node && !root.current?.contains(event.target)) setExpanded(false);
    }
    window.addEventListener("pointerdown", dismiss);
    return () => window.removeEventListener("pointerdown", dismiss);
  }, [expanded]);
  return (
    <div className="category-picker" ref={root} onBlur={event => {
      if (!event.currentTarget.contains(event.relatedTarget)) setExpanded(false);
    }} onKeyDown={event => {
      if (event.key === "Escape" && expanded) {
        setExpanded(false);
        trigger.current?.focus();
      }
    }}>
      <button className="picker-trigger" type="button" ref={trigger} aria-expanded={expanded} aria-controls={id}
        aria-label={`${label}: ${selected}`} onClick={() => setExpanded(!expanded)}>
        {icon}<span>{selected}</span><ChevronDown size={14} />
      </button>
      {expanded && <div className="picker-menu" id={id} role="group" aria-label={label}>
        {options.map(([key, text]) => (
          <button type="button" key={key} aria-pressed={key === value} onClick={() => {
            onChange(key);
            setExpanded(false);
            trigger.current?.focus();
          }}>{text}{key === value && <Check size={15} />}</button>
        ))}
      </div>}
    </div>
  );
}

const monthLabel = new Intl.DateTimeFormat("en", { month: "long", year: "numeric" });
const dateLabel = new Intl.DateTimeFormat("en", { dateStyle: "medium" });
const weekdays = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

function DatePicker({ value, onChange }: { value: string; onChange: (day: string) => void }) {
  const initial = value ? new Date(`${value}T12:00`) : new Date();
  const [month, setMonth] = useState(() => new Date(initial.getFullYear(), initial.getMonth(), 1));
  const [expanded, setExpanded] = useState(false);
  const trigger = useRef<HTMLButtonElement>(null);
  const id = useId();
  const today = toLocalTime(Date.now()).slice(0, 10);
  const start = month.getDay();
  const days = new Date(month.getFullYear(), month.getMonth() + 1, 0).getDate();

  function select(day: string) {
    onChange(day);
    setExpanded(false);
    trigger.current?.focus();
  }

  function shift(amount: number) {
    setMonth(current => new Date(current.getFullYear(), current.getMonth() + amount, 1));
  }

  return (
    <div className="date-picker" onKeyDown={event => {
      if (event.key === "Escape" && expanded) {
        event.preventDefault();
        setExpanded(false);
        trigger.current?.focus();
      }
    }}>
      <span className="date-field-label">Date</span>
      <button type="button" className="picker-trigger" data-initial-focus ref={trigger} aria-expanded={expanded} aria-controls={id}
        aria-label={value ? `Hold date: ${dateLabel.format(initial)}` : "Choose hold date"} onClick={() => setExpanded(!expanded)}>
        <CalendarClock size={16} /><span>{value ? dateLabel.format(initial) : "Choose date"}</span><ChevronDown size={14} />
      </button>
      {expanded && <div className="calendar-panel" id={id} role="group" aria-label="Choose date">
        <div className="calendar-nav">
          <button type="button" className="icon-button" onClick={() => shift(-1)} aria-label="Previous month"><ChevronLeft size={16} /></button>
          <strong aria-live="polite">{monthLabel.format(month)}</strong>
          <button type="button" className="icon-button" onClick={() => shift(1)} aria-label="Next month"><ChevronRight size={16} /></button>
        </div>
        <div className="calendar-grid">
          {weekdays.map(day => <span key={day}>{day}</span>)}
          {Array.from({ length: start }, (_, index) => <span key={`empty-${index}`} aria-hidden="true" />)}
          {Array.from({ length: days }, (_, index) => {
            const day = new Date(month.getFullYear(), month.getMonth(), index + 1, 12);
            const key = toLocalTime(day.getTime()).slice(0, 10);
            return <button key={key} type="button" disabled={key < today} aria-pressed={value === key}
              aria-current={key === today ? "date" : undefined} aria-label={dateLabel.format(day)}
              onClick={() => select(key)}>{day.getDate()}</button>;
          })}
        </div>
        <button type="button" className="calendar-today" onClick={() => select(today)}>Today</button>
      </div>}
    </div>
  );
}

function ClockField({ label, value, onChange }: { label: string; value: string; onChange: (clock: string) => void }) {
  return <label className="clock-field">{label}<span className="clock-input">
    <Clock size={15} /><input type="text" inputMode="numeric" maxLength={5} autoComplete="off" placeholder="HH:MM" value={value}
      onChange={event => {
        const input = event.target.value;
        const digits = input.replace(/\D/g, "").slice(0, 4);
        onChange(digits.length > 2 ? `${digits.slice(0, 2)}:${digits.slice(2)}` : input.endsWith(":") && digits.length === 2 ? `${digits}:` : digits);
      }} />
  </span></label>;
}

function ScheduleForm({ runAfter, dailyWindow, onClose, onSave }: {
  runAfter: number | null; dailyWindow: DailyWindow | null; onClose: () => void; onSave: () => Promise<void>;
}) {
  const initial = runAfter && runAfter > Date.now() ? toLocalTime(runAfter) : "";
  const [date, setDate] = useState(initial.slice(0, 10));
  const [time, setTime] = useState(initial.slice(11));
  const [start, setStart] = useState(dailyWindow ? formatTime(dailyWindow.startMinute) : "");
  const [end, setEnd] = useState(dailyWindow ? formatTime(dailyWindow.endMinute) : "");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  async function save(clear = false) {
    setBusy(true);
    setError("");
    try {
      const validClock = /^(?:[01][0-9]|2[0-3]):[0-5][0-9]$/;
      if (!clear && (Boolean(date) !== Boolean(time) || (time && !validClock.test(time)))) {
        throw new Error("Choose a hold date and enter a time in 24-hour format.");
      }
      const local = `${date}T${time}`;
      const epoch = !clear && date ? new Date(local).getTime() : null;
      if (epoch !== null && (!Number.isFinite(epoch) || epoch <= Date.now() || toLocalTime(epoch) !== local)) {
        throw new Error("Choose a valid future local time.");
      }
      if (!clear && (start || end) && (!validClock.test(start) || !validClock.test(end))) {
        throw new Error("Enter both daily times in 24-hour format.");
      }
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
    <Modal title="Queue schedule" description="Choose when queued downloads can start." className="workspace-dialog"
      mark={<CalendarClock size={20} />} onClose={onClose}>
      <form className="workspace-form" onSubmit={event => { event.preventDefault(); void save(); }}>
        <div className="workspace-body schedule-body">
          <section className="schedule-card">
            <div className="schedule-card-title"><h3>Hold until</h3>
              {(date || time) && <button type="button" className="quiet-button" onClick={() => { setDate(""); setTime(""); }}>Remove hold</button>}
            </div>
            <div className="schedule-hold">
              <DatePicker value={date} onChange={setDate} />
              <ClockField label="Time" value={time} onChange={setTime} />
            </div>
          </section>
          <section className="schedule-card">
            <div className="schedule-card-title"><h3>Daily download window</h3></div>
            <div className="schedule-times">
              <ClockField label="Start" value={start} onChange={setStart} />
              <ClockField label="End" value={end} onChange={setEnd} />
            </div>
            <p className="schedule-hint">Leave both times blank to allow downloads all day. Times use your device’s time zone.</p>
          </section>
          {error && <p className="error" role="alert">{error}</p>}
        </div>
        <div className="workspace-footer">
          {(runAfter || dailyWindow) && <button type="button" className="quiet-button clear-schedule" disabled={busy}
            onClick={() => void save(true)}>Clear schedule</button>}
          <button type="button" className="button secondary" onClick={onClose}>Cancel</button>
          <button type="submit" className="button queue-submit" disabled={busy}><Check size={16} />{busy ? "Saving…" : "Save schedule"}</button>
        </div>
      </form>
    </Modal>
  );
}

function QueueForm({ queue, onClose, onSave }: {
  queue: DownloadQueue | null; onClose: () => void; onSave: (id: string) => Promise<void>;
}) {
  const [name, setName] = useState(queue?.name ?? "");
  const [slots, setSlots] = useState(queue?.slots ?? 1);
  const [device, setDevice] = useState(queue?.interface ?? "");
  const [devices, setDevices] = useState<NetworkInterface[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  useEffect(() => {
    void invoke<NetworkInterface[]>("list_network_interfaces").then(setDevices).catch(failure => setError(describe(failure)));
  }, []);

  async function save(event: FormEvent) {
    event.preventDefault();
    setBusy(true);
    setError("");
    try {
      const id = await invoke<string>("save_download_queue", { id: queue?.id ?? null, name, slots, interface: device || null });
      await onSave(id);
      onClose();
    } catch (failure) {
      setError(describe(failure));
    } finally {
      setBusy(false);
    }
  }

  const options: Array<[string, string]> = [["", "System default"]];
  for (const item of devices) options.push([item.name, `${item.name} · ${item.address}`]);
  if (device && !devices.some(item => item.name === device)) options.push([device, `${device} · unavailable`]);
  return <Modal title={queue ? "Queue settings" : "New download queue"} description="Set the download limit and network interface for this queue."
    mark={<Layers size={20} />} className="workspace-dialog" onClose={onClose}>
    <form className="workspace-form" onSubmit={save}>
      <div className="workspace-body single-fields">
        <label>Queue name<input data-initial-focus value={name} maxLength={60} onChange={event => setName(event.target.value)} required /></label>
        <label>Simultaneous downloads<input type="number" value={slots} min={1} max={8} required
          onChange={event => setSlots(event.target.valueAsNumber)} /></label>
        <div className="queue-field"><span>Network interface</span>
          <ChoicePicker value={device} onChange={setDevice} label="Network interface" options={options} />
        </div>
        {error && <p className="error" role="alert">{error}</p>}
      </div>
      <FormActions busy={busy} onClose={onClose} label={queue ? "Save queue" : "Create queue"} />
    </form>
  </Modal>;
}

function MoveDownload({ job, queues, onClose, onSave }: {
  job: Job; queues: DownloadQueue[]; onClose: () => void; onSave: () => Promise<void>;
}) {
  const [target, setTarget] = useState(queues.find(queue => queue.id !== job.queueId)?.id ?? "");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  async function save(event: FormEvent) {
    event.preventDefault();
    setBusy(true);
    try {
      await invoke("move_download_to_queue", { id: job.id, queueId: target });
      await onSave();
      onClose();
    } catch (failure) {
      setError(describe(failure));
    } finally {
      setBusy(false);
    }
  }
  return <Modal title="Move to queue" description={job.filename} className="workspace-dialog" mark={<Layers size={20} />} onClose={onClose}>
    <form className="workspace-form" onSubmit={save}>
      <div className="workspace-body">
        <ChoicePicker label="Destination queue" value={target} onChange={setTarget}
          options={queues.filter(queue => queue.id !== job.queueId).map(queue => [queue.id, queue.name])} />
        {error && <p className="error" role="alert">{error}</p>}
      </div>
      <FormActions busy={busy} onClose={onClose} label="Move download" />
    </form>
  </Modal>;
}

const resizeDirections = ["North", "South", "East", "West", "NorthEast", "NorthWest", "SouthEast", "SouthWest"] as const;

function WindowControls({ onError }: { onError: (error: string) => void }) {
  const appWindow = getCurrentWindow();
  function operate(action: () => Promise<void>) {
    void action().catch(error => onError(describe(error)));
  }
  return <>
    <div className="window-controls">
      <button className="icon-button" title="Minimize" aria-label="Minimize" onClick={() => operate(() => appWindow.minimize())}><Minus size={15} /></button>
      <button className="icon-button window-close" title="Close" aria-label="Close window" onClick={() => operate(() => appWindow.close())}><X size={15} /></button>
    </div>
    {resizeDirections.map(direction => <span key={direction} aria-hidden="true" className={`resize-grip resize-${direction.toLowerCase()}`}
      onPointerDown={event => { if (event.button === 0) operate(() => appWindow.startResizeDragging(direction)); }} />)}
  </>;
}

type DownloadAction = "open" | "reveal" | "properties" | "forget" | "move" | "pause" | "retry" | "replace" | "stop";
type MenuAnchor = { id: string; x: number; y: number; previous: HTMLElement };

function DownloadMenu({ job, anchor, canMove, onClose, onAction }: {
  job: Job; canMove: boolean; anchor: MenuAnchor; onClose: () => void; onAction: (action: DownloadAction) => void;
}) {
  const menu = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    const element = menu.current;
    if (!element) return;
    element.style.left = `${Math.max(8, Math.min(anchor.x, innerWidth - element.offsetWidth - 8))}px`;
    element.style.top = `${Math.max(8, Math.min(anchor.y, innerHeight - element.offsetHeight - 8))}px`;
    element.querySelector("button")?.focus();
  }, [anchor]);
  useEffect(() => {
    function dismiss(event: PointerEvent) {
      if (event.target instanceof Node && !menu.current?.contains(event.target)) onClose();
    }
    window.addEventListener("pointerdown", dismiss);
    window.addEventListener("resize", onClose);
    window.addEventListener("blur", onClose);
    return () => {
      window.removeEventListener("pointerdown", dismiss);
      window.removeEventListener("resize", onClose);
      window.removeEventListener("blur", onClose);
    };
  }, [onClose]);

  const options: Array<[DownloadAction, string]> = [];
  if (job.status === "complete") options.push(["open", "Open file"], ["reveal", "Show in folder"]);
  if (job.status === "downloading" && job.format === "hls_live") options.push(["stop", "Stop and save recording"]);
  else if (job.status === "queued" || job.status === "downloading") options.push(["pause", "Pause"]);
  if (job.status === "paused" || job.status === "failed") {
    options.push(["retry", job.status === "paused" ? "Resume" : "Retry"]);
    if (!job.sha256) options.push(["replace", "Replace link"]);
  }
  if (canMove && ["queued", "paused", "failed", "complete"].includes(job.status)) options.push(["move", "Move to queue"]);
  options.push(["properties", "Properties"]);
  if (job.status === "complete") options.push(["forget", "Remove from history"]);

  return <div className="download-menu" role="menu" aria-label={`Actions for ${job.filename}`} ref={menu}
    onKeyDown={event => {
      if (event.key === "Escape" || event.key === "Tab") {
        if (event.key === "Escape") event.preventDefault();
        onClose();
        anchor.previous.focus();
      } else if (event.key === "ArrowDown" || event.key === "ArrowUp" || event.key === "Home" || event.key === "End") {
        event.preventDefault();
        const buttons = Array.from(menu.current?.querySelectorAll("button") ?? []);
        if (!buttons.length) return;
        const index = buttons.indexOf(document.activeElement as HTMLButtonElement);
        const next = event.key === "Home" ? 0 : event.key === "End" ? buttons.length - 1 :
          (index + (event.key === "ArrowDown" ? 1 : -1) + buttons.length) % buttons.length;
        buttons[next]?.focus();
      }
    }}>
    {options.map(([key, text]) => <button key={key} type="button" role="menuitem"
      className={key === "forget" ? "history-action" : undefined} onClick={() => onAction(key)}>{text}</button>)}
  </div>;
}

function DownloadProperties({ job, onClose }: { job: Job; onClose: () => void }) {
  return <Modal title="Download properties" description={job.filename} className="workspace-dialog" mark={<File size={20} />} onClose={onClose}>
    <div className="workspace-body">
      <dl className="download-properties">
        <div><dt>Status</dt><dd>{statusLabel[job.status]}</dd></div>
        <div><dt>Size</dt><dd>{displaySize(job.bytes)}</dd></div>
        {job.addedAt != null && <div><dt>Date added</dt><dd>{new Date(job.addedAt).toLocaleString()}</dd></div>}
        <div><dt>Destination folder</dt><dd>{job.settings.destinationFolder}</dd></div>
        <div><dt>Source URL</dt><dd>{job.url}</dd></div>
        {job.sha256 && <div><dt>SHA-256</dt><dd className="checksum">{job.sha256}</dd></div>}
        {job.error && <div><dt>Error</dt><dd>{job.error}</dd></div>}
      </dl>
    </div>
    <div className="workspace-footer"><button className="button secondary" type="button" onClick={onClose}>Done</button></div>
  </Modal>;
}

function App() {
  const [state, setState] = useState<Snapshot>();
  const [filter, setFilter] = useState<Filter>("all");
  const [category, setCategory] = useState("");
  const [replacement, setReplacement] = useState<Job | null>(null);
  const [queueId, setQueueId] = useState("default");
  const [queueForm, setQueueForm] = useState<DownloadQueue | "new" | null>(null);
  const [moving, setMoving] = useState<Job | null>(null);
  const [sort, setSort] = useState<SortKey>("date");
  const [ascending, setAscending] = useState(false);
  const [propertiesId, setPropertiesId] = useState<string | null>(null);
  const [menu, setMenu] = useState<MenuAnchor | null>(null);
  const closeMenu = useCallback(() => setMenu(null), []);
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

  async function jobAction(command: "retry_download" | "pause_download" | "stop_recording", id: string) {
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

  async function openFile(id: string, reveal = false) {
    setActionError("");
    try {
      await invoke("open_download", { id, reveal });
    } catch (failure) {
      setActionError(describe(failure));
    }
  }

  async function menuAction(job: Job, action: DownloadAction) {
    closeMenu();
    menu?.previous.focus();
    if (action === "properties") setPropertiesId(job.id);
    else if (action === "replace") setReplacement(job);
    else if (action === "move") setMoving(job);
    else if (action === "open" || action === "reveal") await openFile(job.id, action === "reveal");
    else if (action === "forget") {
      setActionError("");
      try {
        await invoke("remove_completed_download", { id: job.id });
        await refresh();
      } catch (failure) {
        setActionError(describe(failure));
      }
    } else {
      await jobAction(action === "pause" ? "pause_download" : action === "retry" ? "retry_download" : "stop_recording", job.id);
    }
  }

  const jobs = state?.jobs ?? [];
  const queues = state?.queues ?? [];
  const queue = queues.find(item => item.id === queueId) ?? queues[0];
  const queueJobs = jobs.filter(job => job.queueId === queue?.id);
  const menuJob = menu ? jobs.find(job => job.id === menu.id) : null;
  const properties = jobs.find(job => job.id === propertiesId);
  const activeJobs = queueJobs.filter(job => job.status === "downloading");
  const transferRate = activeJobs.reduce((total, job) => total + (job.transfer?.bytesPerSecond ?? 0), 0);
  const queued = queueJobs.filter(job => job.status === "queued").length;
  const visible = queueJobs.filter(job => (filter === "all" || job.status === filter) && (!category || fileCategory(job.filename) === category));
  const priorities = { low: 0, normal: 1, high: 2 };
  const positions = new Map(queueJobs.map((job, index) => [job.id, index]));
  visible.sort((left, right) => {
    const order = sort === "name" ? left.filename.localeCompare(right.filename, undefined, { numeric: true, sensitivity: "base" })
      : sort === "size" ? left.bytes - right.bytes
      : sort === "status" ? statusLabel[left.status].localeCompare(statusLabel[right.status])
      : sort === "priority" ? priorities[left.priority] - priorities[right.priority]
      : ((left.addedAt ?? 0) - (right.addedAt ?? 0) || (positions.get(right.id) ?? 0) - (positions.get(left.id) ?? 0));
    return order * (ascending ? 1 : -1);
  });

  async function toggleQueue() {
    if (!queue) return;
    setActionError("");
    try {
      await invoke("set_download_queue_running", { id: queue.id, running: !queue.running });
      await refresh();
    } catch (failure) {
      setActionError(describe(failure));
    }
  }
  const heading = filters.find(item => item.key === filter)?.label;
  const hostname = (url: string) => new URL(url).hostname;
  const closePanel = () => setPanel(null);

  return (
    <div className="layout">
      <header className="app-header" onMouseDown={event => {
        if (event.button === 0 && event.detail === 1 && event.target === event.currentTarget) {
          void getCurrentWindow().startDragging().catch(error => setActionError(describe(error)));
        }
      }}>
        <div className="brand"><img className="brand-icon" src="/outernet.svg" width={28} height={28} alt="" /> Outernet</div>
        <div className="app-actions">
          <button className="icon-button" title="Queue schedule" aria-label="Queue schedule" disabled={!state} onClick={() => setPanel("schedule")}><CalendarClock size={17} /></button>
          <button className="icon-button" title="Browser integration" aria-label="Browser integration" onClick={() => setPanel("browser")}><Globe size={17} /></button>
          <button className="icon-button" title="Settings" aria-label="Settings" disabled={!state} onClick={() => setPanel("settings")}><Settings2 size={17} /></button>
          <WindowControls onError={setActionError} />
        </div>
      </header>
      <main>
        <header className="queue-header">
          <h1 className="sr-only">Downloads</h1>
          <div className="queue-selector">
            {queue && <ChoicePicker value={queue.id} onChange={setQueueId} label="Download queue" icon={<Layers size={16} />}
              options={queues.map(item => [item.id, item.name])} />}
            <button type="button" className="icon-button" title="New queue" aria-label="New queue" disabled={!queue} onClick={() => setQueueForm("new")}><Plus size={17} /></button>
            {queue && <button type="button" className="icon-button" title="Queue settings" aria-label="Queue settings" onClick={() => setQueueForm(queue)}><Settings2 size={16} /></button>}
          </div>
          <div className="queue-actions">
            {queue && <button type="button" className="button secondary" onClick={() => void toggleQueue()}>
              {queue.running ? <Pause size={15} /> : <Play size={15} />}{queue.running ? "Stop queue" : "Start queue"}
            </button>}
            <button className="button queue-submit" disabled={!queue} onClick={() => setPanel("add")}><Plus size={17} /> Add download</button>
          </div>
        </header>
        {queue && <div className="queue-meta">
          <p className={activeJobs.length ? "queue-state receiving" : "queue-state"}><span />
            {!queue.running ? "Queue stopped" : activeJobs.length ? "Transfers in progress" : queued ? "Queue waiting" : "Queue idle"}
          </p>
          <span>{queue.interface ?? "System default"} · {queue.slots} simultaneous</span>
        </div>}
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
        {state && <dl className="queue-summary" aria-label="Queue summary">
          <div><dt>Download speed</dt><dd className={transferRate > 0 ? "receiving" : undefined}>{displaySize(transferRate)}/s</dd></div>
          <div><dt>Active</dt><dd>{activeJobs.length}</dd></div>
          <div><dt>Queued</dt><dd>{queued}</dd></div>
        </dl>}
        <div className="queue-controls">
          <nav className="queue-tabs" aria-label="Download filters">
            {filters.map(({ label, key }) => (
              <button key={key} type="button" aria-pressed={filter === key} onClick={() => setFilter(key)}>
                {key === "all" ? "All" : label}
              </button>
            ))}
          </nav>
          <div className="filter-actions">
          <ChoicePicker<SortKey> value={sort} onChange={key => { setSort(key); setAscending(key === "name" || key === "status"); }} label="Sort by"
            options={[["date", "Date added"], ["name", "Name"], ["size", "Size"], ["status", "Status"], ["priority", "Priority"]]} />
          <button className="icon-button" type="button" title={ascending ? "Ascending" : "Descending"}
            aria-label={ascending ? "Switch to descending sort" : "Switch to ascending sort"} onClick={() => setAscending(!ascending)}>
            {ascending ? <ArrowUp size={15} /> : <ArrowDown size={15} />}
          </button>
          <ChoicePicker value={category} onChange={setCategory} label="File type" icon={<ListFilter size={15} />}
            options={[["", "All types"], ...[...Object.keys(categories), "Other"].map(type => [type, type] as const)]} />
          </div>
        </div>
        <section className="download-list" aria-label={heading}>
          <table>
            <thead><tr><th>File</th><th>Status</th><th>Size</th><th><span className="sr-only">Actions</span></th></tr></thead>
            <tbody>
              {visible.map(job => (
                <tr key={job.id} className={job.status === "complete" ? "openable-row" : undefined} tabIndex={0}
                  data-menu-open={menu?.id === job.id}
                  onDoubleClick={event => {
                    if (job.status === "complete" && event.target instanceof Element && !event.target.closest("button")) void openFile(job.id);
                  }}
                  onContextMenu={event => {
                    event.preventDefault();
                    setMenu({ id: job.id, x: event.clientX, y: event.clientY, previous: event.currentTarget });
                  }}
                  onKeyDown={event => {
                    if (event.target !== event.currentTarget) return;
                    if (event.key === "Enter" && job.status === "complete") void openFile(job.id);
                    if (event.key === "ContextMenu" || (event.shiftKey && event.key === "F10")) {
                      event.preventDefault();
                      const rect = event.currentTarget.getBoundingClientRect();
                      setMenu({ id: job.id, x: rect.left + 16, y: rect.top + 20, previous: event.currentTarget });
                    }
                  }}>
                  <td>
                    <div className="file-cell"><span className="file-mark"><File size={16} /></span><div className="file-details">
                      <strong className="filename" title={job.filename}>{job.filename}</strong>
                      <small>{hostname(job.url)}</small>
                      {(job.status === "queued" || job.status === "paused" || job.status === "failed") && (
                        <div className="priority-control"><span>Priority</span>
                          <ChoicePicker<Priority> value={job.priority} label={`Priority for ${job.filename}`}
                            options={[["low", "Low"], ["normal", "Normal"], ["high", "High"]]}
                            onChange={priority => void setPriority(job.id, priority)} />
                        </div>
                      )}
                      {job.transfer && <DownloadProgress transfer={job.transfer} />}
                      {job.error && <small className="error">{job.error}</small>}
                    </div></div>
                  </td>
                  <td><span className={`badge ${job.status}`}>{job.format === "hls_live" && job.status === "downloading" ? "Recording" : statusLabel[job.status]}</span>
                    {job.status === "complete" && job.expectedSha256 && <small>SHA-256 verified</small>}</td>
                  <td className="file-size">
                    {job.bytes > 0 || job.transfer || job.status === "complete" ? displaySize(job.bytes) : "—"}
                    {job.transfer?.totalBytes != null && <small>of {displaySize(job.transfer.totalBytes)}</small>}
                  </td>
                  <td className="row-actions">
                    {job.status === "downloading" && job.format === "hls_live" && (
                      <button className="icon-button" title="Stop and save recording" aria-label={`Stop and save ${job.filename}`}
                        onClick={() => void jobAction("stop_recording", job.id)}><Square size={16} /></button>
                    )}
                    {(job.status === "queued" || (job.status === "downloading" && job.format !== "hls_live")) && (
                      <button className="icon-button" title="Pause" aria-label={`Pause ${job.filename}`} onClick={() => void jobAction("pause_download", job.id)}><Pause size={16} /></button>
                    )}
                    {(job.status === "paused" || job.status === "failed") && (
                      <button className="icon-button" title={job.status === "paused" ? "Resume" : "Retry"}
                        aria-label={`${job.status === "paused" ? "Resume" : "Retry"} ${job.filename}`}
                        onClick={() => void jobAction("retry_download", job.id)}>
                        {job.status === "paused" ? <Play size={16} /> : <RefreshCw size={16} />}
                      </button>
                    )}
                    <button className="icon-button" type="button" title="More actions" aria-label={`More actions for ${job.filename}`}
                      aria-haspopup="menu" aria-expanded={menu?.id === job.id} onClick={event => {
                        const rect = event.currentTarget.getBoundingClientRect();
                        setMenu({ id: job.id, x: rect.right - 190, y: rect.bottom + 4, previous: event.currentTarget });
                      }}><MoreHorizontal size={17} /></button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
          {!visible.length && (
            <div className="empty">
              <span className="empty-icon"><ArrowDownToLine size={26} /></span>
              <h2>{state && category ? "No downloads match this file type" : state ? `No ${filter === "all" ? "" : `${filter === "complete" ? "completed" : "failed"} `}downloads` : loadError ? "Downloads unavailable" : "Loading downloads…"}</h2>
              {state && filter === "all" && !category && <p>Use Add download to start a transfer from a link.</p>}
            </div>
          )}
        </section>
      </main>
      {menu && menuJob && <DownloadMenu job={menuJob} anchor={menu} canMove={queues.length > 1} onClose={closeMenu}
        onAction={action => void menuAction(menuJob, action)} />}
      {properties && <DownloadProperties job={properties} onClose={() => setPropertiesId(null)} />}
      {panel === "schedule" && state && <ScheduleForm runAfter={state.run_after} dailyWindow={state.daily_window} onClose={closePanel} onSave={refresh} />}
      {panel === "browser" && <BrowserSettings onClose={closePanel} />}
      {replacement && <ReplaceLink job={replacement} onClose={() => setReplacement(null)} onSave={refresh} />}
      {panel === "add" && queue && <AddDownload queues={queues} initialQueue={queue.id} onClose={closePanel} onSave={refresh} />}
      {queueForm && <QueueForm queue={queueForm === "new" ? null : queueForm} onClose={() => setQueueForm(null)}
        onSave={async id => { setQueueId(id); await refresh(); }} />}
      {moving && <MoveDownload job={moving} queues={queues} onClose={() => setMoving(null)} onSave={refresh} />}
      {panel === "settings" && state && <SettingsForm settings={state.settings} onClose={closePanel} onSave={refresh} />}
    </div>
  );
}

createRoot(document.getElementById("root")!).render(<App />);
