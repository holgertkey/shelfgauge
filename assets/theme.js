// shelfgauge: applies the saved theme before the page is drawn (loaded without `defer`).
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
  } catch (e) {
    // Storage may be unavailable (private mode); the system theme is used.
  }
})();
