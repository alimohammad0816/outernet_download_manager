const status = document.querySelector("#status");
const list = document.querySelector("#streams");
const quality = document.querySelector("#quality");
const { maxHeight } = await chrome.storage.local.get("maxHeight");
quality.value = String(maxHeight || 0);
quality.addEventListener("change", () => {
  void chrome.storage.local.set({ maxHeight: Number(quality.value) });
});
try {
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  const [connection, data] = await Promise.all([
    chrome.runtime.sendMessage({ action: "ping" }),
    chrome.runtime.sendMessage({ action: "streams", tabId: tab.id }),
  ]);
  status.textContent = connection.ok ? (data?.result?.error || "Desktop app connected") : connection.error;
  for (const stream of data?.streams ?? []) {
    const row = document.createElement("li");
    const button = document.createElement("button");
    const url = new URL(stream.url);
    button.textContent = `Download ${stream.format.toUpperCase()} · ${url.pathname.split("/").pop() || url.hostname}`;
    button.addEventListener("click", async () => {
      button.disabled = true;
      try {
        const result = await chrome.runtime.sendMessage({ action: "download", tabId: tab.id, ...stream });
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
