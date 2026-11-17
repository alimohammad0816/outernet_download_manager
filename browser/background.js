import { receiveWithBrowser, trackDownloads, rememberAttachment } from "./handoff.js";
import { pickHeaders, streamFormat, titleFilename, webUrl } from "./streams.js";
const api = globalThis.browser ?? globalThis.chrome;
const HOST = "io.outernet.downloadmanager";
const MENU = "outernet-download";
const SESSION_MENU = "outernet-session";
const MAX_STREAMS = 12;
const QUALITY_CAPS = [1080, 720, 480, 360];
const MAX_PENDING_HEADERS = 400;
const networkOperations = new Map();
// Request headers by request ID, kept until the response arrives.
const sentHeaders = new Map();
const EXTRA_HEADERS = api.webRequest.OnSendHeadersOptions?.EXTRA_HEADERS;

api.runtime.onInstalled.addListener(() => {
  for (const [id, title] of [[MENU, "Download with Outernet"], [SESSION_MENU, "Download with Outernet using site session"]]) {
    api.contextMenus.create({
      id, title, contexts: ["link", "video", "audio"],
      targetUrlPatterns: ["http://*/*", "https://*/*"],
    });
  }
});
const reconsiderDownload = trackDownloads(api, HOST);

function youtubeSource(input) {
  const checked = webUrl(input);
  if (!checked) return null;
  const url = new URL(checked);
  let id;
  if (url.hostname === "youtu.be") {
    id = /^\/([\w-]{11})\/?$/.exec(url.pathname)?.[1];
  } else if (["youtube.com", "www.youtube.com", "m.youtube.com"].includes(url.hostname)) {
    id = url.pathname === "/watch" ? url.searchParams.get("v") : /^\/shorts\/([\w-]{11})\/?$/.exec(url.pathname)?.[1];
  }
  if (!id || !/^[\w-]{11}$/.test(id)) return null;
  return { url: `https://www.youtube.com/watch?v=${id}`, filename: `youtube-${id}.mkv`, format: "youtube" };
}

async function requestHost(message) {
  try {
    return await api.runtime.sendNativeMessage(HOST, message);
  } catch (error) {
    return { ok: false, error: `Cannot reach Outernet. Connect this browser in Outernet settings. (${error.message})` };
  }
}

// Headers the browser used for this source, so the desktop app is let in the same way.
async function sourceHeaders(tabId, source, pageUrl) {
  const fallback = pickHeaders([], navigator.userAgent);
  const page = webUrl(pageUrl);
  if (page) fallback.referer = page;
  if (!Number.isInteger(tabId) || tabId < 0) return fallback;
  if (source.format === "site") {
    const key = `page:${tabId}:${source.frameId ?? 0}`;
    const stored = (await api.storage.session.get(key))[key];
    const headers = stored?.url === source.url ? { ...stored.headers } : {};
    delete headers.origin;
    // An embedded player checks which page embeds it; a top-level page is its own referrer.
    const referer = (source.frameId ? headers.referer : undefined) ?? source.url;
    return { ...fallback, ...headers, referer };
  }
  await (networkOperations.get(tabId) ?? Promise.resolve()).catch(() => {});
  const key = `streams:${tabId}`;
  const entry = ((await api.storage.session.get(key))[key] ?? []).find(item => item.url === source.url);
  return entry?.headers ? { ...fallback, ...entry.headers } : fallback;
}

async function tabTitle(tabId) {
  if (!Number.isInteger(tabId) || tabId < 0) return undefined;
  try {
    return (await api.tabs.get(tabId)).title;
  } catch {
    return undefined;
  }
}

async function download(tabId, source, pageUrl) {
  const video = source.format === "youtube" ? youtubeSource(source.url) : null;
  const url = source.format === "youtube" ? video?.url : webUrl(source.url);
  const { maxHeight } = source.format && source.format !== "file"
    ? await api.storage.local.get("maxHeight") : {};
  const height = ["hls", "hls_live", "dash", "youtube", "site"].includes(source.format)
    ? ([0, ...QUALITY_CAPS].includes(source.maxHeight) ? source.maxHeight : maxHeight)
    : undefined;
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
      response = await receiveWithBrowser(api, HOST, url, { filename: source.filename, interactive: true });
    } catch (error) {
      response = { ok: false, error: error.message };
    }
  } else {
    const streaming = ["hls", "hls_live", "dash", "site"].includes(source.format);
    const headers = streaming ? await sourceHeaders(tabId, source, pageUrl) : undefined;
    const filename = video?.filename ?? source.filename
      ?? (streaming ? titleFilename(await tabTitle(tabId)) : undefined);
    response = await requestHost({ action: "download", url, filename, format: source.format, max_height: height, headers, interactive: true });
  }
  if (Number.isInteger(tabId) && tabId >= 0) {
    await api.storage.session.set({ [`result:${tabId}`]: response });
    await api.action.setBadgeText({ tabId, text: response.ok ? "✓" : "!" }).catch(() => {});
  }
  return response;
}

api.contextMenus.onClicked.addListener((info, tab) => {
  if (info.menuItemId !== MENU && info.menuItemId !== SESSION_MENU) return;
  const url = info.srcUrl || info.linkUrl;
  const session = info.menuItemId === SESSION_MENU;
  if (url && !(session && tab?.incognito)) {
    const video = !session && youtubeSource(url);
    void download(tab?.id, video || { url, session, format: streamFormat(url) || "file" }, tab?.url);
  }
});

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

const REQUEST_FILTER = { urls: ["http://*/*", "https://*/*"], types: ["main_frame", "sub_frame", "media", "xmlhttprequest", "other"] };

api.webRequest.onSendHeaders.addListener(details => {
  if (details.method !== "GET" || details.tabId < 0) return;
  const headers = pickHeaders(details.requestHeaders, navigator.userAgent);
  if (details.type === "main_frame" || details.type === "sub_frame") {
    const url = webUrl(details.url);
    if (url) {
      void api.storage.session.set({ [`page:${details.tabId}:${details.frameId}`]: { url, headers } });
    }
    return;
  }
  sentHeaders.delete(details.requestId);
  sentHeaders.set(details.requestId, headers);
  if (sentHeaders.size > MAX_PENDING_HEADERS) sentHeaders.delete(sentHeaders.keys().next().value);
}, REQUEST_FILTER, EXTRA_HEADERS ? ["requestHeaders", EXTRA_HEADERS] : ["requestHeaders"]);

for (const event of [api.webRequest.onCompleted, api.webRequest.onErrorOccurred]) {
  event.addListener(details => sentHeaders.delete(details.requestId), REQUEST_FILTER);
}

api.webRequest.onHeadersReceived.addListener(details => {
  const requestHeaders = sentHeaders.get(details.requestId);
  sentHeaders.delete(details.requestId);
  if (details.method !== "GET" || ![200, 206].includes(details.statusCode)) return;
  if (details.statusCode === 200 && rememberAttachment(details)) reconsiderDownload(details.url);
  if (details.tabId < 0) return;
  const url = webUrl(details.url);
  if (!url) return;
  const type = details.responseHeaders?.find(header => header.name.toLowerCase() === "content-type")?.value ?? "";
  const directType = details.type === "media" && /^video\/(mp4|webm|ogg)(?:\s*;|$)/i.exec(type);
  const format = streamFormat(url, details.responseHeaders) || (directType ? "file" : null);
  if (!format) return;
  const entry = { url, format, frameId: details.frameId };
  if (requestHeaders) entry.headers = requestHeaders;
  if (format === "file") {
    const leaf = new URL(url).pathname.split("/").pop();
    if (!leaf || !/\.[a-z0-9]{2,5}$/i.test(leaf)) {
      const extension = directType[1].toLowerCase();
      entry.filename = `${leaf || "video"}.${extension === "ogg" ? "ogv" : extension}`;
    }
  }
  updateStreams(details.tabId, streams => [
    entry,
    ...streams.filter(item => item.url !== entry.url || item.frameId !== entry.frameId),
  ].slice(0, MAX_STREAMS));
}, REQUEST_FILTER, ["responseHeaders"]);

api.tabs.onUpdated.addListener((tabId, change) => {
  if (change.status === "loading") updateStreams(tabId, () => []);
});
api.tabs.onRemoved.addListener(tabId => {
  void (networkOperations.get(tabId) ?? Promise.resolve()).catch(() => {}).then(async () => {
    const pages = Object.keys(await api.storage.session.get(null)).filter(key => key.startsWith(`page:${tabId}:`));
    await api.storage.session.remove([`streams:${tabId}`, `result:${tabId}`, ...pages]);
  });
});

api.runtime.onMessage.addListener((message, sender, respond) => {
  const extensionPage = sender.url?.startsWith(api.runtime.getURL(""));
  const tabId = extensionPage ? message.tabId : sender.tab?.id;
  if (message.action === "ping" && extensionPage) {
    void requestHost({ action: "ping" }).then(respond);
  } else if (message.action === "download" && !message.session) {
    const pageUrl = extensionPage ? message.pageUrl : sender.url;
    const source = message.format === "site"
      ? { ...message, url: extensionPage ? message.pageUrl : sender.url, frameId: extensionPage ? 0 : sender.frameId }
      : message;
    void download(tabId, source, pageUrl).then(respond, () => respond({ ok: false, error: "Could not send this download." }));
  } else if (message.action === "streams" && Number.isInteger(tabId) && tabId >= 0) {
    const key = `streams:${tabId}`;
    void (networkOperations.get(tabId) ?? Promise.resolve())
      .then(() => api.storage.session.get([key, `result:${tabId}`])).then(data => {
      const playerPage = webUrl(extensionPage ? message.pageUrl : sender.url);
      const requestedPage = webUrl(message.pageUrl);
      const currentPage = playerPage && requestedPage
        && new URL(playerPage).origin === new URL(requestedPage).origin ? requestedPage : playerPage;
      const video = youtubeSource(currentPage);
      const items = data[key] ?? [];
      const seen = new Set();
      respond({
        streams: video ? [video] : items.filter(item => {
          if ((!extensionPage && item.frameId !== sender.frameId) || seen.has(item.url)) return false;
          seen.add(item.url);
          return true;
        }).map(({ headers, ...item }) => item),
        result: data[`result:${tabId}`],
      });
    }, () => respond({ streams: [] }));
  } else {
    return false;
  }
  return true;
});
