// shelfgauge: applies the saved theme and sidebar width before the page is drawn
// (loaded without `defer`).
"use strict";
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
