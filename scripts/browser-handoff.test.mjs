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
    emit(data) { callback?.(data); },
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
