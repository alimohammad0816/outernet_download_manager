(() => {
  const api = globalThis.browser ?? globalThis.chrome;
  const container = document.createElement("div");
  container.style.cssText = "all:initial;position:fixed;display:none;z-index:2147483647";
  const shadow = container.attachShadow({ mode: "closed" });
  const style = document.createElement("style");
  style.textContent = `
    button { color:#fafafa; background:#18181b; border:1px solid #777; border-radius:7px; padding:7px 11px;
      font:600 13px system-ui,sans-serif; cursor:pointer; box-shadow:0 2px 8px #0005 }
    button:focus-visible { outline:2px solid #bee3fc; outline-offset:2px }
    button:disabled { opacity:.75; cursor:wait }
    .choices { margin-top:4px; display:grid; gap:3px; max-width:220px }
    .choices:empty { display:none }
    .choices button { overflow:hidden; text-overflow:ellipsis; white-space:nowrap; text-align:left }
  `;
  const button = document.createElement("button");
  button.textContent = "Download this video";
  const choices = document.createElement("div");
  choices.className = "choices";
  shadow.append(style, button, choices);
  document.documentElement.append(container);

  let activeVideo;
  let activeSource;
  let streams = [];
  let pending = false;
  let messageTimeout;

  function directSource(video) {
    try {
      const url = new URL(video.currentSrc);
      if (!["http:", "https:"].includes(url.protocol)) return null;
      const path = url.pathname.toLowerCase();
      const format = path.endsWith(".m3u8") ? "hls" : path.endsWith(".mpd") ? "dash" : "file";
      return { url: url.href, format };
    } catch {
      return null;
    }
  }

  function position() {
    const videos = [...document.querySelectorAll("video")].filter(video => {
      const rect = video.getBoundingClientRect();
      return video.currentSrc && rect.width >= 120 && rect.height >= 70
        && rect.bottom >= 48 && rect.top < innerHeight - 48
        && rect.right > 0 && rect.left < innerWidth;
    });
    const video = videos.find(item => !item.paused && !item.ended) ?? videos[0];
    if (video !== activeVideo) {
      activeVideo = video;
      choices.replaceChildren();
    }
    activeSource = video ? directSource(video) : null;
    if (!video || (!activeSource && !streams.length) || document.fullscreenElement) {
      container.style.display = "none";
      return;
    }
    const rect = video.getBoundingClientRect();
    container.style.display = "block";
    container.style.left = `${Math.max(5, Math.min(innerWidth - 205, rect.right - container.offsetWidth - 10))}px`;
    container.style.top = `${Math.max(5, rect.top + 10)}px`;
  }

  async function send(source) {
    window.clearTimeout(messageTimeout);
    pending = true;
    choices.replaceChildren();
    button.disabled = true;
    button.textContent = "Sending…";
    try {
      const result = await api.runtime.sendMessage({ action: "download", ...source });
      button.textContent = result.ok ? "Added to Outernet" : "Open Outernet settings";
      button.title = result.error || "";
    } catch {
      button.textContent = "Outernet unavailable";
    } finally {
      button.disabled = false;
      pending = false;
      messageTimeout = window.setTimeout(() => {
        button.textContent = "Download this video";
        button.title = "";
      }, 3500);
    }
  }

  button.addEventListener("click", event => {
    if (!event.isTrusted || pending) return;
    if (activeSource) {
      void send(activeSource);
    } else if (streams.length === 1) {
      void send(streams[0]);
    } else if (choices.childElementCount) {
      choices.replaceChildren();
    } else {
      for (const stream of streams) {
        const option = document.createElement("button");
        const url = new URL(stream.url);
        option.textContent = `${stream.format.toUpperCase()} · ${url.pathname.split("/").pop() || url.hostname}`;
        option.addEventListener("click", click => { if (click.isTrusted) void send(stream); });
        choices.append(option);
      }
    }
  });

  let tick = 0;
  window.setInterval(() => {
    position();
    if (++tick % 3 !== 1 || !document.querySelector("video")) return;
    void api.runtime.sendMessage({ action: "streams" }).then(result => {
      streams = result?.streams ?? [];
      position();
    }).catch(() => {});
  }, 650);
  document.addEventListener("play", position, true);
  window.addEventListener("scroll", position, { passive: true, capture: true });
  window.addEventListener("resize", position);
})();
