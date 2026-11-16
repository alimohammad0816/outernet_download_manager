import { createHash } from "node:crypto";
import { cp, mkdir, readFile, writeFile } from "node:fs/promises";
import { basename, dirname, join, resolve } from "node:path";

const destination = resolve("src-tauri/engines");
const release = "2026.10.26";
const windows = process.platform === "win32";
const upstream = windows ? "yt-dlp.exe" : "yt-dlp_linux";
const checksum = windows
  ? "f5389d3698ecde4ef06f4e6b1ce0294fe9ac1396228c8990b8d3416d6d177a7d"
  : "767ba9dce7052aa196f8b12652f433d76de128ba168966fa84b6394d5e6c6795";
const binary = join(destination, windows ? "yt-dlp.exe" : "yt-dlp");

async function download(url) {
  const response = await fetch(url, { signal: AbortSignal.timeout(120_000) });
  if (!response.ok) throw new Error(`Media engine request failed (${response.status}): ${url}`);
  return Buffer.from(await response.arrayBuffer());
}

await mkdir(destination, { recursive: true });
const previous = await readFile(binary).catch(error => {
  if (error.code !== "ENOENT") throw error;
});
const bytes = previous?.length && createHash("sha256").update(previous).digest("hex") === checksum
  ? previous : await download(`https://github.com/yt-dlp/yt-dlp/releases/download/${release}/${upstream}`);
if (createHash("sha256").update(bytes).digest("hex") !== checksum) {
  throw new Error("The yt-dlp checksum does not match the pinned upstream release.");
}
await writeFile(binary, bytes, { mode: 0o755 });
const nodeDirectory = dirname(process.execPath);
await cp(process.execPath, join(destination, basename(process.execPath)));
await cp(join(windows ? nodeDirectory : dirname(nodeDirectory), "LICENSE"), join(destination, "NODE-LICENSE.txt"));
await cp("LICENSE", join(destination, "GPL-3.0.txt"));
await writeFile(join(destination, "MEDIA-SOURCES.txt"), [
  `yt-dlp ${release}: https://github.com/yt-dlp/yt-dlp/tree/${release}`,
  "yt-dlp third-party notices: https://github.com/yt-dlp/yt-dlp#licensing",
  `Node.js ${process.version}: https://nodejs.org/dist/${process.version}/`,
  "",
].join("\n"));
