// shelfgauge reader: bookmarks, marks (selected text with a note), highlights and the
// reading position. Loaded on read pages only; the page works without it (the lists
// are rendered by the server, deleting works with plain forms).
//
// Anchors count Unicode code points into a block's text (the `textContent` of the
// element `id="b<n>"`); JavaScript strings count UTF-16 units, so offsets are
// converted at the edges. The server derives the quote from the anchor itself.
"use strict";

(() => {
  const TAB_KEY = "shelfgauge-reader-tab";
  // Reading position: save after scrolling has stopped for this long...
  const SCROLL_IDLE_MS = 2500;
  // ...and only for a place the reader has stayed at for this long (stray jumps to a
  // footnote or another chapter do not move the position).
  const DWELL_MS = 6000;
  const BLOCK_ID = /^b[1-9][0-9]*$/;

  const article = document.querySelector("article.reader");
  const text = article && article.querySelector(".text");
  if (!article || !text) {
    return;
  }
  const createUrl = article.dataset.annotations;
  const positionUrl = article.dataset.position;
  const maxText = Number(article.dataset.maxText) || Infinity;
  const maxNote = Number(article.dataset.maxNote) || Infinity;

  function storageGet(key) {
    try {
      return sessionStorage.getItem(key);
    } catch (e) {
      return null;
    }
  }

  function storageSet(key, value) {
    try {
      sessionStorage.setItem(key, value);
    } catch (e) {
      // Not remembered.
    }
  }

  function cpLength(s) {
    let n = 0;
    for (const _ of s) {
      n += 1;
    }
    return n;
  }

  // The UTF-16 index of code point `cp` in `s` (its length if past the end).
  function utf16Index(s, cp) {
    let i = 0;
    let n = 0;
    while (i < s.length && n < cp) {
      const c = s.charCodeAt(i);
      i += c >= 0xd800 && c <= 0xdbff && i + 1 < s.length ? 2 : 1;
      n += 1;
    }
    return i;
  }

  // ---------- Blocks ----------

  // The leaf blocks of the part in document order: block elements without nested
  // blocks. Only ids inside `.text` count (the page has other ids).
  let leafCache = null;
  function leaves() {
    leafCache =
      leafCache ||
      Array.from(text.querySelectorAll("[id]")).filter(
        (el) =>
          BLOCK_ID.test(el.id) &&
          !Array.from(el.querySelectorAll("[id]")).some((c) => BLOCK_ID.test(c.id)),
      );
    return leafCache;
  }

  function blockText(el) {
    return el.textContent;
  }

  // The code point offset of a boundary point inside `block`.
  function cpOffsetIn(block, node, offset) {
    const range = document.createRange();
    range.setStart(block, 0);
    range.setEnd(node, offset);
    return cpLength(range.toString());
  }

  // A DOM point for code point `cp` of `block`'s text.
  function domPoint(block, cp) {
    const walker = document.createTreeWalker(block, NodeFilter.SHOW_TEXT);
    let remaining = cp;
    let last = null;
    for (let node = walker.nextNode(); node; node = walker.nextNode()) {
      const len = cpLength(node.data);
      if (remaining <= len) {
        return { node, offset: utf16Index(node.data, remaining) };
      }
      remaining -= len;
      last = node;
    }
    return last ? { node: last, offset: last.data.length } : { node: block, offset: 0 };
  }

  // The anchor of a DOM range: its ends moved into leaf blocks.
  function anchorOf(range) {
    const all = leaves();
    let start = null;
    let end = null;
    for (const leaf of all) {
      const r = document.createRange();
      r.selectNodeContents(leaf);
      if (!start) {
        const cmp = r.comparePoint(range.startContainer, range.startOffset);
        if (cmp === 0) {
          start = { block: leaf, offset: cpOffsetIn(leaf, range.startContainer, range.startOffset) };
        } else if (cmp < 0) {
          start = { block: leaf, offset: 0 };
        }
      }
      const cmpEnd = r.comparePoint(range.endContainer, range.endOffset);
      if (cmpEnd === 0) {
        end = { block: leaf, offset: cpOffsetIn(leaf, range.endContainer, range.endOffset) };
      } else if (cmpEnd > 0) {
        end = { block: leaf, offset: cpLength(blockText(leaf)) };
      }
    }
    if (!start || !end) {
      return null;
    }
    const order = all.indexOf(end.block) - all.indexOf(start.block);
    if (order < 0 || (order === 0 && end.offset <= start.offset)) {
      return null;
    }
    return { start, end };
  }

  // The top of the readable area: below the top bar, or below the running head while
  // focus mode hides the bar (its box may still be sliding away).
  function topLimit() {
    const root = document.documentElement;
    const top = document.querySelector(".topbar");
    if (
      !top ||
      (root.classList.contains("reading-focus") && !root.classList.contains("topbar-peek"))
    ) {
      const head = document.querySelector(".running-head");
      return (head ? Math.max(head.getBoundingClientRect().bottom, 0) : 0) + 4;
    }
    return Math.max(top.getBoundingClientRect().bottom, 0) + 4;
  }

  // The topmost leaf block that is (at least partly) visible below the top bar.
  function topBlock() {
    const limit = topLimit();
    const all = leaves();
    // Blocks are in document order, so their bottoms grow: binary search for the
    // first one below the limit.
    let lo = 0;
    let hi = all.length;
    while (lo < hi) {
      const mid = (lo + hi) >> 1;
      if (all[mid].getBoundingClientRect().bottom > limit) {
        hi = mid;
      } else {
        lo = mid + 1;
      }
    }
    // Skip blocks without a box or without text (an empty line, an uncaptioned
    // image): a bookmark or position there could not be found again.
    while (
      lo < all.length &&
      (all[lo].getBoundingClientRect().height === 0 || !all[lo].textContent.trim())
    ) {
      lo += 1;
    }
    return all[lo] || null;
  }

  // ---------- Requests ----------

  async function postJson(url, body, keepalive) {
    const response = await fetch(url, {
      method: "POST",
      headers: { "Content-Type": "application/json", Accept: "application/json" },
      body: JSON.stringify(body),
      keepalive: Boolean(keepalive),
    });
    if (!response.ok) {
      let message = `${response.status}`;
      try {
        message = (await response.json()).error || message;
      } catch (e) {
        // Not JSON.
      }
      throw new Error(message);
    }
    return response.status === 204 ? null : response.json();
  }

  // ---------- Toast ----------

  let toastTimer = null;
  function toast(message) {
    let el = document.querySelector(".reader-toast");
    if (!el) {
      el = document.createElement("div");
      el.className = "reader-toast";
      el.setAttribute("role", "status");
      document.body.append(el);
    }
    el.textContent = message;
    el.hidden = false;
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => {
      el.hidden = true;
    }, 2500);
  }

  // ---------- Sidebar tabs and lists ----------

  const sidebar = document.getElementById("sidebar");

  function showTab(name) {
    if (!sidebar) {
      return;
    }
    const panels = sidebar.querySelectorAll(".tab-panel");
    if (!Array.from(panels).some((p) => p.dataset.tab === name)) {
      name = "toc";
    }
    for (const panel of panels) {
      panel.classList.toggle("active", panel.dataset.tab === name);
    }
    for (const tab of sidebar.querySelectorAll(".reader-tabs a")) {
      const active = tab.dataset.tab === name;
      tab.classList.toggle("active", active);
      tab.setAttribute("aria-current", active ? "true" : "false");
    }
    storageSet(TAB_KEY, name);
  }

  async function refreshLists() {
    const lists = sidebar && sidebar.querySelector(".annotation-lists");
    if (!lists || !createUrl) {
      return;
    }
    const response = await fetch(createUrl, { headers: { Accept: "text/html" } });
    if (!response.ok) {
      throw new Error(`${createUrl}: ${response.status}`);
    }
    const template = document.createElement("template");
    template.innerHTML = (await response.text()).trim();
    const replacement = template.content.firstElementChild;
    if (replacement) {
      lists.replaceWith(replacement);
    }
    showTab(storageGet(TAB_KEY) || "toc");
    highlight();
    drawBookmarks();
  }

  function setupSidebar() {
    if (!sidebar) {
      return;
    }
    sidebar.classList.add("tabbed");
    showTab(storageGet(TAB_KEY) || "toc");
    sidebar.addEventListener("click", (event) => {
      const tab = event.target.closest(".reader-tabs a[data-tab]");
      if (tab) {
        event.preventDefault();
        showTab(tab.dataset.tab);
        return;
      }
      if (event.target.closest(".add-bookmark")) {
        addBookmark();
        return;
      }
      if (event.target.closest("[data-mark-link]")) {
        // A jump to a bookmark or mark never moves the reading position.
        suspendPosition();
      }
    });
    sidebar.addEventListener("submit", async (event) => {
      const form = event.target.closest(".delete-form");
      if (!form) {
        return;
      }
      event.preventDefault();
      try {
        await postJson(form.action, {});
        await refreshLists();
        toast("Deleted.");
      } catch (e) {
        toast(`Could not delete: ${e.message}`);
      }
    });
  }

  // ---------- Highlights ----------

  const canHighlight = typeof CSS !== "undefined" && CSS.highlights && typeof Highlight === "function";

  // The flat text of the part (leaf texts joined with "\n") and where each leaf
  // starts in it (UTF-16 units), for finding a quote whose offsets no longer fit.
  function flatText(all) {
    const starts = [];
    let flat = "";
    all.forEach((leaf, i) => {
      if (i > 0) {
        flat += "\n";
      }
      starts.push(flat.length);
      flat += blockText(leaf);
    });
    return { flat, starts };
  }

  function rangeAt(all, flat, starts, from, to) {
    const locate = (pos) => {
      let i = starts.length - 1;
      while (i > 0 && starts[i] > pos) {
        i -= 1;
      }
      const leaf = all[i];
      const local = Math.min(pos - starts[i], blockText(leaf).length);
      return domPoint(leaf, cpLength(blockText(leaf).slice(0, local)));
    };
    const a = locate(from);
    const b = locate(to);
    const range = document.createRange();
    range.setStart(a.node, a.offset);
    range.setEnd(b.node, b.offset);
    return range;
  }

  // The range of a saved mark in this part, re-found by its text if needed.
  function markRange(item, all, flatInfo) {
    const startEl = document.getElementById(item.dataset.startBlock);
    const endEl = document.getElementById(item.dataset.endBlock);
    const quote = item.dataset.text || "";
    if (!quote) {
      return null;
    }
    const { flat, starts } = flatInfo();
    if (startEl && endEl && text.contains(startEl) && text.contains(endEl)) {
      const si = all.indexOf(startEl);
      const ei = all.indexOf(endEl);
      if (si >= 0 && ei >= si) {
        const from = starts[si] + utf16Index(blockText(startEl), Number(item.dataset.startOffset));
        const to = starts[ei] + utf16Index(blockText(endEl), Number(item.dataset.endOffset));
        if (flat.slice(from, to) === quote) {
          return rangeAt(all, flat, starts, from, to);
        }
      }
    }
    // The rendering changed: look for prefix + text + suffix in this part.
    const needle = (item.dataset.prefix || "") + quote + (item.dataset.suffix || "");
    const at = flat.indexOf(needle);
    if (at < 0) {
      return null;
    }
    const from = at + (item.dataset.prefix || "").length;
    return rangeAt(all, flat, starts, from, from + quote.length);
  }

  function clearFallbackMarks() {
    for (const mark of text.querySelectorAll("mark.sg-mark")) {
      mark.replaceWith(...mark.childNodes);
    }
    text.normalize();
  }

  // Without the Custom Highlight API: wrap the text nodes of a range in <mark>
  // (textContent and the offsets stay the same).
  function wrapRange(range) {
    const walker = document.createTreeWalker(range.commonAncestorContainer, NodeFilter.SHOW_TEXT);
    const nodes = [];
    for (let node = walker.currentNode; node; node = walker.nextNode()) {
      if (node.nodeType === Node.TEXT_NODE && range.intersectsNode(node)) {
        nodes.push(node);
      }
    }
    for (let node of nodes) {
      const start = node === range.startContainer ? range.startOffset : 0;
      const end = node === range.endContainer ? range.endOffset : node.data.length;
      if (end <= start) {
        continue;
      }
      if (end < node.data.length) {
        node.splitText(end);
      }
      if (start > 0) {
        node = node.splitText(start);
      }
      const mark = document.createElement("mark");
      mark.className = "sg-mark";
      node.replaceWith(mark);
      mark.append(node);
    }
  }

  function highlight() {
    const items = sidebar ? Array.from(sidebar.querySelectorAll(".mark-item:not([data-lost])")) : [];
    if (!canHighlight) {
      clearFallbackMarks();
    }
    const all = leaves();
    let cached = null;
    const flatInfo = () => {
      cached = cached || flatText(all);
      return cached;
    };
    const ranges = [];
    for (const item of items) {
      try {
        const range = markRange(item, all, flatInfo);
        if (range) {
          ranges.push(range);
        }
      } catch (e) {
        console.error(e);
      }
    }
    if (canHighlight) {
      CSS.highlights.set("sg-mark", new Highlight(...ranges));
    } else {
      // Wrap from the end so earlier ranges stay valid.
      for (const range of ranges.reverse()) {
        wrapRange(range);
      }
    }
  }

  // ---------- Creating marks ----------

  let popup = null;
  let pending = null;

  function hidePopup() {
    if (popup) {
      popup.remove();
      popup = null;
    }
    pending = null;
  }

  function place(el, rect) {
    const top = window.scrollY + rect.bottom + 8;
    const left = Math.max(8, Math.min(window.scrollX + rect.left, window.scrollX + window.innerWidth - el.offsetWidth - 8));
    el.style.top = `${top}px`;
    el.style.left = `${left}px`;
  }

  function showMarkButton(range, anchor) {
    hidePopup();
    pending = anchor;
    popup = document.createElement("div");
    popup.className = "mark-popup";
    const button = document.createElement("button");
    button.type = "button";
    button.className = "mark-button";
    button.textContent = "Quote";
    button.title = "Keep this text as a quote (with a note if you like)";
    // Keep the selection: a mousedown on the button would collapse it.
    button.addEventListener("mousedown", (e) => e.preventDefault());
    button.addEventListener("click", () => showNoteForm(range));
    popup.append(button);
    document.body.append(popup);
    place(popup, range.getBoundingClientRect());
  }

  function showNoteForm(range) {
    if (!popup || !pending) {
      return;
    }
    const anchor = pending;
    const length = cpLength(range.toString());
    popup.textContent = "";
    popup.classList.add("form");
    const form = document.createElement("form");
    const info = document.createElement("p");
    info.className = "muted";
    const tooLong = length > maxText;
    info.textContent = tooLong
      ? `The selection is ${length} characters long; the limit is ${maxText}.`
      : `${length} characters selected.`;
    const note = document.createElement("textarea");
    note.rows = 3;
    note.placeholder = "Note (optional)";
    note.setAttribute("aria-label", "Note");
    const actions = document.createElement("div");
    actions.className = "actions";
    const save = document.createElement("button");
    save.type = "submit";
    save.textContent = "Save";
    save.disabled = tooLong;
    const cancel = document.createElement("button");
    cancel.type = "button";
    cancel.textContent = "Cancel";
    cancel.addEventListener("click", hidePopup);
    actions.append(save, cancel);
    form.append(info, note, actions);
    popup.append(form);
    note.focus();
    form.addEventListener("keydown", (e) => {
      if (e.key === "Escape") {
        hidePopup();
      } else if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
        form.requestSubmit();
      }
    });
    form.addEventListener("submit", async (event) => {
      event.preventDefault();
      if (cpLength(note.value) > maxNote) {
        info.textContent = `The note is too long; the limit is ${maxNote} characters.`;
        return;
      }
      save.disabled = true;
      try {
        await postJson(createUrl, {
          start_block: anchor.start.block.id,
          start_offset: anchor.start.offset,
          end_block: anchor.end.block.id,
          end_offset: anchor.end.offset,
          note: note.value,
        });
        hidePopup();
        const selection = window.getSelection();
        if (selection) {
          selection.removeAllRanges();
        }
        await refreshLists();
        toast("Quote saved.");
      } catch (e) {
        info.textContent = `Could not save: ${e.message}`;
        save.disabled = false;
      }
    });
  }

  function onSelection() {
    const selection = window.getSelection();
    if (!selection || selection.isCollapsed || selection.rangeCount === 0) {
      if (popup && !popup.classList.contains("form")) {
        hidePopup();
      }
      return;
    }
    const range = selection.getRangeAt(0);
    if (!text.contains(range.commonAncestorContainer) && range.commonAncestorContainer !== text) {
      return;
    }
    if (popup && popup.classList.contains("form")) {
      return;
    }
    const anchor = anchorOf(range);
    if (anchor) {
      showMarkButton(range, anchor);
    }
  }

  function setupMarks() {
    if (!createUrl) {
      return;
    }
    let timer = null;
    document.addEventListener("selectionchange", () => {
      clearTimeout(timer);
      timer = setTimeout(onSelection, 250);
    });
    document.addEventListener("mousedown", (event) => {
      if (popup && !popup.contains(event.target) && popup.classList.contains("form")) {
        hidePopup();
      }
    });
  }

  // ---------- Bookmarks ----------

  async function addBookmark() {
    const block = topBlock();
    if (!block || !createUrl) {
      return;
    }
    try {
      await postJson(createUrl, { block: block.id });
      await refreshLists();
      toast("Bookmark saved.");
    } catch (e) {
      toast(`Could not save the bookmark: ${e.message}`);
    }
  }

  function setupBookmarks() {
    const button = document.querySelector(".bookmark-button");
    if (button && createUrl) {
      button.hidden = false;
      button.addEventListener("click", addBookmark);
    }
    document.addEventListener("keydown", (event) => {
      if (event.ctrlKey || event.metaKey || event.altKey || event.key !== "b") {
        return;
      }
      const target = event.target;
      const typing =
        target instanceof HTMLInputElement ||
        target instanceof HTMLTextAreaElement ||
        target instanceof HTMLSelectElement ||
        (target instanceof HTMLElement && target.isContentEditable);
      if (!typing) {
        event.preventDefault();
        addBookmark();
      }
    });
  }

  // ---------- Reading position ----------

  // A page opened from a bookmark or quote (`from=mark`, `from=quote`) never saves
  // the position; prev/next links do not carry it, so reading on resumes saving.
  const openedFrom = new URLSearchParams(window.location.search).get("from");
  let suspended = openedFrom === "mark" || openedFrom === "quote";
  let candidate = null;
  let since = 0;
  let lastSaved = null;
  let timer = null;

  function suspendPosition() {
    suspended = true;
    clearTimeout(timer);
  }

  function observe() {
    const block = topBlock();
    if (block !== candidate) {
      candidate = block;
      since = Date.now();
    }
  }

  function save(keepalive) {
    if (suspended || !positionUrl || !candidate) {
      return;
    }
    // Never a position inside notes.
    if (candidate.closest(".notes") || candidate.id === lastSaved) {
      return;
    }
    lastSaved = candidate.id;
    postJson(positionUrl, { block: candidate.id }, keepalive).catch((e) => console.error(e));
  }

  function trySave() {
    observe();
    if (!candidate) {
      return;
    }
    const wait = DWELL_MS - (Date.now() - since);
    if (wait > 0) {
      timer = setTimeout(trySave, wait);
      return;
    }
    save(false);
  }

  function setupPosition() {
    if (!positionUrl) {
      return;
    }
    observe();
    timer = setTimeout(trySave, DWELL_MS);
    window.addEventListener(
      "scroll",
      () => {
        if (suspended) {
          return;
        }
        observe();
        clearTimeout(timer);
        timer = setTimeout(trySave, SCROLL_IDLE_MS);
      },
      { passive: true },
    );
    const leave = () => {
      observe();
      if (Date.now() - since >= DWELL_MS) {
        save(true);
      }
    };
    window.addEventListener("pagehide", leave);
    document.addEventListener("visibilitychange", () => {
      if (document.visibilityState === "hidden") {
        leave();
      }
    });
  }

  // Late images above the target block shift the layout: scroll to the block again
  // unless the reader has scrolled meanwhile.
  function setupRescroll() {
    const id = decodeURIComponent(window.location.hash.slice(1));
    const target = BLOCK_ID.test(id) && document.getElementById(id);
    if (!target) {
      return;
    }
    let moved = false;
    const stop = () => {
      moved = true;
    };
    for (const type of ["wheel", "touchstart", "keydown", "mousedown"]) {
      window.addEventListener(type, stop, { once: true, passive: true });
    }
    for (const img of text.querySelectorAll("img")) {
      if (!img.complete) {
        img.addEventListener("load", () => {
          if (!moved) {
            target.scrollIntoView();
          }
        });
      }
    }
  }

  // ---------- Reader settings and focus mode ----------

  // Runs `change` (which reflows the text) and scrolls back to the same place: the
  // block at the top of the screen, at the same share of its height (or at the same
  // distance below the top bar when it starts below it).
  function keepPlace(change) {
    const block = topBlock();
    let share = null;
    let offset = 0;
    if (block) {
      const limit = topLimit();
      const rect = block.getBoundingClientRect();
      if (rect.top < limit && rect.height > 0) {
        share = (limit - rect.top) / rect.height;
      } else {
        offset = rect.top - limit;
      }
    }
    change();
    if (block) {
      const rect = block.getBoundingClientRect();
      const target = share === null ? rect.top - offset : rect.top + share * rect.height;
      window.scrollBy({ top: target - topLimit(), behavior: "instant" });
    }
  }

  function setupSettings() {
    // `shelfgaugeReader` comes from theme.js (a global of a classic script).
    if (typeof shelfgaugeReader === "undefined") {
      return;
    }
    const settings = shelfgaugeReader;
    const root = document.documentElement;
    const panel = document.getElementById("reader-settings");
    const form = panel && panel.querySelector("form");
    const settingsButton = document.querySelector(".settings-button");
    const focusButton = document.querySelector(".focus-button");
    const sizes = settings.OPTIONS.size.map(Number).sort((a, b) => a - b);
    const defaultSize = Number(settings.OPTIONS.size[0]);
    let closedAt = 0;
    let peekByMouse = false;

    const focused = () => root.classList.contains("reading-focus");
    const panelOpen = () => Boolean(panel) && panel.matches(":popover-open");

    function sync() {
      if (focusButton) {
        focusButton.setAttribute("aria-pressed", String(focused()));
      }
      if (!form) {
        return;
      }
      const current = settings.current();
      current.theme = root.dataset.theme || "";
      for (const [name, value] of Object.entries(current)) {
        for (const input of form.querySelectorAll(`input[name="${name}"]`)) {
          input.checked = input.value === value;
        }
      }
      form.elements.focus.checked = focused();
      const size = Number(current.size);
      form.querySelector('[data-step="-1"]').disabled = size === sizes[0];
      form.querySelector('[data-step="1"]').disabled = size === sizes[sizes.length - 1];
      form.querySelector(".size-value").textContent =
        `${Math.round(parseFloat(getComputedStyle(text).fontSize))}px`;
    }

    function update(name, value) {
      keepPlace(() => {
        const current = settings.current();
        current[name] = value;
        settings.save(current);
      });
      sync();
      updateStatus();
    }

    function stepSize(delta) {
      const index = sizes.indexOf(Number(settings.current().size)) + delta;
      if (index >= 0 && index < sizes.length) {
        update("size", String(sizes[index]));
      }
    }

    function peek(on) {
      root.classList.toggle("topbar-peek", on);
      if (!on) {
        peekByMouse = false;
      }
    }

    function setFocus(on) {
      keepPlace(() => settings.setFocus(on));
      // Toggled from the open panel: the top bar it hangs from stays.
      peek(panelOpen());
      sync();
      updateStatus();
    }

    if (focusButton) {
      focusButton.hidden = false;
      focusButton.addEventListener("click", () => setFocus(!focused()));
    }
    // The panel needs the Popover API; without it (the panel would show inline) only
    // the keys work.
    if (panel && !("showPopover" in panel)) {
      panel.remove();
    } else if (form && settingsButton) {
      settingsButton.hidden = false;
      form.addEventListener("submit", (event) => event.preventDefault());
      form.addEventListener("change", (event) => {
        const input = event.target;
        if (input.name === "theme") {
          // app.js: remembers it and updates the top-bar select.
          applyTheme(input.value);
        } else if (input.name === "focus") {
          setFocus(input.checked);
        } else if (input.type === "checkbox") {
          update(input.name, input.checked ? "on" : "off");
        } else if (input.name in settings.OPTIONS) {
          update(input.name, input.value);
        }
      });
      form.addEventListener("click", (event) => {
        const step = event.target.closest("[data-step]");
        if (step) {
          stepSize(Number(step.dataset.step));
        } else if (event.target.closest(".settings-reset")) {
          keepPlace(() => settings.save({}));
          sync();
          updateStatus();
        }
      });
      panel.addEventListener("toggle", (event) => {
        if (event.newState === "open") {
          sync();
        } else {
          closedAt = Date.now();
        }
        // Keep the top bar (and the button the panel hangs from) in focus mode.
        peek(event.newState === "open");
      });
      document.addEventListener("shelfgauge:theme", sync);
    }
    sync();

    // In focus mode the sidebar toggle leaves focus mode; it goes on to show the
    // sidebar only when the sidebar is hidden (app.js handles that click).
    document.addEventListener("click", (event) => {
      if (!focused() || !event.target.closest(".sidebar-toggle")) {
        return;
      }
      setFocus(false);
      const toggle = document.querySelector(".sidebar-toggle");
      if (toggle.getAttribute("aria-expanded") === "true") {
        event.stopPropagation();
      }
    }, true);

    // Focus mode: the top bar comes back on scroll up and at the top edge.
    let lastY = window.scrollY;
    window.addEventListener("scroll", () => {
      const y = window.scrollY;
      const delta = y - lastY;
      if (!focused() || panelOpen() || (Math.abs(delta) < 8 && y > 0)) {
        return;
      }
      lastY = y;
      peek(delta < 0 || y <= 0);
    }, { passive: true });
    document.addEventListener("mousemove", (event) => {
      if (!focused() || panelOpen()) {
        return;
      }
      if (event.clientY <= 6) {
        peek(true);
        peekByMouse = true;
      } else if (peekByMouse) {
        const top = document.querySelector(".topbar");
        if (top && event.clientY > top.getBoundingClientRect().bottom + 48) {
          peek(false);
        }
      }
    });

    document.addEventListener("keydown", (event) => {
      if (event.ctrlKey || event.metaKey || event.altKey) {
        return;
      }
      const target = event.target;
      const typing =
        target instanceof HTMLInputElement && !["radio", "checkbox"].includes(target.type)
        || target instanceof HTMLTextAreaElement
        || target instanceof HTMLSelectElement
        || (target instanceof HTMLElement && target.isContentEditable);
      if (typing) {
        return;
      }
      switch (event.key) {
        case "Escape":
          // An open panel or mark popup closes first (the panel may already have
          // closed itself on this key).
          if (
            focused() &&
            !panelOpen() &&
            Date.now() - closedAt > 100 &&
            !document.querySelector(".mark-popup")
          ) {
            setFocus(false);
          }
          break;
        case "f":
          event.preventDefault();
          setFocus(!focused());
          break;
        case "-":
          event.preventDefault();
          stepSize(-1);
          break;
        case "=":
        case "+":
          event.preventDefault();
          stepSize(1);
          break;
        case "0":
          event.preventDefault();
          update("size", String(defaultSize));
          break;
        default:
      }
    });
  }

  // ---------- Status line ----------

  // Focus mode's status line: the current chapter (the last title above the block at
  // the top of the screen) and the reading progress. The server gives the progress at
  // the start of this part and of the next one (in ten-thousandths of the main text);
  // in between it grows with the main text (notes excluded) above that block, so it
  // matches the book card's figure.
  //
  // The track between them shows the whole book: chapter ticks (from the server),
  // bookmark ticks (from the Bookmarks list, so they follow its changes), the read part
  // and the current position. A click jumps to that place: within this part by
  // scrolling to the block (a fragment link, so Back returns), elsewhere through
  // `?progress=`. On a touch screen the first tap only shows the place. The block
  // reached from the track is not highlighted as a `:target` (the place is rough), nor
  // the one opened from a quote (the quote is); other fragment links keep their
  // highlight.
  const SCALE = 10000;
  let updateStatus = () => {};
  let drawBookmarks = () => {};

  function setupStatus() {
    const line = document.querySelector(".status-line");
    if (!line) {
      return;
    }
    const chapter = line.querySelector(".status-chapter");
    const progress = line.querySelector(".status-progress");
    const start = Number(article.dataset.progressStart);
    const end = Number(article.dataset.progressEnd);
    const hasProgress = "progressStart" in article.dataset && end >= start;
    const titles = Array.from(text.querySelectorAll(".title"));
    const track = line.querySelector(".track");
    let before = null;
    let total = 0;
    let frame = 0;
    let fill = null;
    let point = null;
    // The block id shown without the `:target` highlight (track or quote).
    let quiet = null;

    // Characters of the main text before each leaf block of the part (built lazily).
    function counts() {
      if (!before) {
        before = new Map();
        for (const leaf of leaves()) {
          if (!leaf.closest(".notes")) {
            before.set(leaf, total);
            total += leaf.textContent.length;
          }
        }
      }
      return before;
    }

    function titleText(title) {
      const lines = Array.from(title.querySelectorAll("p"), (p) => p.textContent.trim());
      return (lines.length ? lines.filter(Boolean).join(". ") : title.textContent.trim());
    }

    function render() {
      frame = 0;
      if (!document.documentElement.classList.contains("reading-focus")) {
        return;
      }
      const block = topBlock();
      if (!block) {
        return;
      }
      let current = null;
      for (const title of titles) {
        const above =
          title === block ||
          title.contains(block) ||
          title.compareDocumentPosition(block) & Node.DOCUMENT_POSITION_FOLLOWING;
        if (!above) {
          break;
        }
        current = title;
      }
      if (current) {
        chapter.textContent = titleText(current);
      }
      if (hasProgress) {
        const map = counts();
        const atEnd =
          window.scrollY + window.innerHeight >= document.documentElement.scrollHeight - 2;
        let value = null;
        if (atEnd) {
          value = end;
        } else if (map.has(block) && total > 0) {
          value = start + ((end - start) * map.get(block)) / total;
        }
        if (value !== null) {
          progress.textContent = `${Math.min(Math.floor(value / 100), 100)} %`;
          if (point) {
            point.hidden = false;
            place(point, value);
            fill.style.width = `${value / 100}%`;
          }
        }
      }
    }

    // CSSOM, not a style attribute: allowed by the CSP.
    function place(el, at) {
      el.style.left = `${Math.min(Math.max(at, 0), SCALE) / 100}%`;
    }

    function mark(className) {
      const el = document.createElement("i");
      el.className = className;
      track.append(el);
      return el;
    }

    // The main-text block at `share` of this part's range (the last one starting at
    // or before it).
    function blockAt(share) {
      const map = counts();
      const target = ((share - start) / (end - start)) * total;
      let found = null;
      for (const [leaf, chars] of map) {
        if (chars > target) {
          break;
        }
        found = leaf;
      }
      return found;
    }

    function jump(share) {
      const here = hasProgress && end > start && share >= start && (share < end || end === SCALE);
      const block = here && blockAt(share);
      if (!block) {
        window.location.href = `${track.dataset.href}?progress=${share}`;
      } else if (window.location.hash === `#${block.id}`) {
        block.scrollIntoView();
      } else {
        quietTarget(block.id);
        window.location.hash = block.id;
      }
    }

    // Opened through `?progress=` (the server redirects with `from=track`) or from a
    // quote.
    function setupQuietTarget() {
      if (openedFrom === "track" || openedFrom === "quote") {
        quietTarget(decodeURIComponent(window.location.hash.slice(1)));
      }
      window.addEventListener("hashchange", () => {
        if (decodeURIComponent(window.location.hash.slice(1)) !== quiet) {
          quiet = null;
          text.classList.remove("quiet-target");
        }
      });
    }

    function quietTarget(id) {
      quiet = id;
      text.classList.add("quiet-target");
    }

    function setupTrack() {
      track.hidden = false;
      const chapters = Array.from(track.querySelectorAll(".tick.chapter"));
      for (const tick of chapters) {
        place(tick, Number(tick.dataset.at));
      }
      fill = mark("fill");
      point = mark("point");
      point.hidden = true;
      const ghost = mark("ghost");
      const tip = mark("tip");
      ghost.hidden = true;
      tip.hidden = true;

      drawBookmarks = () => {
        for (const tick of track.querySelectorAll(".tick.bookmark")) {
          tick.remove();
        }
        for (const item of document.querySelectorAll("#tab-bookmarks li[data-progress]")) {
          place(mark("tick bookmark"), Number(item.dataset.progress));
        }
      };
      drawBookmarks();

      const shareAt = (event) => {
        const rect = track.getBoundingClientRect();
        const x = Math.min(Math.max((event.clientX - rect.left) / rect.width, 0), 1);
        return Math.round(x * SCALE);
      };
      const chapterAt = (share) => {
        let title = "";
        for (const tick of chapters) {
          if (Number(tick.dataset.at) > share) {
            break;
          }
          title = tick.dataset.title;
        }
        return title;
      };
      const show = (share) => {
        ghost.hidden = false;
        place(ghost, share);
        const title = chapterAt(share);
        tip.textContent = `${Math.floor(share / 100)} %${title ? ` · ${title}` : ""}`;
        tip.hidden = false;
        // Keep the tip within the track's width (centered on the place otherwise).
        const width = track.getBoundingClientRect().width;
        const half = tip.getBoundingClientRect().width / 2;
        const x = Math.min(Math.max((share / SCALE) * width, half), Math.max(width - half, half));
        tip.style.left = `${x}px`;
      };
      const hide = () => {
        ghost.hidden = true;
        tip.hidden = true;
      };

      let pointerType = "mouse";
      let armed = null;
      track.addEventListener("pointerdown", (event) => {
        pointerType = event.pointerType;
      });
      track.addEventListener("pointermove", (event) => {
        if (event.pointerType === "mouse") {
          show(shareAt(event));
        }
      });
      track.addEventListener("pointerleave", (event) => {
        if (event.pointerType === "mouse") {
          hide();
        }
      });
      track.addEventListener("click", (event) => {
        let share = shareAt(event);
        if (pointerType !== "mouse") {
          // Touch: the first tap shows the place, a second one near it goes there.
          if (armed === null || Math.abs(share - armed) > 300) {
            armed = share;
            show(share);
            return;
          }
          share = armed;
        }
        armed = null;
        hide();
        jump(share);
      });
      document.addEventListener("pointerdown", (event) => {
        if (armed !== null && !track.contains(event.target)) {
          armed = null;
          hide();
        }
      });
    }

    setupQuietTarget();
    if (track) {
      setupTrack();
    }
    updateStatus = render;
    const schedule = () => {
      if (!frame) {
        frame = requestAnimationFrame(render);
      }
    };
    window.addEventListener("scroll", schedule, { passive: true });
    window.addEventListener("resize", schedule);
    render();
  }

  setupStatus();
  setupSettings();
  setupSidebar();
  setupMarks();
  setupBookmarks();
  setupPosition();
  setupRescroll();
  highlight();
})();
