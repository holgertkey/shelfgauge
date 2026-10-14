// shelfgauge: applies the saved theme, sidebar width, reader settings and focus mode
// before the page is drawn (loaded without `defer`).
"use strict";

// Reader settings: the allowed values of each `data-r*` attribute on <html>; the first
// one is the default (no attribute). reader.css maps them to custom properties, and
// reader.js edits them through `shelfgaugeReader`.
const shelfgaugeReader = (() => {
  const KEY = "shelfgauge-reader";
  const FOCUS_KEY = "shelfgauge-focus";
  const OPTIONS = {
    font: ["sans", "serif"],
    size: ["3", "0", "1", "2", "4", "5", "6", "7"],
    leading: ["normal", "tight", "loose"],
    width: ["medium", "narrow", "wide", "full"],
    align: ["left", "justify"],
  };
  const ATTRS = {
    font: "rfont",
    size: "rsize",
    leading: "rleading",
    width: "rwidth",
    align: "ralign",
  };
  const root = document.documentElement;

  // Only known keys with allowed values; anything else (old or broken data) is dropped.
  function clean(value) {
    const settings = {};
    if (value && typeof value === "object") {
      for (const name of Object.keys(OPTIONS)) {
        const v = String(value[name]);
        if (OPTIONS[name].includes(v) && v !== OPTIONS[name][0]) {
          settings[name] = v;
        }
      }
    }
    return settings;
  }

  function load() {
    try {
      return clean(JSON.parse(localStorage.getItem(KEY)));
    } catch (e) {
      return {};
    }
  }

  function apply(settings) {
    for (const name of Object.keys(OPTIONS)) {
      if (settings[name]) {
        root.dataset[ATTRS[name]] = settings[name];
      } else {
        delete root.dataset[ATTRS[name]];
      }
    }
  }

  // Applies and remembers `settings` (missing keys: the defaults).
  function save(settings) {
    const value = clean(settings);
    apply(value);
    try {
      if (Object.keys(value).length) {
        localStorage.setItem(KEY, JSON.stringify(value));
      } else {
        localStorage.removeItem(KEY);
      }
    } catch (e) {
      // Not remembered.
    }
    return value;
  }

  function current() {
    const settings = {};
    for (const name of Object.keys(OPTIONS)) {
      settings[name] = root.dataset[ATTRS[name]] || OPTIONS[name][0];
    }
    return settings;
  }

  function setFocus(on) {
    root.classList.toggle("reading-focus", on);
    try {
      if (on) {
        localStorage.setItem(FOCUS_KEY, "1");
      } else {
        localStorage.removeItem(FOCUS_KEY);
      }
    } catch (e) {
      // Not remembered.
    }
  }

  apply(load());
  try {
    if (localStorage.getItem(FOCUS_KEY) === "1") {
      root.classList.add("reading-focus");
    }
  } catch (e) {
    // Storage unavailable: no focus mode.
  }

  return { OPTIONS, current, save, setFocus };
})();

(() => {
  const root = document.documentElement;
  root.classList.remove("no-js");
  root.classList.add("js");
  try {
    const theme = localStorage.getItem("shelfgauge-theme");
    if (theme) {
      root.dataset.theme = theme;
    }
    // CSSOM, not a style attribute: allowed by the CSP. app.js clamps it later.
    const width = Number(localStorage.getItem("shelfgauge-sidebar-width"));
    if (width > 0) {
      root.style.setProperty("--sidebar-width", `${width}px`);
    }
  } catch (e) {
    // Storage may be unavailable (private mode); the system theme is used.
  }
})();
