const HOST = "io.outernet.downloadmanager";
const MENU = "outernet-download";
const MAX_STREAMS = 12;
const networkOperations = new Map();

chrome.runtime.onInstalled.addListener(() => {
  chrome.contextMenus.create({
    id: MENU,
    title: "Download with Outernet",
    contexts: ["link", "video", "audio"],
    targetUrlPatterns: ["http://*/*", "https://*/*"],
  });
});

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
    return await chrome.runtime.sendNativeMessage(HOST, message);
  } catch (error) {
    return { ok: false, error: `Cannot reach Outernet. Connect this browser in Outernet settings. (${error.message})` };
  }
}

async function download(tabId, source) {
  const url = webUrl(source.url);
  const { maxHeight } = source.format && source.format !== "file"
    ? await chrome.storage.local.get("maxHeight") : {};
  const response = url
    ? await requestHost({ action: "download", url, filename: source.filename, format: source.format, max_height: maxHeight })
    : { ok: false, error: "Choose a direct HTTP or HTTPS media source." };
  if (Number.isInteger(tabId) && tabId >= 0) {
    await chrome.storage.session.set({ [`result:${tabId}`]: response });
    await chrome.action.setBadgeText({ tabId, text: response.ok ? "" : "!" }).catch(() => {});
  }
  return response;
}

chrome.contextMenus.onClicked.addListener((info, tab) => {
  if (info.menuItemId === MENU) {
    const url = info.srcUrl || info.linkUrl;
    if (url) void download(tab?.id, { url, format: streamFormat(url) || "file" });
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
    const stored = (await chrome.storage.session.get(key))[key] ?? [];
    await chrome.storage.session.set({ [key]: change(stored) });
  });
  networkOperations.set(tabId, next);
  const finish = () => {
    if (networkOperations.get(tabId) === next) networkOperations.delete(tabId);
  };
  void next.then(finish, finish);
}

chrome.webRequest.onHeadersReceived.addListener(details => {
  if (details.tabId < 0 || details.method !== "GET" || details.statusCode < 200 || details.statusCode >= 300) return;
  const format = streamFormat(details.url, details.responseHeaders);
  if (!format) return;
  const entry = { url: details.url, format, frameId: details.frameId };
  updateStreams(details.tabId, streams => [
    entry,
    ...streams.filter(item => item.url !== entry.url || item.frameId !== entry.frameId),
  ].slice(0, MAX_STREAMS));
}, { urls: ["http://*/*", "https://*/*"], types: ["media", "xmlhttprequest", "other"] }, ["responseHeaders"]);

chrome.tabs.onUpdated.addListener((tabId, change) => {
  if (change.status === "loading") updateStreams(tabId, () => []);
});
chrome.tabs.onRemoved.addListener(tabId => {
  void (networkOperations.get(tabId) ?? Promise.resolve()).catch(() => {}).then(() =>
    chrome.storage.session.remove([`streams:${tabId}`, `result:${tabId}`]));
});

chrome.runtime.onMessage.addListener((message, sender, respond) => {
  const extensionPage = sender.url?.startsWith(chrome.runtime.getURL(""));
  const tabId = extensionPage ? message.tabId : sender.tab?.id;
  if (message.action === "ping" && extensionPage) {
    void requestHost({ action: "ping" }).then(respond);
  } else if (message.action === "download") {
    void download(tabId, message).then(respond, () => respond({ ok: false, error: "Could not send this download." }));
  } else if (message.action === "streams" && Number.isInteger(tabId) && tabId >= 0) {
    const key = `streams:${tabId}`;
    void chrome.storage.session.get([key, `result:${tabId}`]).then(data => {
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
