import assert from "node:assert/strict";
import { test } from "node:test";
import { pickHeaders, streamFormat, titleFilename } from "../browser/streams.js";

const type = value => [{ name: "Content-Type", value }];

test("recognizes manifests by path, media type, or a generic response naming the format", () => {
  assert.equal(streamFormat("https://cdn.test/v/master.m3u8?token=1"), "hls");
  assert.equal(streamFormat("https://cdn.test/v/master.m3u8/clip"), "hls");
  assert.equal(streamFormat("https://cdn.test/play", type("application/vnd.apple.mpegurl")), "hls");
  assert.equal(streamFormat("https://cdn.test/play", type("audio/mpegurl")), "hls");
  assert.equal(streamFormat("https://cdn.test/play?format=m3u8", type("application/octet-stream")), "hls");
  assert.equal(streamFormat("https://cdn.test/v/manifest.mpd"), "dash");
  assert.equal(streamFormat("https://cdn.test/list.m3u"), null);
  assert.equal(streamFormat("https://cdn.test/page?format=m3u8", type("text/html")), null);
  assert.equal(streamFormat("ftp://cdn.test/a.m3u8"), null);
});

test("forwards only the headers sites check, adding the browser agent when missing", () => {
  const headers = pickHeaders([
    { name: "Cookie", value: "session=1" },
    { name: "Referer", value: "https://site.test/watch" },
    { name: "Authorization", value: "Bearer secret" },
    { name: "Origin", value: "bad\r\nInjected: 1" },
  ], "Mozilla/5.0");
  assert.deepEqual(headers, { cookie: "session=1", referer: "https://site.test/watch", "user-agent": "Mozilla/5.0" });
});

test("builds portable filenames from page titles", () => {
  assert.equal(titleFilename('Episode 3: "Pilot" / Part 1 - YouTube'), "Episode 3 Pilot Part 1.mkv");
  assert.equal(titleFilename("trailing dots..."), "trailing dots.mkv");
  assert.equal(titleFilename("   "), undefined);
});
