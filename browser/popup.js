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
  for (const stream of data?.streams ?? []) {
    const row = document.createElement("li");
    const button = document.createElement("button");
    const url = new URL(stream.url);
    button.textContent = `Download ${stream.format.toUpperCase()} · ${url.pathname.split("/").pop() || url.hostname}`;
    button.addEventListener("click", async () => {
      button.disabled = true;
      try {
        const result = await api.runtime.sendMessage({ action: "download", tabId: tab.id, ...stream });
        status.textContent = result.ok ? "Added to Outernet" : result.error;
      } catch {
        status.textContent = "Could not send this download.";
      } finally {
        button.disabled = false;
      }
    });
    row.append(button);
    list.append(row);
  }
} catch {
  status.textContent = "Open Outernet and connect this browser in settings.";
}
