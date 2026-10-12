// shelfgauge: small UI comforts. The pages work without JavaScript.
"use strict";

document.addEventListener("DOMContentLoaded", () => {
  const toggle = document.querySelector(".sidebar-toggle");
  if (!toggle) {
    return;
  }
  const narrow = window.matchMedia("(max-width: 40rem)");
  const visible = () =>
    narrow.matches
      ? document.body.classList.contains("sidebar-shown")
      : !document.body.classList.contains("sidebar-hidden");
  const sync = () => toggle.setAttribute("aria-expanded", String(visible()));
  toggle.addEventListener("click", () => {
    document.body.classList.toggle(narrow.matches ? "sidebar-shown" : "sidebar-hidden");
    sync();
  });
  narrow.addEventListener("change", sync);
  sync();
});
