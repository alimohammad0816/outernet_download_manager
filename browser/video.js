(() => {
  const api = globalThis.browser ?? globalThis.chrome;
  const container = document.createElement("div");
  container.style.cssText = "all:initial;position:fixed;display:none;z-index:2147483647";
  const shadow = container.attachShadow({ mode: "closed" });
  const style = document.createElement("style");
  style.textContent = `
    button { color:#f5f6f6; background:#111315; border:1px solid #3b4245; border-radius:7px;
      padding:6px 10px; font:600 12px system-ui,sans-serif; cursor:pointer }
    button:focus-visible { outline:2px solid #9ae3ca; outline-offset:2px }
    button:disabled { opacity:.75; cursor:wait }
    .launcher { box-shadow:0 2px 10px #0007; white-space:nowrap }
    .launcher::before { content:"↓"; margin-right:6px; color:#99dfc7 }
    .choices { position:absolute; inset:calc(100% + 4px) 0 auto auto; width:min(218px, 90vw);
      box-sizing:border-box; max-height:var(--menu-height, 260px); overflow:auto;
      display:grid; gap:2px; padding:4px; border:1px solid #3b4245; border-radius:9px;
      background:#111315; box-shadow:0 8px 22px #0009 }
    .choices:empty { display:none }
    .choices button { border-color:transparent; text-overflow:ellipsis; white-space:nowrap;
      overflow:hidden; text-align:left; font-weight:500; min-height:30px }
    .choices button:hover { background:#252b2d }
  `;
  const button = document.createElement("button");
  button.className = "launcher";
  button.type = "button";
  button.textContent = "Download this video";
  button.setAttribute("aria-expanded", "false");
  button.setAttribute("aria-controls", "outernet-video-choices");
  const choices = document.createElement("div");
  choices.className = "choices";
  choices.id = "outernet-video-choices";
  choices.setAttribute("role", "group");
  choices.setAttribute("aria-label", "Download options");
  shadow.append(style, button, choices);
  document.documentElement.append(container);

  let activeVideo;
  let activeSource;
  let activeUrl;
  let streams = [];
  let streamsPage = location.href;
  let pending = false;
  let messageTimeout;
  const listeners = new AbortController();

  function closeMenu() {
    choices.replaceChildren();
    button.setAttribute("aria-expanded", "false");
  }

  document.addEventListener("pointerdown", event => {
    if (!event.composedPath().includes(container)) closeMenu();
  }, { capture: true, signal: listeners.signal });
  shadow.addEventListener("keydown", event => {
    if (event.key === "Escape" && choices.childElementCount) {
      event.preventDefault();
      event.stopPropagation();
      closeMenu();
      button.focus();
    }
  });

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
    if (streamsPage !== location.href) {
      streamsPage = location.href;
      streams = [];
      closeMenu();
    }
    const videos = [...document.querySelectorAll("video")].filter(video => {
      const rect = video.getBoundingClientRect();
      return (video.currentSrc || streams.some(stream => stream.format === "youtube")) && rect.width >= 120 && rect.height >= 70
        && rect.bottom >= 48 && rect.top < innerHeight - 48
        && rect.right > 0 && rect.left < innerWidth;
    });
    const video = videos.find(item => !item.paused && !item.ended) ?? videos[0];
    if (video !== activeVideo || video?.currentSrc !== activeUrl) {
      activeVideo = video;
      activeUrl = video?.currentSrc;
      closeMenu();
    }
    const direct = video ? directSource(video) : null;
    activeSource = streams.find(stream => stream.format === "youtube")
      ?? (direct ? streams.find(stream => stream.url === direct.url) ?? direct : null);
    if (!video || (!activeSource && !streams.length) || document.fullscreenElement) {
      container.style.display = "none";
      return;
    }
    const rect = video.getBoundingClientRect();
    container.style.display = "block";
    const top = Math.max(6, rect.top > 39 ? rect.top - button.offsetHeight - 5 : rect.top + 8);
    const right = Math.min(innerWidth - 6, Math.max(button.offsetWidth + 6, rect.right - 8));
    container.style.left = `${right - button.offsetWidth}px`;
    container.style.top = `${top}px`;
    container.style.setProperty("--menu-height", `${Math.max(36, Math.min(268, innerHeight - top - button.offsetHeight - 11))}px`);
    if (choices.childElementCount) choices.style.right = `${Math.min(0, right - choices.offsetWidth - 6)}px`;
  }

  async function send(source) {
    window.clearTimeout(messageTimeout);
    pending = true;
    closeMenu();
    button.disabled = true;
    button.textContent = "Sending…";
    try {
      const result = await api.runtime.sendMessage({ action: "download", ...source });
      button.textContent = result.ok ? (result.popup ? "Opening download window…" : "Added to Outernet") : "Could not open download";
      button.title = result.error || "";
    } catch (error) {
      if (contextInvalidated(error)) disconnect();
      button.textContent = "Outernet unavailable";
    } finally {
      button.disabled = false;
      pending = false;
      if (!listeners.signal.aborted) messageTimeout = window.setTimeout(() => {
        button.textContent = "Download this video";
        button.title = "";
      }, 3500);
    }
  }

  function addChoice(label, action) {
    const option = document.createElement("button");
    option.type = "button";
    option.textContent = label;
    option.title = label;
    option.addEventListener("click", event => {
      if (event.isTrusted && !pending) action();
    });
    choices.append(option);
    position();
    button.setAttribute("aria-expanded", "true");
  }

  function chooseQuality(source) {
    closeMenu();
    if (source.format === "file") {
      addChoice("Download video file", () => void send(source));
      return;
    }
    if (source.format === "hls") {
      addChoice("Record live in Outernet", () => void send({ ...source, format: "hls_live" }));
    }
    for (const [height, label] of [[0, "Best available"], [1080, "Up to 1080p"], [720, "Up to 720p"], [480, "Up to 480p"], [360, "Up to 360p"]]) {
      addChoice(label, () => void send({ ...source, maxHeight: height }));
    }
  }

  button.addEventListener("click", event => {
    if (!event.isTrusted || pending) return;
    if (choices.childElementCount) {
      closeMenu();
      return;
    }
    if (activeSource) {
      chooseQuality(activeSource);
    } else if (streams.length === 1) {
      chooseQuality(streams[0]);
    } else {
      for (const stream of streams) {
        const url = new URL(stream.url);
        const format = stream.format === "file" ? "Video file" : stream.format.toUpperCase();
        addChoice(`${format} · ${url.pathname.split("/").pop() || url.hostname}`, () => chooseQuality(stream));
      }
    }
  });

  function contextInvalidated(error) {
    return String(error).includes("Extension context invalidated") || !api.runtime?.id;
  }

  function disconnect() {
    window.clearInterval(poll);
    window.clearTimeout(messageTimeout);
    listeners.abort();
    container.remove();
  }

  async function pollStreams(page) {
    try {
      const result = await api.runtime.sendMessage({ action: "streams", pageUrl: page });
      if (page !== location.href) return;
      streams = result?.streams ?? [];
      position();
    } catch (error) {
      if (contextInvalidated(error)) disconnect();
    }
  }

  let tick = 0;
  const poll = window.setInterval(() => {
    position();
    if (++tick % 3 !== 1 || !document.querySelector("video")) return;
    void pollStreams(location.href);
  }, 650);
  document.addEventListener("play", position, { capture: true, signal: listeners.signal });
  window.addEventListener("scroll", position, { passive: true, capture: true, signal: listeners.signal });
  window.addEventListener("resize", position, { signal: listeners.signal });
})();
