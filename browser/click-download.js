(() => {
  const api = globalThis.browser ?? globalThis.chrome;
  const directFiles = /\.(?:zip|7z|rar|tar|gz|bz2|xz|zst|iso|dmg|deb|rpm|appimage|msi|exe|apk|epub)(?:$)/i;
  const pending = new Set();
  let enabled = false;

  void api.storage.local.get("automaticHandoff")
    .then(settings => { enabled = settings.automaticHandoff !== false; })
    .catch(() => {});
  api.storage.onChanged.addListener((changes, area) => {
    if (area === "local" && changes.automaticHandoff) {
      enabled = changes.automaticHandoff.newValue !== false;
    }
  });

  document.addEventListener("click", async event => {
    if (!enabled || !api.runtime?.id || api.extension?.inIncognitoContext || !event.isTrusted || event.defaultPrevented
      || event.button !== 0 || event.ctrlKey || event.shiftKey || event.metaKey || event.altKey) return;
    const link = event.composedPath().find(element => element instanceof HTMLAnchorElement);
    if (!link?.href || link.hasAttribute("ping")) return;
    let url;
    try {
      url = new URL(link.href);
    } catch {
      return;
    }
    if (!["http:", "https:"].includes(url.protocol) || url.username || url.password
      || (!link.hasAttribute("download") && !directFiles.test(url.pathname))) return;

    event.preventDefault();
    if (pending.has(url.href)) return;
    pending.add(url.href);
    try {
      const result = await api.runtime.sendMessage({
        action: "download",
        url: url.href,
        filename: link.getAttribute("download") || undefined,
        format: "file",
      });
      if (result?.ok) return;
    } catch {
      // Fall back to the original link when desktop messaging is unavailable.
    } finally {
      pending.delete(url.href);
    }
    const fallback = document.createElement("a");
    fallback.href = url.href;
    for (const attribute of ["download", "target", "rel", "referrerpolicy"]) {
      if (link.hasAttribute(attribute)) fallback.setAttribute(attribute, link.getAttribute(attribute));
    }
    document.documentElement.append(fallback);
    fallback.click();
    fallback.remove();
  });
})();
