const ACK_TIMEOUT = 30_000;
const CHUNK_SIZE = 32 * 1024;
const PAUSED_PREFIX = "outernet-paused:";
const RECOVERY_ALARM = "outernet-browser-recovery";
const observations = new Map();

function lengthOf(value) {
  if (!value || !/^\d+$/.test(value)) return null;
  const number = Number(value);
  return Number.isSafeInteger(number) ? number : null;
}

function mimeOf(value) {
  return value?.split(";")[0].trim().toLowerCase() ?? "";
}

function isFileMime(mime) {
  return mime && !["text/html", "application/xhtml+xml", "application/dash+xml"].includes(mime)
    && !mime.includes("mpegurl");
}

function attachmentName(disposition) {
  if (!disposition) return undefined;
  const utf8 = /(?:^|;)\s*filename\*=UTF-8''([^;]+)/i.exec(disposition);
  if (utf8) {
    try { return decodeURIComponent(utf8[1].trim()); } catch { /* use the plain filename */ }
  }
  const plain = /(?:^|;)\s*filename=(?:"([^"]+)"|([^;]+))/i.exec(disposition);
  return plain && (plain[1] ?? plain[2].trim());
}

function timedRead(reader, signal) {
  const timeout = setTimeout(() => signal.abort(), ACK_TIMEOUT);
  return reader.read().finally(() => clearTimeout(timeout));
}

function nativeSession(api, host) {
  const port = api.runtime.connectNative(host);
  let pending;
  let closed = false;
  port.onMessage.addListener(response => {
    if (!pending) return;
    const { settle } = pending;
    settle(response?.ok ? null : new Error(response?.error || "Outernet rejected the browser transfer."), response);
  });
  port.onDisconnect.addListener(() => {
    closed = true;
    const message = port.error?.message ?? api.runtime.lastError?.message;
    pending?.settle(new Error(message || "Outernet disconnected during the transfer."));
  });
  return {
    request(message) {
      if (closed) return Promise.reject(new Error("Connect this browser in Outernet settings."));
      return new Promise((resolve, reject) => {
        const timeout = setTimeout(() => pending?.settle(new Error("Outernet did not acknowledge the transfer.")), ACK_TIMEOUT);
        pending = {
          settle(error, response) {
            clearTimeout(timeout);
            pending = undefined;
            if (error) reject(error);
            else resolve(response);
          },
        };
        try { port.postMessage(message); } catch (error) { pending.settle(error); }
      });
    },
    close() {
      if (!closed) port.disconnect();
    },
  };
}

export async function receiveWithBrowser(api, host, url, { filename, expected, onReady } = {}) {
  const source = new URL(url);
  if (!["http:", "https:"].includes(source.protocol) || source.username || source.password
      || /\.(m3u8|mpd)$/i.test(source.pathname)) {
    throw new Error("Choose a direct HTTP or HTTPS file.");
  }
  const controller = new AbortController();
  let response;
  const headerTimeout = setTimeout(() => controller.abort(), ACK_TIMEOUT);
  try {
    response = await fetch(source, {
      credentials: "include",
      cache: "no-store",
      redirect: expected ? "error" : "follow",
      signal: controller.signal,
      headers: expected ? { "If-Match": expected.etag } : {},
    });
  } finally {
    clearTimeout(headerTimeout);
  }
  const length = lengthOf(response.headers.get("content-length"));
  const mime = mimeOf(response.headers.get("content-type"));
  const encoding = response.headers.get("content-encoding")?.toLowerCase();
  if (response.status !== 200 || !response.body || !isFileMime(mime) || length === null
      || response.headers.has("content-range") || (encoding && encoding !== "identity")
      || (expected && (length !== expected.length || mime !== expected.mime || response.headers.get("etag") !== expected.etag))) {
    controller.abort();
    throw new Error("The site did not return the expected file and response length. Check your sign-in or use the browser’s download.");
  }
  let reader;
  let native;
  try {
    reader = response.body.getReader();
    const preview = new Uint8Array(Math.min(length, 512));
    const initial = [];
    let filled = 0;
    while (filled < preview.length) {
      const part = await timedRead(reader, controller);
      if (part.done) break;
      initial.push(part.value);
      const bytes = part.value.subarray(0, preview.length - filled);
      preview.set(bytes, filled);
      filled += bytes.length;
    }
    const prefix = new TextDecoder().decode(preview.subarray(0, filled)).trimStart();
    if (/^<(?:!doctype\s+html|html\b|head\b|body\b|form\b)/i.test(prefix)) {
      throw new Error("The site returned an HTML page instead of download data.");
    }
    native = nativeSession(api, host);
    const ready = await native.request({
      action: "receive", url: response.url || source.href,
      filename: filename || attachmentName(response.headers.get("content-disposition")),
      length,
    });
    const packetSize = Number.isInteger(ready.chunk_bytes) && ready.chunk_bytes > 0
      ? Math.min(ready.chunk_bytes, CHUNK_SIZE) : CHUNK_SIZE;
    if (onReady) await onReady();
    async function sendBytes(bytes) {
      for (let offset = 0; offset < bytes.length; offset += packetSize) {
        const slice = bytes.subarray(offset, offset + packetSize);
        await native.request({ action: "chunk", bytes: btoa(String.fromCharCode(...slice)) });
      }
    }
    for (const bytes of initial) await sendBytes(bytes);
    while (true) {
      const part = await timedRead(reader, controller);
      if (part.done) break;
      await sendBytes(part.value);
    }
    await native.request({ action: "finish" });
  } catch (error) {
    if (native) await native.request({ action: "abort" }).catch(() => {});
    throw error;
  } finally {
    controller.abort();
    native?.close();
  }
}

const AUTOMATIC_EXTENSIONS = /\.(pdf|zip|7z|rar|tar|gz|bz2|xz|iso|mp4|mkv|webm|mov|mp3|flac|ogg|wav|png|jpg|jpeg|webp|gif|csv)$/i;

export function rememberAttachment(details) {
  if (details.incognito) return;
  const headers = new Map(details.responseHeaders?.map(item => [item.name.toLowerCase(), item.value]));
  const disposition = headers.get("content-disposition");
  const etag = headers.get("etag");
  const length = lengthOf(headers.get("content-length"));
  const mime = mimeOf(headers.get("content-type"));
  const encoding = headers.get("content-encoding");
  if (!/^attachment\b/i.test(disposition ?? "") || !/^"[!#-~]*"$/.test(etag ?? "")
      || length === null || length < 1_048_576 || !isFileMime(mime)
      || (encoding && encoding.toLowerCase() !== "identity")) return;
  if (observations.size >= 128) observations.delete(observations.keys().next().value);
  const filename = attachmentName(disposition);
  observations.set(details.url, { length, etag, mime, filename, at: Date.now() });
  return true;
}

export function trackDownloads(api, host) {
  const active = new Set();
  const rescan = new Set();
  const attempted = new Set();

  async function resumeOriginal(id, key) {
    const [item] = await api.downloads.search({ id });
    if ((item?.state === "in_progress" && item.paused) || (item?.state === "interrupted" && item.canResume)) {
      await api.downloads.resume(id);
    }
    await api.storage.local.remove(key);
  }

  async function recover() {
    const stored = await api.storage.local.get(null);
    for (const key of Object.keys(stored).filter(key => key.startsWith(PAUSED_PREFIX))) {
      const id = Number(key.slice(PAUSED_PREFIX.length));
      if (!active.has(id)) await resumeOriginal(id, key).catch(() => {});
    }
  }

  async function attempt(id) {
    if (attempted.has(id)) return;
    if (active.has(id)) {
      rescan.add(id);
      return;
    }
    active.add(id);
    const key = `${PAUSED_PREFIX}${id}`;
    let marked = false;
    try {
      if (!(await api.storage.local.get("automaticHandoff")).automaticHandoff) return;
      const [item] = await api.downloads.search({ id });
      if (!item || item.incognito || item.paused || item.state !== "in_progress"
          || item.byExtensionId || (item.cookieStoreId && item.cookieStoreId !== "firefox-default")
          || !["safe", "accepted"].includes(item.danger)) return;
      const url = item.finalUrl || item.url;
      if (url !== item.url) return;
      const expected = observations.get(url);
      const filename = item.filename?.split(/[\\/]/).pop();
      if (!expected || Date.now() - expected.at > 30_000
          || (item.startTime && Math.abs(Date.parse(item.startTime) - expected.at) > 30_000)
          || (item.totalBytes >= 0 ? item.totalBytes !== expected.length : filename !== expected.filename)
          || mimeOf(item.mime) !== expected.mime || !filename || !AUTOMATIC_EXTENSIONS.test(filename)) return;
      if (attempted.size >= 256) attempted.delete(attempted.values().next().value);
      attempted.add(id);
      await receiveWithBrowser(api, host, url, {
        filename,
        expected,
        async onReady() {
          const [current] = await api.downloads.search({ id });
          if (current?.state !== "in_progress" || current.paused || !["safe", "accepted"].includes(current.danger)) {
            throw new Error("The original browser download is no longer available.");
          }
          await api.storage.local.set({ [key]: true });
          marked = true;
          await api.downloads.pause(id);
        },
      });
      // The desktop acknowledgment means all bytes are durably stored and queued.
      await api.downloads.cancel(id);
      await api.storage.local.remove(key);
    } catch {
      if (marked) await resumeOriginal(id, key).catch(() => {});
    } finally {
      active.delete(id);
      if (rescan.delete(id) && !attempted.has(id)) void attempt(id);
    }
  }

  api.downloads.onCreated.addListener(item => { void attempt(item.id); });
  api.downloads.onChanged.addListener(change => {
    if (change.filename || change.totalBytes || change.mime || change.finalUrl || change.danger) {
      void attempt(change.id);
    }
  });
  api.downloads.onErased.addListener(id => {
    attempted.delete(id);
    void api.storage.local.remove(`${PAUSED_PREFIX}${id}`);
  });
  api.runtime.onStartup.addListener(() => { void recover().catch(() => {}); });
  api.alarms.onAlarm.addListener(alarm => {
    if (alarm.name === RECOVERY_ALARM) void recover().catch(() => {});
  });
  void api.alarms.get(RECOVERY_ALARM).then(alarm => {
    if (!alarm) return api.alarms.create(RECOVERY_ALARM, { periodInMinutes: 1 });
  }).catch(() => {});

  // Firefox can announce the download before WebRequest supplies its attachment headers.
  return url => {
    void (async () => {
      if (!(await api.storage.local.get("automaticHandoff")).automaticHandoff) return;
      const downloads = await api.downloads.search({ state: "in_progress", url });
      for (const item of downloads) {
        if (item.url === url) void attempt(item.id);
      }
    })().catch(() => {});
  };
}
