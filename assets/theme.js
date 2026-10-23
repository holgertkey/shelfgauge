// shelfgauge: the theme and reader settings of the profile, and the browser's own
// layout state (sidebar width, focus mode) before the page is drawn (loaded without
// `defer`).
//
// The server renders the profile's theme and reader settings as `data-theme` and
// `data-r*` on <html>; changes are applied at once and saved to the profile with a
// POST to `data-settings`. Settings that older versions kept in this browser are
// adopted once, when the server asks for it with `data-adopt`.
// The hub's reader app has no profile: with `data-local-settings` the settings are
// kept in this browser instead.
"use strict";

// Reader settings: the allowed values of each `data-r*` attribute on <html>; the first
// one is the default (no attribute). reader.css maps them to custom properties, and
// reader.js edits them through `shelfgaugeReader`. The same lists are in
// src/profiles/mod.rs (`SETTINGS`); tests/fixtures/profiles/settings.json keeps them
// equal.
const shelfgaugeReader = (() => {
  // Kept by older versions; adopted once, then removed.
  const KEY = "shelfgauge-reader";
  const THEME_KEY = "shelfgauge-theme";
  const FOCUS_KEY = "shelfgauge-focus";
  // The settings of the hub's reader app (`data-local-settings`).
  const LOCAL_KEY = "shelfgauge-hub-reader";
  const OPTIONS = {
    font: ["sans", "serif"],
    size: ["3", "0", "1", "2", "4", "5", "6", "7"],
    leading: ["normal", "tight", "loose"],
    width: ["medium", "narrow", "wide", "full"],
    align: ["left", "justify"],
    // Focus mode's running head and status line.
    status: ["on", "off"],
  };
  const THEMES = ["", "light", "dark", "sepia", "navy", "black"];
  const ATTRS = {
    font: "rfont",
    size: "rsize",
    leading: "rleading",
    width: "rwidth",
    align: "ralign",
    status: "rstatus",
  };
  const root = document.documentElement;
  const SAVE_DELAY = 400;
  let timer = null;

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

  function apply(settings) {
    for (const name of Object.keys(OPTIONS)) {
      if (settings[name]) {
        root.dataset[ATTRS[name]] = settings[name];
      } else {
        delete root.dataset[ATTRS[name]];
      }
    }
  }

  function current() {
    const settings = {};
    for (const name of Object.keys(OPTIONS)) {
      settings[name] = root.dataset[ATTRS[name]] || OPTIONS[name][0];
    }
    return settings;
  }

  // Sends the theme and reader settings shown now to the profile. A failure is
  // announced with a `shelfgauge:save-failed` event; without a profile (401) the page
  // is reloaded, which leads to the profile choice.
  function send() {
    clearTimeout(timer);
    timer = null;
    const body = Object.assign(current(), { theme: root.dataset.theme || "" });
    if ("localSettings" in root.dataset) {
      try {
        localStorage.setItem(LOCAL_KEY, JSON.stringify(body));
      } catch (e) {
        // Not remembered.
      }
      return Promise.resolve(true);
    }
    const url = root.dataset.settings;
    if (!url) {
      return Promise.resolve(false);
    }
    return fetch(url, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
      keepalive: true,
    }).then((response) => {
      if (response.status === 401) {
        window.location.reload();
        return false;
      }
      if (!response.ok) {
        throw new Error(`${response.status}`);
      }
      return true;
    }).catch((e) => {
      console.error(e);
      document.dispatchEvent(new CustomEvent("shelfgauge:save-failed"));
      return false;
    });
  }

  // Saves soon: several quick changes (the size buttons) make one request.
  function persist() {
    clearTimeout(timer);
    timer = setTimeout(send, SAVE_DELAY);
  }

  // A change not sent yet goes out when the page is left.
  window.addEventListener("pagehide", () => {
    if (timer !== null) {
      send();
    }
  });

  // Applies and saves `settings` (missing keys: the defaults).
  function save(settings) {
    const value = clean(settings);
    apply(value);
    persist();
    return value;
  }

  // Applies and saves a theme ("" for the system theme).
  function setTheme(theme) {
    const value = THEMES.includes(theme) ? theme : "";
    if (value) {
      root.dataset.theme = value;
    } else {
      delete root.dataset.theme;
    }
    persist();
    return value;
  }

  // Adopts the settings older versions kept in this browser: applies them before the
  // page is drawn, saves them to the profile and then removes them.
  function adopt() {
    let saved = {};
    let theme = null;
    try {
      saved = clean(JSON.parse(localStorage.getItem(KEY)));
      theme = localStorage.getItem(THEME_KEY);
    } catch (e) {
      return;
    }
    apply(saved);
    if (theme && THEMES.includes(theme)) {
      root.dataset.theme = theme;
    }
    send().then((ok) => {
      if (ok) {
        try {
          localStorage.removeItem(KEY);
          localStorage.removeItem(THEME_KEY);
        } catch (e) {
          // Kept: adopted again next time.
        }
      }
    });
  }

  // On a phone-sized screen focus mode is always on (its status line shows the
  // position and the marks; the sidebar opens over it). Elsewhere this browser
  // remembers the choice. The media query is followed after the load too: a browser may
  // apply the viewport meta tag only after this script has run.
  const phone = matchMedia("(max-width: 48rem)");
  let remembered = false;
  try {
    remembered = localStorage.getItem(FOCUS_KEY) === "1";
  } catch (e) {
    // Storage unavailable: no focus mode on a wide screen.
  }

  function applyFocus() {
    root.classList.toggle("reading-focus", phone.matches || remembered);
  }

  function setFocus(on) {
    if (phone.matches) {
      return;
    }
    remembered = on;
    applyFocus();
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

  if ("adopt" in root.dataset) {
    adopt();
  }
  if ("localSettings" in root.dataset) {
    try {
      const saved = JSON.parse(localStorage.getItem(LOCAL_KEY));
      apply(clean(saved));
      if (saved && THEMES.includes(saved.theme) && saved.theme) {
        root.dataset.theme = saved.theme;
      }
    } catch (e) {
      // Nothing saved, or storage unavailable: the defaults.
    }
  }
  applyFocus();
  phone.addEventListener("change", applyFocus);

  return { OPTIONS, THEMES, current, save, setTheme, setFocus, phone };
})();

(() => {
  const root = document.documentElement;
  root.classList.remove("no-js");
  root.classList.add("js");
  try {
    // CSSOM, not a style attribute: allowed by the CSP. app.js clamps it later.
    const width = Number(localStorage.getItem("shelfgauge-sidebar-width"));
    if (width > 0) {
      root.style.setProperty("--sidebar-width", `${width}px`);
    }
  } catch (e) {
    // Storage may be unavailable (private mode): the default width.
  }
})();
