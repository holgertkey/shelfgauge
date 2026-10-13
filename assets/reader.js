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

  // The topmost leaf block that is (at least partly) visible below the top bar.
  function topBlock() {
    const top = document.querySelector(".topbar");
    const limit = (top ? top.getBoundingClientRect().bottom : 0) + 4;
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
    button.textContent = "Mark";
    button.title = "Keep this text as a mark (with a note if you like)";
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
        toast("Mark saved.");
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

  // A page opened from a bookmark or mark (`from=mark`) never saves the position;
  // prev/next links do not carry it, so reading on resumes saving.
  let suspended = new URLSearchParams(window.location.search).get("from") === "mark";
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

  setupSidebar();
  setupMarks();
  setupBookmarks();
  setupPosition();
  setupRescroll();
  highlight();
})();
