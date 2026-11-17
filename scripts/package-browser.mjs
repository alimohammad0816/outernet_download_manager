import { cp, mkdir, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const sources = fileURLToPath(new URL("../browser/", import.meta.url));
const target = process.argv[2];
if (!target) throw new Error("Provide a destination for the browser packages.");
const manifest = JSON.parse(await readFile(join(sources, "manifest.json"), "utf8"));
const firefoxId = (await readFile(new URL("../src-tauri/firefox-extension-id", import.meta.url), "utf8")).trim();
const files = ["background.js", "handoff.js", "streams.js", "popup.js", "popup.html", "popup.css", "video.js", "click-download.js", "icon-16.png", "icon-32.png", "icon-48.png", "icon.png"];

for (const browser of ["chromium", "chromium-store", "firefox"]) {
  const directory = join(target, browser);
  await mkdir(directory, { recursive: true });
  await Promise.all(files.map(filename => cp(join(sources, filename), join(directory, filename))));
  const config = structuredClone(manifest);
  if (browser !== "chromium") delete config.key;
  if (browser === "firefox") {
    config.background = { scripts: ["background.js"], type: "module" };
    config.browser_specific_settings = {
      gecko: { id: firefoxId, strict_min_version: "140.0", data_collection_permissions: { required: ["none"] } },
    };
  }
  await writeFile(join(directory, "manifest.json"), JSON.stringify(config, null, 2) + "\n");
}
