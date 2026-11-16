const api = globalThis.browser ?? globalThis.chrome;
const status = document.querySelector("#status");
const message = status.querySelector("span");
const list = document.querySelector("#streams");
const qualities = Array.from(document.querySelectorAll("#quality button"));
const automatic = document.querySelector("#automatic");
const { maxHeight, automaticHandoff } = await api.storage.local.get(["maxHeight", "automaticHandoff"]);
automatic.checked = Boolean(automaticHandoff);
automatic.addEventListener("change", () => {
  void api.storage.local.set({ automaticHandoff: automatic.checked });
});
let selectedHeight = Number(maxHeight) || 0;
function selectQuality(height) {
  selectedHeight = qualities.some(button => Number(button.dataset.height) === height) ? height : 0;
  for (const button of qualities) {
    button.setAttribute("aria-pressed", String(Number(button.dataset.height) === selectedHeight));
  }
}
function showStatus(text, state) {
  message.textContent = text;
  status.dataset.state = state;
}
selectQuality(selectedHeight);
for (const button of qualities) {
  button.addEventListener("click", () => {
    selectQuality(Number(button.dataset.height));
    void api.storage.local.set({ maxHeight: selectedHeight });
  });
}
try {
  const [tab] = await api.tabs.query({ active: true, currentWindow: true });
  const [connection, data] = await Promise.all([
    api.runtime.sendMessage({ action: "ping" }),
    api.runtime.sendMessage({ action: "streams", tabId: tab.id, pageUrl: tab.url }),
  ]);
  const error = connection.ok ? data?.result?.error : connection.error;
  const result = data?.result;
  const connectionText = result?.pending ? "Receiving through your browser…"
    : result?.ok ? result.popup ? "Opening download window…" : "Added to Outernet" : "Desktop app connected";
  showStatus(error || connectionText,
    error ? "error" : data?.result?.pending ? "checking" : "connected");
  if (!data?.streams?.length) {
    document.querySelector("#hint").textContent = "Start playing a video to detect its source, or right-click a link to download it.";
  }
  for (const stream of data?.streams ?? []) {
    const row = document.createElement("li");
    const url = new URL(stream.url);
    const name = url.pathname.split("/").pop() || url.hostname;
    const format = stream.format === "file" ? "video file" : stream.format.toUpperCase();
    const label = stream.format === "youtube" ? "Download YouTube video" : `Download ${format} · ${name}`;
    const actions = [[label, stream.format]];
    if (stream.format === "hls") actions.push([`Record live · ${name}`, "hls_live"]);
    for (const [label, format] of actions) {
      const button = document.createElement("button");
      button.textContent = label;
      button.addEventListener("click", async () => {
        button.disabled = true;
        try {
          const result = await api.runtime.sendMessage({ action: "download", tabId: tab.id, ...stream, format, maxHeight: selectedHeight });
          showStatus(result.ok ? (result.popup ? "Opening download window…" : "Added to Outernet") : result.error, result.ok ? "connected" : "error");
        } catch {
          showStatus("Could not send this download.", "error");
        } finally {
          button.disabled = false;
        }
      });
      row.append(button);
    }
    list.append(row);
  }
} catch {
  showStatus("Open Outernet and connect this browser in settings.", "error");
}
