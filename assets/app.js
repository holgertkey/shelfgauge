// shelfgauge: small UI comforts. Every page also works without JavaScript.
//
// All URLs come from data attributes rendered by the server, so `base_path` is
// respected. Fetched fragments are our own server-rendered HTML.
"use strict";

const SIDEBAR_KEY = "shelfgauge-sidebar-hidden";
// Set for one reload: the narrow sidebar was open when its mode was switched.
const REOPEN_KEY = "shelfgauge-sidebar-reopen";
const SCROLL_KEY = "shelfgauge-sidebar-scroll";
const MODE_COOKIE = "shelfgauge-sidebar";
const WIDTH_KEY = "shelfgauge-sidebar-width";
const MIN_SIDEBAR_WIDTH = 192; // 12rem
const WIDTH_STEP = 16;

function storageGet(storage, key) {
  try {
    return storage.getItem(key);
  } catch (e) {
    return null;
  }
}

function storageSet(storage, key, value) {
  try {
    if (value === null) {
      storage.removeItem(key);
    } else {
      storage.setItem(key, value);
    }
  } catch (e) {
    // Storage unavailable: the setting is not remembered.
  }
}

// Without a profile (the session ended, or another profile was chosen elsewhere) the
// server answers 401: reloading the page leads to the profile choice.
function reloadIfSignedOut(response) {
  if (response.status === 401) {
    window.location.reload();
  }
}

async function fetchFragment(url) {
  const response = await fetch(url, { headers: { Accept: "text/html" } });
  reloadIfSignedOut(response);
  if (!response.ok) {
    throw new Error(`${url}: ${response.status}`);
  }
  return response.text();
}

// ---------- Theme ----------

// The theme is part of the profile's settings (theme.js saves it).
function applyTheme(theme) {
  const value = shelfgaugeReader.setTheme(theme);
  const select = document.querySelector(".theme-select");
  if (select) {
    select.value = value;
  }
  // The reader's settings panel shows the theme too.
  document.dispatchEvent(new CustomEvent("shelfgauge:theme", { detail: value }));
}

function setupTheme() {
  const select = document.querySelector(".theme-select");
  if (!select) {
    return;
  }
  select.value = document.documentElement.dataset.theme || "";
  select.addEventListener("change", () => applyTheme(select.value));
}

function nextTheme() {
  const themes = shelfgaugeReader.THEMES;
  const current = document.documentElement.dataset.theme || "";
  applyTheme(themes[(themes.indexOf(current) + 1) % themes.length]);
}

// A setting that could not be saved: a short notice at the bottom of the page.
let noticeTimer = null;
function showNotice(message) {
  let el = document.querySelector(".save-notice");
  if (!el) {
    el = document.createElement("div");
    el.className = "save-notice";
    el.setAttribute("role", "status");
    document.body.append(el);
  }
  el.textContent = message;
  el.hidden = false;
  clearTimeout(noticeTimer);
  noticeTimer = setTimeout(() => {
    el.hidden = true;
  }, 4000);
}

function setupSaveNotice() {
  document.addEventListener("shelfgauge:save-failed", () => {
    showNotice("The setting could not be saved to your profile.");
  });
}

// ---------- Profile menu ----------

// The avatar menu is a <details>: it works without JS; here Escape and a click
// elsewhere close it.
function setupProfileMenu() {
  const menu = document.querySelector(".profile-menu");
  if (!menu) {
    return;
  }
  const summary = menu.querySelector("summary");
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && menu.open) {
      menu.open = false;
      summary.focus();
      event.stopPropagation();
    }
  }, true);
  document.addEventListener("click", (event) => {
    if (menu.open && !menu.contains(event.target)) {
      menu.open = false;
    }
  });
  menu.addEventListener("toggle", () => {
    if (menu.open) {
      const first = menu.querySelector(".menu a, .menu button");
      if (first) {
        first.focus();
      }
    }
  });
}

// ---------- Sidebar visibility ----------

const narrow = window.matchMedia("(max-width: 48rem)");

function sidebarVisible() {
  const body = document.body;
  return narrow.matches
    ? body.classList.contains("sidebar-shown")
    : !body.classList.contains("sidebar-hidden");
}

function syncToggle() {
  const toggle = document.querySelector(".sidebar-toggle");
  if (toggle) {
    toggle.setAttribute("aria-expanded", String(sidebarVisible()));
  }
}

function setupSidebarToggle() {
  const toggle = document.querySelector(".sidebar-toggle");
  if (!toggle) {
    return;
  }
  if (storageGet(localStorage, SIDEBAR_KEY) === "1") {
    document.body.classList.add("sidebar-hidden");
  }
  if (storageGet(sessionStorage, REOPEN_KEY) === "1") {
    storageSet(sessionStorage, REOPEN_KEY, null);
    if (narrow.matches) {
      document.body.classList.add("sidebar-shown");
    }
  }
  toggle.addEventListener("click", () => {
    if (narrow.matches) {
      document.body.classList.remove("sidebar-hidden");
      document.body.classList.toggle("sidebar-shown");
    } else {
      const hidden = document.body.classList.toggle("sidebar-hidden");
      storageSet(localStorage, SIDEBAR_KEY, hidden ? "1" : null);
    }
    syncToggle();
  });
  // On a narrow screen the open sidebar lies over the page: a tap beside it (on the
  // dimmed page) closes it and does nothing else.
  document.addEventListener(
    "click",
    (event) => {
      const target = event.target;
      if (
        narrow.matches &&
        document.body.classList.contains("sidebar-shown") &&
        target instanceof Element &&
        target.closest(".layout") &&
        !target.closest(".sidebar")
      ) {
        event.preventDefault();
        event.stopPropagation();
        document.body.classList.remove("sidebar-shown");
        syncToggle();
      }
    },
    true,
  );
  narrow.addEventListener("change", syncToggle);
  syncToggle();
}

// ---------- Sidebar width ----------

function maxSidebarWidth() {
  return Math.max(MIN_SIDEBAR_WIDTH, Math.floor(window.innerWidth / 2));
}

function clampWidth(width) {
  return Math.round(Math.min(Math.max(width, MIN_SIDEBAR_WIDTH), maxSidebarWidth()));
}

function sidebarWidth() {
  const sidebar = document.getElementById("sidebar");
  return sidebar ? sidebar.getBoundingClientRect().width : MIN_SIDEBAR_WIDTH;
}

// Sets the width (`null`: the default from the stylesheet) and updates the ARIA values.
function setSidebarWidth(width, save) {
  const root = document.documentElement;
  if (width === null) {
    root.style.removeProperty("--sidebar-width");
  } else {
    root.style.setProperty("--sidebar-width", `${clampWidth(width)}px`);
  }
  if (save) {
    storageSet(localStorage, WIDTH_KEY, width === null ? null : String(clampWidth(width)));
  }
  const handle = document.querySelector(".sidebar-resizer");
  if (handle) {
    handle.setAttribute("aria-valuemin", String(MIN_SIDEBAR_WIDTH));
    handle.setAttribute("aria-valuemax", String(maxSidebarWidth()));
    handle.setAttribute("aria-valuenow", String(Math.round(sidebarWidth())));
  }
}

function setupResizer() {
  const handle = document.querySelector(".sidebar-resizer");
  const sidebar = document.getElementById("sidebar");
  if (!handle || !sidebar) {
    return;
  }
  const saved = Number(storageGet(localStorage, WIDTH_KEY));
  setSidebarWidth(saved > 0 ? saved : null, false);

  handle.addEventListener("pointerdown", (event) => {
    if (event.button !== 0) {
      return;
    }
    event.preventDefault();
    handle.setPointerCapture(event.pointerId);
    document.body.classList.add("resizing");
    const left = sidebar.getBoundingClientRect().left;
    const move = (e) => setSidebarWidth(e.clientX - left, false);
    const end = (e) => {
      handle.removeEventListener("pointermove", move);
      handle.removeEventListener("pointerup", end);
      handle.removeEventListener("pointercancel", end);
      document.body.classList.remove("resizing");
      setSidebarWidth(e.clientX - left, true);
    };
    handle.addEventListener("pointermove", move);
    handle.addEventListener("pointerup", end);
    handle.addEventListener("pointercancel", end);
  });
  handle.addEventListener("dblclick", () => setSidebarWidth(null, true));
  handle.addEventListener("keydown", (event) => {
    if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") {
      return;
    }
    // Not the reader's previous/next part.
    event.preventDefault();
    event.stopPropagation();
    const step = (event.shiftKey ? 4 : 1) * WIDTH_STEP;
    const delta = event.key === "ArrowLeft" ? -step : step;
    setSidebarWidth(sidebarWidth() + delta, true);
  });
  // Keep the saved width within half of a smaller window (the saved value, not the
  // current one: a hidden sidebar has no width).
  window.addEventListener("resize", () => {
    const width = Number(storageGet(localStorage, WIDTH_KEY));
    if (width > 0) {
      setSidebarWidth(width, false);
    }
  });
}

// ---------- Sidebar tree ----------

async function toggleNode(node) {
  const children = node.querySelector(":scope > .children");
  const twisty = node.querySelector(":scope > .twisty");
  if (!children || !twisty) {
    return;
  }
  if (node.classList.contains("open")) {
    node.classList.remove("open");
    children.hidden = true;
    twisty.setAttribute("aria-expanded", "false");
    return;
  }
  if (!children.firstElementChild && node.dataset.src) {
    node.classList.add("loading");
    try {
      children.innerHTML = await fetchFragment(node.dataset.src);
    } catch (e) {
      console.error(e);
      return;
    } finally {
      node.classList.remove("loading");
    }
  }
  node.classList.add("open");
  children.hidden = false;
  twisty.setAttribute("aria-expanded", "true");
}

async function replaceListing(listing, url) {
  try {
    const html = await fetchFragment(url);
    const template = document.createElement("template");
    template.innerHTML = html.trim();
    const replacement = template.content.firstElementChild;
    if (replacement) {
      listing.replaceWith(replacement);
    }
  } catch (e) {
    console.error(e);
  }
}

function setupTree(sidebar) {
  sidebar.addEventListener("click", (event) => {
    const twisty = event.target.closest(".twisty");
    if (twisty) {
      event.preventDefault();
      toggleNode(twisty.closest(".node"));
      return;
    }
    const pageLink = event.target.closest(".pager a[data-src]");
    if (pageLink && pageLink.dataset.src) {
      event.preventDefault();
      replaceListing(pageLink.closest(".listing"), pageLink.dataset.src);
      return;
    }
    const mode = event.target.closest(".modes a[data-mode]");
    if (mode) {
      event.preventDefault();
      setMode(mode.dataset.mode);
    }
  });
}

function setMode(mode) {
  const sidebar = document.getElementById("sidebar");
  const path = (sidebar && sidebar.dataset.cookiePath) || "/";
  document.cookie =
    `${MODE_COOKIE}=${mode}; path=${path}; max-age=31536000; samesite=lax`;
  storageSet(sessionStorage, SCROLL_KEY, null);
  // The page is rendered again in the new mode: an open narrow sidebar stays open.
  if (narrow.matches && document.body.classList.contains("sidebar-shown")) {
    storageSet(sessionStorage, REOPEN_KEY, "1");
  }
  window.location.reload();
}

function toggleMode() {
  const active = document.querySelector(".modes a.active");
  if (!active) {
    return; // No mode switch on this page (reading mode).
  }
  setMode(active.dataset.mode === "folders" ? "authors" : "folders");
}

function setupFilter(sidebar) {
  const form = sidebar.querySelector(".filter");
  const input = form && form.querySelector("input[data-src]");
  const root = sidebar.querySelector(".tree-root");
  if (!input || !root) {
    return;
  }
  let timer = null;
  let generation = 0;
  const update = async () => {
    const current = ++generation;
    const url = new URL(input.dataset.src, window.location.href);
    if (input.value.trim()) {
      url.searchParams.set("q", input.value.trim());
    }
    try {
      const html = await fetchFragment(url);
      if (current === generation) {
        root.innerHTML = html;
      }
    } catch (e) {
      console.error(e);
    }
  };
  input.addEventListener("input", () => {
    clearTimeout(timer);
    timer = setTimeout(update, 250);
  });
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    clearTimeout(timer);
    update();
  });
}

function setupScroll(sidebar) {
  const active = sidebar.querySelector("a.active");
  if (active) {
    // Scroll only the sidebar: scrollIntoView would also move the page and undo a
    // #b123 position in reading mode.
    const offset = active.getBoundingClientRect().top - sidebar.getBoundingClientRect().top;
    sidebar.scrollTop += offset - sidebar.clientHeight / 2;
  } else {
    const saved = Number(storageGet(sessionStorage, SCROLL_KEY));
    if (saved > 0) {
      sidebar.scrollTop = saved;
    }
  }
  window.addEventListener("pagehide", () => {
    storageSet(sessionStorage, SCROLL_KEY, String(sidebar.scrollTop));
  });
}

// ---------- Scan status ----------

function setupScan() {
  const banner = document.getElementById("scan-banner");
  // The top bar's button and those of the settings page.
  const forms = document.querySelectorAll("form.rescan");
  if (!banner) {
    return;
  }
  const message = banner.querySelector(".message");
  const progress = banner.querySelector("progress");
  let polling = false;

  const show = (status) => {
    banner.hidden = false;
    message.textContent = status.message;
    if (status.running && status.total > 0) {
      progress.max = status.total;
      progress.value = status.done;
      progress.hidden = false;
    } else {
      progress.hidden = true;
    }
    if (!status.running && !banner.querySelector(".reload")) {
      const reload = document.createElement("a");
      reload.className = "reload";
      reload.href = window.location.href;
      reload.textContent = "Reload";
      banner.append(reload);
    }
  };

  const poll = async () => {
    if (polling) {
      return;
    }
    polling = true;
    try {
      for (;;) {
        await new Promise((resolve) => setTimeout(resolve, 1000));
        const response = await fetch(banner.dataset.src, {
          headers: { Accept: "application/json" },
        });
        reloadIfSignedOut(response);
        if (!response.ok) {
          break;
        }
        const status = await response.json();
        show(status);
        if (!status.running) {
          break;
        }
      }
    } catch (e) {
      console.error(e);
    } finally {
      polling = false;
    }
  };

  if (!banner.hidden) {
    poll();
  }
  for (const form of forms) {
    form.addEventListener("submit", async (event) => {
      event.preventDefault();
      try {
        const response = await fetch(form.action, {
          method: "POST",
          headers: { Accept: "application/json" },
        });
        reloadIfSignedOut(response);
        if (!response.ok) {
          throw new Error(`rescan: ${response.status}`);
        }
        const reload = banner.querySelector(".reload");
        if (reload) {
          reload.remove();
        }
        show(await response.json());
        poll();
      } catch (e) {
        console.error(e);
      }
    });
  }
}

// ---------- Settings page: this browser ----------

function setupBrowserSettings() {
  const section = document.querySelector(".browser-settings");
  if (!section) {
    return;
  }
  const done = section.querySelector(".browser-done");
  const say = (text) => {
    done.textContent = text;
  };

  section.querySelector(".reset-sidebar").addEventListener("click", () => {
    storageSet(localStorage, SIDEBAR_KEY, null);
    storageSet(localStorage, WIDTH_KEY, null);
    storageSet(sessionStorage, SCROLL_KEY, null);
    const sidebar = document.getElementById("sidebar");
    const path = (sidebar && sidebar.dataset.cookiePath) || "/";
    document.cookie = `${MODE_COOKIE}=; path=${path}; max-age=0; samesite=lax`;
    // The server renders the sidebar mode: show the default one.
    window.location.reload();
  });

  const leaveFocus = section.querySelector(".leave-focus");
  // theme.js has set the class: the stored choice, or the default of this screen.
  leaveFocus.disabled = !document.documentElement.classList.contains("reading-focus");
  leaveFocus.addEventListener("click", () => {
    shelfgaugeReader.setFocus(false);
    leaveFocus.disabled = true;
    say("Focus mode is off.");
  });
}

// ---------- Notes on the book card ----------

// "Cancel" resets the note (a reset button) and closes its <details>.
function setupNoteForms() {
  document.addEventListener("click", (event) => {
    const cancel = event.target.closest(".note-cancel");
    const details = cancel && cancel.closest("details");
    if (details) {
      details.open = false;
    }
  });
  document.addEventListener("toggle", (event) => {
    const details = event.target;
    if (details instanceof HTMLDetailsElement && details.matches(".note-edit") && details.open) {
      const textarea = details.querySelector("textarea");
      if (textarea) {
        textarea.focus();
        textarea.setSelectionRange(textarea.value.length, textarea.value.length);
      }
    }
  }, true);
}

// ---------- Keyboard ----------

function setupKeys() {
  document.addEventListener("keydown", (event) => {
    if (event.ctrlKey || event.metaKey || event.altKey) {
      return;
    }
    const target = event.target;
    const typing =
      target instanceof HTMLInputElement ||
      target instanceof HTMLTextAreaElement ||
      target instanceof HTMLSelectElement ||
      (target instanceof HTMLElement && target.isContentEditable);
    if (event.key === "Escape") {
      if (typing) {
        target.blur();
      } else if (narrow.matches && document.body.classList.contains("sidebar-shown")) {
        document.body.classList.remove("sidebar-shown");
        syncToggle();
      }
      return;
    }
    if (typing) {
      return;
    }
    const search = document.querySelector(".search input");
    switch (event.key) {
      case "/":
      case "s":
        // In reading mode reader.js takes `/` for the search within the book.
        if (search && !event.defaultPrevented) {
          event.preventDefault();
          search.focus();
          search.select();
        }
        break;
      case "t":
        nextTheme();
        break;
      case "m":
        toggleMode();
        break;
      case "ArrowLeft":
      case "ArrowRight": {
        // Reading mode: previous / next part.
        const rel = event.key === "ArrowLeft" ? "prev" : "next";
        const link = document.querySelector(`.reader-nav a[rel="${rel}"]`);
        if (link) {
          event.preventDefault();
          window.location.href = link.href;
        }
        break;
      }
      default:
    }
  });
}

document.addEventListener("DOMContentLoaded", () => {
  setupTheme();
  setupSaveNotice();
  setupProfileMenu();
  setupSidebarToggle();
  setupResizer();
  const sidebar = document.getElementById("sidebar");
  if (sidebar) {
    setupTree(sidebar);
    setupFilter(sidebar);
    setupScroll(sidebar);
  }
  setupScan();
  setupBrowserSettings();
  setupNoteForms();
  setupKeys();
});
