// Pure helpers for recognizing video streams and the request context needed to fetch them.

const FORWARDED = ["cookie", "origin", "referer", "user-agent"];
const HLS_TYPE = /(?:vnd\.apple\.mpegurl|x-mpegurl|audio\/mpegurl|application\/mpegurl)/;
const GENERIC_TYPE = /^(?:application\/octet-stream|binary\/octet-stream|text\/plain)/;

export function webUrl(input) {
  try {
    const url = new URL(input);
    if (["http:", "https:"].includes(url.protocol) && !url.username && !url.password) {
      return url.href;
    }
  } catch {
    return null;
  }
  return null;
}

export function streamFormat(url, headers = []) {
  const checked = webUrl(url);
  if (!checked) return null;
  const parsed = new URL(checked);
  const path = parsed.pathname.toLowerCase();
  const type = headers.find(header => header.name.toLowerCase() === "content-type")?.value?.toLowerCase() || "";
  if (/\.m3u8(?:$|\/)/.test(path) || HLS_TYPE.test(type)) return "hls";
  if (/\.mpd(?:$|\/)/.test(path) || type.startsWith("application/dash+xml")) return "dash";
  // Many players load manifests from extensionless URLs served with a generic type.
  if (GENERIC_TYPE.test(type) || !type) {
    const query = parsed.search.toLowerCase();
    if (/m3u8/.test(query)) return "hls";
    if (/\bmpd\b/.test(query)) return "dash";
  }
  return null;
}

/** Keeps the headers a site checks before serving media: session cookie, referrer, origin, agent. */
export function pickHeaders(requestHeaders = [], userAgent) {
  const result = {};
  for (const { name, value } of requestHeaders) {
    const key = name.toLowerCase();
    if (FORWARDED.includes(key) && typeof value === "string" && value && !/[\r\n]/.test(value)) {
      result[key] = value;
    }
  }
  if (!result["user-agent"] && userAgent) result["user-agent"] = userAgent;
  return result;
}

/** Turns a page title into a filename that is valid on Linux and Windows. */
export function titleFilename(title, extension = "mkv") {
  const name = String(title ?? "")
    .replace(/[<>:"/\\|?*\u0000-\u001f]+/g, " ")
    .replace(/\s+-\s+(?:YouTube|Vimeo|Dailymotion)$/i, "")
    .replace(/\s+/g, " ")
    .trim()
    .slice(0, 120)
    .replace(/[. ]+$/, "");
  return name ? `${name}.${extension}` : undefined;
}
