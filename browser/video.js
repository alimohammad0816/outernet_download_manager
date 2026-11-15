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
  let activeUrl;
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
    if (video !== activeVideo || video?.currentSrc !== activeUrl) {
      activeVideo = video;
      activeUrl = video?.currentSrc;
      choices.replaceChildren();
    }
    const direct = video ? directSource(video) : null;
    activeSource = direct ? streams.find(stream => stream.url === direct.url) ?? direct : null;
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
      button.textContent = result.ok ? "Added to Outernet" : "Could not queue video";
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

  function addChoice(label, action) {
    const option = document.createElement("button");
    option.textContent = label;
    option.title = label;
    option.addEventListener("click", event => {
      if (event.isTrusted && !pending) action();
    });
    choices.append(option);
  }

  function chooseQuality(source) {
    if (source.format === "file") {
      void send(source);
      return;
    }
    choices.replaceChildren();
    addChoice("Use extension preference", () => void send(source));
    for (const [height, label] of [[0, "Best available"], [1080, "Up to 1080p"], [720, "Up to 720p"], [480, "Up to 480p"], [360, "Up to 360p"]]) {
      addChoice(label, () => void send({ ...source, maxHeight: height }));
    }
  }

  button.addEventListener("click", event => {
    if (!event.isTrusted || pending) return;
    if (choices.childElementCount) {
      choices.replaceChildren();
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
