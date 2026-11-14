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
    font: ["sans", "serif", "literata", "pt-serif", "merriweather", "lora", "inter", "jetbrains-mono"],
    size: ["3", "0", "1", "2", "4", "5", "6", "7"],
    leading: ["4", "0", "1", "2", "3", "5", "6", "7"],
    width: ["3", "0", "1", "2", "4", "5", "6", "7"],
    align: ["left", "justify"],
    // Focus mode's running head and status line.
    status: ["on", "off"],
    // Minutes the screen stays on after the last page turn (reader.js).
    awake: ["10", "off", "5", "30"],
  };
  const THEMES = ["", "light", "sepia", "mist", "dusk", "dark", "navy", "forest", "wine", "custom"];
  // The Custom theme's background and accent (`data-cbg`, `data-caccent`; the same
  // as `COLOR_SETTINGS` in src/profiles/mod.rs), kept with any theme. palette.js
  // derives the rest of the theme from them.
  const COLORS = ["cbg", "caccent"];
  const HEX = /^#[0-9a-f]{6}$/;
  // Auto's day and night themes (`data-tlight`, `data-tdark`), the default first; the
  // same as `tlight` and `tdark` in `SETTINGS` of src/profiles/mod.rs.
  const AUTO = {
    tlight: ["light", "sepia", "mist", "custom"],
    tdark: ["dark", "dusk", "navy", "forest", "wine", "custom"],
  };
  // With Auto chosen, `data-auto` is set and `data-theme` is the theme shown: the
  // night's under a dark system setting, else the day's (without JavaScript,
  // style.css follows the system setting with Light and Dark).
  const night = matchMedia("(prefers-color-scheme: dark)");
  const ATTRS = {
    font: "rfont",
    size: "rsize",
    leading: "rleading",
    width: "rwidth",
    align: "ralign",
    status: "rstatus",
    awake: "rawake",
  };
  const root = document.documentElement;
  const SAVE_DELAY = 400;
  let timer = null;
  // The custom properties set on <html> for the Custom theme, and what palette.js
  // derived (the colors in use and what it corrected).
  let customVars = [];
  let customPalette = null;

  // The values of earlier versions that were replaced (the steps of line spacing and
  // width, the themes that were removed); the same as `RENAMED_VALUES` in
  // src/profiles/mod.rs.
  const RENAMED = {
    theme: { stone: "mist", black: "dark" },
    leading: { tight: "2", normal: "4", loose: "6" },
    width: { narrow: "1", medium: "3", wide: "5", full: "7" },
  };

  // Only known keys with allowed values (a value of an earlier version as its step);
  // anything else (broken data) is dropped.
  function clean(value) {
    const settings = {};
    if (value && typeof value === "object") {
      for (const name of Object.keys(OPTIONS)) {
        let v = String(value[name]);
        v = (RENAMED[name] && RENAMED[name][v]) || v;
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
    const body = Object.assign(current(), colors(), pair(), { theme: choice() });
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

  // A theme kept by this browser as it is now: a removed one as the closest one left,
  // an unknown one as "" (the system theme).
  function adoptedTheme(theme) {
    const value = (theme && RENAMED.theme[theme]) || theme;
    return THEMES.includes(value) ? value : "";
  }

  // The Custom theme's colors that are set, by setting name.
  function colors() {
    const value = {};
    for (const name of COLORS) {
      const color = root.dataset[name];
      if (color && HEX.test(color)) {
        value[name] = color;
      }
    }
    return value;
  }

  // Sets the Custom theme's colors on <html> (invalid ones are dropped).
  function putColors(value) {
    for (const name of COLORS) {
      const color = value && typeof value[name] === "string" ? value[name].toLowerCase() : "";
      if (HEX.test(color)) {
        root.dataset[name] = color;
      } else {
        delete root.dataset[name];
      }
    }
  }

  // The theme chosen: "" for Auto, whichever theme it shows.
  function choice() {
    return "auto" in root.dataset ? "" : root.dataset.theme || "";
  }

  // Auto's day and night themes that are not the defaults, by setting name.
  function pair() {
    const value = {};
    for (const [name, allowed] of Object.entries(AUTO)) {
      const theme = root.dataset[name];
      if (allowed.includes(theme) && theme !== allowed[0]) {
        value[name] = theme;
      }
    }
    return value;
  }

  // Sets Auto's day and night themes on <html> (unknown ones and the defaults are
  // dropped).
  function putPair(value) {
    for (const [name, allowed] of Object.entries(AUTO)) {
      const theme = value ? value[name] : undefined;
      if (allowed.includes(theme) && theme !== allowed[0]) {
        root.dataset[name] = theme;
      } else {
        delete root.dataset[name];
      }
    }
  }

  // The scheme of the Custom theme's colors (`light` or `dark`).
  function customScheme() {
    if (typeof shelfgaugePalette === "undefined") {
      return null;
    }
    return shelfgaugePalette.derive(root.dataset.cbg, root.dataset.caccent).scheme;
  }

  // The theme Auto shows now: the night's under a dark system setting, else the
  // day's. Custom only on the side of its scheme; on the other one (its colors
  // changed since it was chosen), that side's default.
  function autoTheme() {
    const side = night.matches ? "tdark" : "tlight";
    const theme = AUTO[side].includes(root.dataset[side]) ? root.dataset[side] : AUTO[side][0];
    const scheme = theme === "custom" ? customScheme() : null;
    if (scheme && scheme !== (side === "tdark" ? "dark" : "light")) {
      return AUTO[side][0];
    }
    return theme;
  }

  // Shows the theme chosen: with Auto, the day's or the night's; then the Custom
  // palette when that is the theme shown.
  function show() {
    if ("auto" in root.dataset) {
      root.dataset.theme = autoTheme();
    }
    applyCustom();
  }

  // Applies the Custom theme's palette when it is the theme shown, and removes it
  // otherwise. `data-cscheme` (light or dark) follows, as the server renders it for
  // pages without JavaScript.
  function applyCustom() {
    for (const name of customVars) {
      root.style.removeProperty(name);
    }
    customVars = [];
    customPalette = null;
    if (root.dataset.theme !== "custom" || typeof shelfgaugePalette === "undefined") {
      delete root.dataset.cscheme;
      return null;
    }
    // CSSOM, not a style attribute: allowed by the CSP.
    customPalette = shelfgaugePalette.derive(root.dataset.cbg, root.dataset.caccent);
    for (const [name, value] of Object.entries(customPalette.vars)) {
      root.style.setProperty(name, value);
      customVars.push(name);
    }
    root.style.setProperty("color-scheme", customPalette.scheme);
    customVars.push("color-scheme");
    root.dataset.cscheme = customPalette.scheme;
    return customPalette;
  }

  // The background and the accent shown now, as `#rrggbb` (null when the browser
  // gives them otherwise).
  function shownColors() {
    const style = getComputedStyle(root);
    const bg = style.getPropertyValue("--bg").trim().toLowerCase();
    const accent = style.getPropertyValue("--accent").trim().toLowerCase();
    return HEX.test(bg) && HEX.test(accent) ? { cbg: bg, caccent: accent } : null;
  }

  // Applies and saves a theme ("" for the system theme). Custom chosen without its
  // colors starts from the theme shown now, so nothing jumps.
  function setTheme(theme) {
    const value = THEMES.includes(theme) ? theme : "";
    if (value === "custom" && Object.keys(colors()).length < COLORS.length) {
      putColors(shownColors() || { cbg: "#ffffff", caccent: "#0969da" });
    }
    if (value) {
      root.dataset.theme = value;
      delete root.dataset.auto;
    } else {
      root.dataset.auto = "";
    }
    show();
    persist();
    return value;
  }

  // Applies and saves Auto's day and night themes (`{ tlight, tdark }`); returns the
  // ones that are not the defaults.
  function setAuto(value) {
    putPair(Object.assign(pair(), value));
    show();
    persist();
    return pair();
  }

  // Applies and saves the Custom theme's colors (`{ cbg, caccent }`, `#rrggbb`);
  // returns what palette.js derived, or null when Custom is not the theme.
  function setColors(value) {
    putColors(Object.assign(colors(), value));
    persist();
    // Auto may show another side's theme now (Custom moved to the other scheme).
    show();
    return customPalette;
  }

  // What palette.js derived for the Custom theme shown now (`{ vars, scheme, bg,
  // accent, notes }`), or null.
  function custom() {
    return customPalette;
  }

  // Adopts the settings older versions kept in this browser: applies them before the
  // page is drawn, saves them to the profile and then removes them.
  function adopt() {
    let saved = {};
    let theme = null;
    try {
      saved = clean(JSON.parse(localStorage.getItem(KEY)));
      theme = adoptedTheme(localStorage.getItem(THEME_KEY));
    } catch (e) {
      return;
    }
    apply(saved);
    if (theme) {
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
      if (saved && typeof saved === "object") {
        putColors(saved);
        putPair(saved);
      }
      const theme = adoptedTheme(saved && saved.theme);
      if (theme) {
        root.dataset.theme = theme;
      }
    } catch (e) {
      // Nothing saved, or storage unavailable: the defaults.
    }
  }
  if (!root.dataset.theme) {
    root.dataset.auto = "";
  }
  show();
  // A phone turning to night mode while a book is open.
  night.addEventListener("change", () => {
    if ("auto" in root.dataset) {
      show();
      // The reader's settings panel shows Custom's colors while it is shown.
      document.dispatchEvent(new CustomEvent("shelfgauge:theme", { detail: "" }));
    }
  });
  applyFocus();
  phone.addEventListener("change", applyFocus);

  return {
    OPTIONS,
    THEMES,
    AUTO,
    current,
    save,
    choice,
    setTheme,
    setAuto,
    setColors,
    custom,
    customScheme,
    setFocus,
    phone,
  };
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
