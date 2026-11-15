// Debug builds only (the reading view includes this script then): `window.sgLog(...)`
// sends lines to the hub's log (`POST /debug/log`, `src/hub/client_log.rs`), for checks
// of intermediate builds on a phone through the staging hub. Lines are batched for
// half a second; each starts with this page's random tag and the seconds since it
// loaded. Errors and unhandled rejections are logged too.
(() => {
  "use strict";
  const meta = document.querySelector('meta[name="sg-log"]');
  if (!meta) {
    return;
  }
  const url = meta.content;
  const tag = Math.random().toString(36).slice(2, 6);
  const start = performance.now();
  let queue = [];
  let timer = 0;

  function flush() {
    clearTimeout(timer);
    timer = 0;
    if (!queue.length) {
      return;
    }
    const body = queue.join("\n");
    queue = [];
    fetch(url, {
      method: "POST",
      body,
      keepalive: true,
      headers: { "Content-Type": "text/plain" },
    }).catch(() => {});
  }

  function show(part) {
    if (typeof part === "string") {
      return part;
    }
    try {
      return JSON.stringify(part);
    } catch (e) {
      return String(part);
    }
  }

  window.sgLog = (...parts) => {
    const seconds = ((performance.now() - start) / 1000).toFixed(3);
    queue.push(`[${tag} ${seconds}] ${parts.map(show).join(" ")}`);
    if (!timer) {
      timer = setTimeout(flush, 500);
    }
  };

  window.addEventListener("error", (event) => {
    window.sgLog("error", event.message, `${event.filename}:${event.lineno}:${event.colno}`);
  });
  window.addEventListener("unhandledrejection", (event) => {
    window.sgLog("rejection", String(event.reason && (event.reason.stack || event.reason)));
  });
  window.addEventListener("pagehide", flush);
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "hidden") {
      flush();
    }
  });
  window.sgLog(
    "open",
    location.pathname,
    navigator.userAgent,
    `${window.innerWidth}x${window.innerHeight}@${window.devicePixelRatio}`,
  );
})();
