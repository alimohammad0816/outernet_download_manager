const api = globalThis.browser ?? globalThis.chrome;
const status = document.querySelector("#status");
const list = document.querySelector("#streams");
const quality = document.querySelector("#quality");
const automatic = document.querySelector("#automatic");
const { maxHeight, automaticHandoff } = await api.storage.local.get(["maxHeight", "automaticHandoff"]);
automatic.checked = Boolean(automaticHandoff);
automatic.addEventListener("change", () => {
  void api.storage.local.set({ automaticHandoff: automatic.checked });
});
quality.value = String(maxHeight || 0);
quality.addEventListener("change", () => {
  void api.storage.local.set({ maxHeight: Number(quality.value) });
});
try {
  const [tab] = await api.tabs.query({ active: true, currentWindow: true });
  const [connection, data] = await Promise.all([
    api.runtime.sendMessage({ action: "ping" }),
    api.runtime.sendMessage({ action: "streams", tabId: tab.id }),
  ]);
  status.textContent = connection.ok
    ? (data?.result?.pending ? "Receiving through your browser…" : data?.result?.error || "Desktop app connected")
    : connection.error;
  if (!data?.streams?.length) {
    document.querySelector("#hint").textContent = "Start playing a video to detect its source, or right-click a link to download it.";
  }
  for (const stream of data?.streams ?? []) {
    const row = document.createElement("li");
    const url = new URL(stream.url);
    const name = url.pathname.split("/").pop() || url.hostname;
    const format = stream.format === "file" ? "video file" : stream.format.toUpperCase();
    const actions = [[`Download ${format} · ${name}`, stream.format]];
    if (stream.format === "hls") actions.push([`Record live · ${name}`, "hls_live"]);
    for (const [label, format] of actions) {
      const button = document.createElement("button");
      button.textContent = label;
      button.addEventListener("click", async () => {
        button.disabled = true;
        try {
          const result = await api.runtime.sendMessage({ action: "download", tabId: tab.id, ...stream, format, maxHeight: Number(quality.value) });
          status.textContent = result.ok ? "Added to Outernet" : result.error;
        } catch {
          status.textContent = "Could not send this download.";
        } finally {
          button.disabled = false;
        }
      });
      row.append(button);
    }
    list.append(row);
  }
} catch {
  status.textContent = "Open Outernet and connect this browser in settings.";
}
