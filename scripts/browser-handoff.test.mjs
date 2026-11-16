import assert from "node:assert/strict";
import { setTimeout as wait } from "node:timers/promises";
import { test } from "node:test";
import { receiveWithBrowser, rememberAttachment, trackDownloads } from "../browser/handoff.js";

const URL = "https://example.test/fixture.zip";
const LENGTH = 1024 * 1024;
const headers = {
  "content-type": "application/zip",
  "content-length": String(LENGTH),
  "content-disposition": 'attachment; filename="fixture.zip"',
  etag: '"stable"',
};

function signal() {
  let callback;
  return {
    addListener(listener) { callback = listener; },
    emit(...args) { callback?.(...args); },
  };
}

function browserFixture({ failChunk = false } = {}) {
  const data = new Map([["automaticHandoff", true]]);
  const item = { id: 42, url: URL, state: "in_progress", paused: false, danger: "safe",
    totalBytes: LENGTH, mime: "application/zip", filename: "C:\\Downloads\\fixture.zip" };
  const events = [];
  const chunks = [];
  return {
    events, chunks, data, item,
    api: {
      runtime: {
        onStartup: signal(),
        connectNative() {
          const onMessage = signal();
          const onDisconnect = signal();
          return {
            onMessage, onDisconnect,
            postMessage(message) {
              events.push(message.action);
              if (message.action === "chunk") chunks.push(Buffer.from(message.bytes, "base64"));
              queueMicrotask(() => onMessage.emit(
                failChunk && message.action === "chunk" ? { ok: false, error: "Disk full" }
                  : message.action === "receive" ? { ok: true, chunk_bytes: 8192 } : { ok: true },
              ));
            },
            disconnect() { onDisconnect.emit(); },
          };
        },
      },
      storage: { local: {
        async get(key) { return key ? { [key]: data.get(key) } : Object.fromEntries(data); },
        async set(input) { for (const [key, value] of Object.entries(input)) data.set(key, value); },
        async remove(key) { data.delete(key); },
      } },
      alarms: { onAlarm: signal(), async get() {}, async create() {} },
      downloads: {
        onCreated: signal(), onChanged: signal(), onErased: signal(),
        async search() { return [{ ...item }]; },
        async pause() { events.push("pause"); item.paused = true; },
        async resume() { events.push("resume"); item.paused = false; item.state = "in_progress"; },
        async cancel() { events.push("cancel"); item.state = "interrupted"; item.paused = false; },
      },
    },
  };
}

function observe() {
  rememberAttachment({
    url: URL,
    responseHeaders: Object.entries(headers).map(([name, value]) => ({ name, value })),
  });
}

async function until(predicate) {
  for (let tries = 0; tries < 100; tries++) {
    if (predicate()) return;
    await wait(5);
  }
  assert.ok(predicate(), "Expected the download handoff to finish");
}

test("automatic transfer preserves the original until the native host stores the whole body", async context => {
  const fixture = browserFixture();
  context.mock.method(globalThis, "fetch", async (_url, options) => {
    assert.equal(options.credentials, "include");
    assert.equal(options.redirect, "error");
    assert.equal(options.headers["If-Match"], '"stable"');
    return new Response(Buffer.alloc(LENGTH, 37), { headers });
  });
  observe();
  trackDownloads(fixture.api, "host");
  fixture.api.downloads.onCreated.emit(fixture.item);
  await until(() => fixture.events.includes("cancel"));
  assert.deepEqual(fixture.events.slice(0, 3), ["receive", "pause", "chunk"]);
  assert.deepEqual(fixture.events.slice(-2), ["finish", "cancel"]);
  assert.deepEqual(Buffer.concat(fixture.chunks), Buffer.alloc(LENGTH, 37));
  assert.ok(fixture.chunks.every(chunk => chunk.length <= 8192));
});

test("automatic transfer resumes the browser download after a failed write", async context => {
  const fixture = browserFixture({ failChunk: true });
  context.mock.method(globalThis, "fetch", async () => new Response(Buffer.alloc(LENGTH), { headers }));
  observe();
  trackDownloads(fixture.api, "host");
  fixture.api.downloads.onCreated.emit(fixture.item);
  await until(() => fixture.events.includes("resume"));
  assert.deepEqual(fixture.events, ["receive", "pause", "chunk", "abort", "resume"]);
  assert.equal(fixture.item.paused, false);
});

test("the session transfer rejects a login page even with a binary response type", async context => {
  const fixture = browserFixture();
  const html = "<!doctype html><html>Sign in</html>";
  context.mock.method(globalThis, "fetch", async () => new Response(html, {
    headers: { "content-type": "application/octet-stream", "content-length": String(html.length) },
  }));
  await assert.rejects(receiveWithBrowser(fixture.api, "host", URL), /HTML page/);
  assert.deepEqual(fixture.events, []);
});

test("a browser restart resumes an interrupted original that can still resume", async () => {
  const fixture = browserFixture();
  fixture.item.state = "interrupted";
  fixture.item.canResume = true;
  fixture.data.set("outernet-paused:42", true);
  trackDownloads(fixture.api, "host");
  fixture.api.runtime.onStartup.emit();
  await until(() => fixture.events.includes("resume"));
  assert.equal(fixture.item.state, "in_progress");
});


test("attachment headers arriving after a browser download can trigger the handoff", async context => {
  const fixture = browserFixture();
  fixture.item.totalBytes = -1;
  const freshUrl = URL.replace("/fixture.zip", "/fresh.zip");
  fixture.item.url = freshUrl;
  fixture.item.filename = "/downloads/fresh.zip";
  context.mock.method(globalThis, "fetch", async () => new Response(Buffer.alloc(LENGTH), { headers }));
  const reconsider = trackDownloads(fixture.api, "host");
  fixture.api.downloads.onCreated.emit(fixture.item);
  await wait(10);
  assert.deepEqual(fixture.events, []);
  assert(rememberAttachment({
    url: freshUrl,
    responseHeaders: Object.entries({ ...headers, "content-disposition": 'attachment; filename="fresh.zip"' })
      .map(([name, value]) => ({ name, value })),
  }));
  reconsider(freshUrl);
  await until(() => fixture.events.includes("cancel"));
});

test("browser finds progressive and adaptive video and forwards the chosen quality", async () => {
  const fixture = browserFixture();
  const session = new Map();
  const native = [];
  const headers = signal();
  const messages = signal();
  const api = fixture.api;
  Object.assign(api.runtime, {
    onInstalled: signal(),
    onMessage: messages,
    getURL: path => `chrome-extension://outernet/${path}`,
    async sendNativeMessage(_host, message) { native.push(message); return { ok: true }; },
  });
  Object.assign(api, {
    contextMenus: { onClicked: signal() },
    tabs: { onUpdated: signal(), onRemoved: signal() },
    webRequest: { onHeadersReceived: headers },
    action: { async setBadgeText() {} },
  });
  api.storage.session = {
    async get(keys) {
      return Object.fromEntries([keys].flat().map(key => [key, session.get(key)]));
    },
    async set(values) {
      for (const [key, value] of Object.entries(values)) session.set(key, value);
    },
  };
  fixture.data.set("maxHeight", 720);
  globalThis.chrome = api;
  try {
    await import("../browser/background.js");
    function observe(url, type, contentType, frameId = 0, statusCode = 200) {
      headers.emit({ url, type, statusCode, frameId, tabId: 8, method: "GET",
        responseHeaders: [{ name: "Content-Type", value: contentType }] });
    }
    const direct = "https://example.test/movie?id=1";
    const hls = "https://example.test/index?id=2";
    const dash = "https://example.test/index?id=3";
    observe(direct, "media", "video/mp4", 0, 206);
    observe(hls, "xmlhttprequest", "application/vnd.apple.mpegurl", 0);
    observe(hls, "xmlhttprequest", "application/vnd.apple.mpegurl", 1);
    observe(dash, "xmlhttprequest", "application/dash+xml", 1);
    observe("https://example.test/segment.m4s", "xmlhttprequest", "video/mp4", 0, 206);

    const popup = { url: api.runtime.getURL("popup.html") };
    const player = { url: "https://example.test", tab: { id: 8 }, frameId: 0 };
    const send = (message, sender = popup) => new Promise(resolve => messages.emit(message, sender, resolve));
    const toolbar = await send({ action: "streams", tabId: 8 });
    assert.deepEqual(toolbar.streams.map(({ url, format }) => [url, format]), [[dash, "dash"], [hls, "hls"], [direct, "file"]]);
    const frame = await send({ action: "streams" }, player);
    assert.deepEqual(frame.streams.map(({ url }) => url), [hls, direct]);
    await send({ action: "download", tabId: 8, ...frame.streams[0], maxHeight: 480 });
    await send({ action: "download", tabId: 8, ...frame.streams[0], maxHeight: 0 });
    await send({ action: "download", tabId: 8, ...frame.streams[0] });
    await send({ action: "download", tabId: 8, ...frame.streams[1], maxHeight: 480 });
    assert.deepEqual(native.map(message => message.max_height), [480, 0, 720, undefined]);
    assert.equal(native.at(-1).filename, "movie.mp4");
    await send({ action: "download", tabId: 8, ...frame.streams[0], format: "hls_live", maxHeight: 480 });
    assert.equal(native.at(-1).format, "hls_live");
    assert.equal(native.at(-1).max_height, 480);
    const youtube = await send({ action: "streams", tabId: 8, pageUrl: "https://www.youtube.com/watch?v=BaW_jenozKc&list=ignored" });
    assert.deepEqual(youtube.streams, [{
      url: "https://www.youtube.com/watch?v=BaW_jenozKc",
      filename: "youtube-BaW_jenozKc.mkv",
      format: "youtube",
    }]);
    await send({ action: "download", tabId: 8, ...youtube.streams[0], maxHeight: 1080 });
    assert.equal(native.at(-1).format, "youtube");
    assert.equal(native.at(-1).max_height, 1080);

  } finally {
    delete globalThis.chrome;
  }
});

test("a trusted file click opens Outernet and restores the browser link if the host is unreachable", async () => {
  const { readFile } = await import("node:fs/promises");
  const { runInNewContext } = await import("node:vm");
  const script = await readFile(new globalThis.URL("../browser/click-download.js", import.meta.url), "utf8");
  class Anchor {
    attributes = new Map();
    hasAttribute(key) { return this.attributes.has(key); }
    getAttribute(key) { return this.attributes.get(key); }
    setAttribute(key, value) { this.attributes.set(key, value); }
    click() { opened.push(this.href); }
    remove() {}
  }
  const opened = [];
  const sent = [];
  const storageChanges = signal();
  let redirectToBrowser = false;
  let click;
  runInNewContext(script, {
    URL: globalThis.URL,
    HTMLAnchorElement: Anchor,
    document: {
      addEventListener(_name, handler) { click = handler; },
      documentElement: { append() {} },
      createElement() { return new Anchor(); },
    },
    chrome: {
      storage: { local: { async get() { return {}; } }, onChanged: storageChanges },
      runtime: { id: "outernet", async sendMessage(message) { sent.push(message); return { ok: !redirectToBrowser }; } },
    },
  });
  await wait(0);
  const link = new Anchor();
  const event = { button: 0, isTrusted: true, defaultPrevented: false, composedPath: () => [link],
    preventDefault() { this.defaultPrevented = true; } };

  link.href = "https://example.test/download";
  await click(event);
  assert.equal(sent.length, 0);
  link.setAttribute("download", "archive.zip");
  await click(event);
  assert.equal(sent[0].filename, "archive.zip");
  assert.equal(event.defaultPrevented, true);

  redirectToBrowser = true;
  await click({ ...event, defaultPrevented: false });
  assert.deepEqual(opened, [link.href]);

  await click({ ...event, defaultPrevented: false, altKey: true });
  storageChanges.emit({ automaticHandoff: { newValue: false } }, "local");
  await click({ ...event, defaultPrevented: false });
  assert.equal(sent.length, 2);
});
