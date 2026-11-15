import { receiveWithBrowser, trackDownloads, rememberAttachment } from "./handoff.js";
const api = globalThis.browser ?? globalThis.chrome;
const HOST = "io.outernet.downloadmanager";
const MENU = "outernet-download";
const SESSION_MENU = "outernet-session";
const MAX_STREAMS = 12;
const networkOperations = new Map();

api.runtime.onInstalled.addListener(() => {
  for (const [id, title] of [[MENU, "Download with Outernet"], [SESSION_MENU, "Download with Outernet using site session"]]) {
    api.contextMenus.create({
      id, title, contexts: ["link", "video", "audio"],
      targetUrlPatterns: ["http://*/*", "https://*/*"],
    });
  }
});
trackDownloads(api, HOST);

function webUrl(input) {
  try {
    const url = new URL(input);
    if (["http:", "https:"].includes(url.protocol) && !url.username && !url.password) {
      return url.href;
    }
  } catch {
    return null;
  }
  return null;
}

async function requestHost(message) {
  try {
    return await api.runtime.sendNativeMessage(HOST, message);
  } catch (error) {
    return { ok: false, error: `Cannot reach Outernet. Connect this browser in Outernet settings. (${error.message})` };
  }
}

async function download(tabId, source) {
  const url = webUrl(source.url);
  const { maxHeight } = source.format && source.format !== "file"
    ? await api.storage.local.get("maxHeight") : {};
  let response;
  if (!url) {
    response = { ok: false, error: "Choose a direct HTTP or HTTPS media source." };
  } else if (source.session) {
    try {
      if (source.format !== "file") throw new Error("Choose a direct file to download using a site session.");
      if (Number.isInteger(tabId) && tabId >= 0) {
        await api.storage.session.set({ [`result:${tabId}`]: { pending: true } });
        await api.action.setBadgeText({ tabId, text: "↓" }).catch(() => {});
      }
      await receiveWithBrowser(api, HOST, url, { filename: source.filename });
      response = { ok: true };
    } catch (error) {
      response = { ok: false, error: error.message };
    }
  } else {
    response = await requestHost({ action: "download", url, filename: source.filename, format: source.format, max_height: maxHeight });
  }
  if (Number.isInteger(tabId) && tabId >= 0) {
    await api.storage.session.set({ [`result:${tabId}`]: response });
    await api.action.setBadgeText({ tabId, text: response.ok ? "" : "!" }).catch(() => {});
  }
  return response;
}

api.contextMenus.onClicked.addListener((info, tab) => {
  if (info.menuItemId !== MENU && info.menuItemId !== SESSION_MENU) return;
  const url = info.srcUrl || info.linkUrl;
  const session = info.menuItemId === SESSION_MENU;
  if (url && !(session && tab?.incognito)) {
    void download(tab?.id, { url, session, format: streamFormat(url) || "file" });
  }
});

function streamFormat(url, headers = []) {
  const checked = webUrl(url);
  if (!checked) return null;
  const path = new URL(checked).pathname.toLowerCase();
  const type = headers.find(header => header.name.toLowerCase() === "content-type")?.value?.toLowerCase() || "";
  if (path.endsWith(".m3u8") || /(?:vnd\.apple\.mpegurl|x-mpegurl)/.test(type)) return "hls";
  if (path.endsWith(".mpd") || type.startsWith("application/dash+xml")) return "dash";
  return null;
}

function updateStreams(tabId, change) {
  const key = `streams:${tabId}`;
  const current = networkOperations.get(tabId) ?? Promise.resolve();
  const next = current.catch(() => {}).then(async () => {
    const stored = (await api.storage.session.get(key))[key] ?? [];
    await api.storage.session.set({ [key]: change(stored) });
  });
  networkOperations.set(tabId, next);
  const finish = () => {
    if (networkOperations.get(tabId) === next) networkOperations.delete(tabId);
  };
  void next.then(finish, finish);
}

api.webRequest.onHeadersReceived.addListener(details => {
  if (details.method !== "GET" || details.statusCode !== 200) return;
  rememberAttachment(details);
  if (details.tabId < 0) return;
  const format = streamFormat(details.url, details.responseHeaders);
  if (!format) return;
  const entry = { url: details.url, format, frameId: details.frameId };
  updateStreams(details.tabId, streams => [
    entry,
    ...streams.filter(item => item.url !== entry.url || item.frameId !== entry.frameId),
  ].slice(0, MAX_STREAMS));
}, { urls: ["http://*/*", "https://*/*"], types: ["main_frame", "sub_frame", "media", "xmlhttprequest", "other"] }, ["responseHeaders"]);

api.tabs.onUpdated.addListener((tabId, change) => {
  if (change.status === "loading") updateStreams(tabId, () => []);
});
api.tabs.onRemoved.addListener(tabId => {
  void (networkOperations.get(tabId) ?? Promise.resolve()).catch(() => {}).then(() =>
    api.storage.session.remove([`streams:${tabId}`, `result:${tabId}`]));
});

api.runtime.onMessage.addListener((message, sender, respond) => {
  const extensionPage = sender.url?.startsWith(api.runtime.getURL(""));
  const tabId = extensionPage ? message.tabId : sender.tab?.id;
  if (message.action === "ping" && extensionPage) {
    void requestHost({ action: "ping" }).then(respond);
  } else if (message.action === "download" && !message.session) {
    void download(tabId, message).then(respond, () => respond({ ok: false, error: "Could not send this download." }));
  } else if (message.action === "streams" && Number.isInteger(tabId) && tabId >= 0) {
    const key = `streams:${tabId}`;
    void api.storage.session.get([key, `result:${tabId}`]).then(data => {
      const items = data[key] ?? [];
      respond({
        streams: extensionPage ? items : items.filter(item => item.frameId === sender.frameId),
        result: data[`result:${tabId}`],
      });
    }, () => respond({ streams: [] }));
  } else {
    return false;
  }
  return true;
});
