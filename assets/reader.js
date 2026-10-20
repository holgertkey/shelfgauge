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
  // Until this time a scroll is a page turn, not the reader's: focus mode keeps the
  // top bar hidden.
  let pageTurnUntil = 0;

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
      (root.classList.contains("reading-focus") &&
        !root.classList.contains("topbar-peek") &&
        !root.classList.contains("page-fits"))
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
    // No profile any more: reloading leads to the profile choice (app.js).
    reloadIfSignedOut(response);
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

  function showTab(name, remember = true) {
    if (!sidebar) {
      return;
    }
    const all = Array.from(sidebar.querySelectorAll(".tab-panel"));
    const panels = all.filter((p) => !p.hidden);
    // An unknown tab, or Contents in a book without them: the first tab there is, and
    // the chosen tab stays remembered for the next book.
    const known = panels.some((p) => p.dataset.tab === name);
    if (!known && panels.length) {
      name = panels[0].dataset.tab;
    }
    for (const panel of all) {
      panel.classList.toggle("active", panel.dataset.tab === name);
    }
    for (const tab of sidebar.querySelectorAll(".reader-tabs a")) {
      const active = tab.dataset.tab === name;
      tab.classList.toggle("active", active);
      tab.setAttribute("aria-current", active ? "true" : "false");
    }
    syncPager();
    // The Search tab comes and goes with the query: never remembered.
    if (known && remember && name !== "find") {
      storageSet(TAB_KEY, name);
    }
  }

  async function refreshLists() {
    const lists = sidebar && sidebar.querySelector(".annotation-lists");
    if (!lists || !createUrl) {
      return;
    }
    const response = await fetch(createUrl, { headers: { Accept: "text/html" } });
    reloadIfSignedOut(response);
    if (!response.ok) {
      throw new Error(`${createUrl}: ${response.status}`);
    }
    const template = document.createElement("template");
    template.innerHTML = (await response.text()).trim();
    const replacement = template.content.firstElementChild;
    const active = sidebar.querySelector(".reader-tabs a.active");
    if (replacement) {
      lists.replaceWith(replacement);
    }
    // The replaced panels lost their state: show the same tab again.
    showTab(active ? active.dataset.tab : storageGet(TAB_KEY) || "toc", false);
    syncPager();
    highlight();
    drawBookmarks();
  }

  // The ‹ › buttons next to the book link page through the open tab (the sidebar scrolls) like
  // Page Up / Page Down; each is disabled at its end.
  let syncPager = () => {};

  function setupPager() {
    const pager = sidebar.querySelector(".tab-pager");
    if (!pager) {
      return;
    }
    const [up, down] = pager.querySelectorAll("[data-page]");
    pager.hidden = false;
    syncPager = () => {
      up.disabled = sidebar.scrollTop <= 0;
      down.disabled = sidebar.scrollTop + sidebar.clientHeight >= sidebar.scrollHeight - 1;
    };
    pager.addEventListener("click", (event) => {
      const button = event.target.closest("[data-page]");
      if (button) {
        // The visible part below the sticky tab row, less a little overlap, like the
        // browser's Page Down.
        const row = pager.closest(".sidebar-head").getBoundingClientRect().height;
        const page = Math.max(sidebar.clientHeight - row - 40, 40);
        sidebar.scrollBy({ top: Number(button.dataset.page) * page, behavior: "smooth" });
      }
    });
    sidebar.addEventListener("scroll", syncPager, { passive: true });
    window.addEventListener("resize", syncPager);
    syncPager();
  }

  function setupSidebar() {
    if (!sidebar) {
      return;
    }
    sidebar.classList.add("tabbed");
    setupPager();
    showTab(storageGet(TAB_KEY) || "toc");
    sidebar.addEventListener("click", (event) => {
      const tab = event.target.closest(".reader-tabs a[data-tab]");
      if (tab) {
        event.preventDefault();
        showTab(tab.dataset.tab);
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
  function wrapRange(range, className = "sg-mark") {
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
      mark.className = className;
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

  const coarse = window.matchMedia("(pointer: coarse)");

  // Below the selection, inside the viewport. On a touch screen the selection handles
  // hang below the text, so the popup keeps clear of them; when there is no room
  // below, it goes above the selection.
  function place(el, rect) {
    const gap = coarse.matches ? 32 : 8;
    const view = window.visualViewport;
    const viewHeight = view ? view.height : window.innerHeight;
    let top = rect.bottom + gap;
    if (top + el.offsetHeight > viewHeight - 8 && rect.top - gap - el.offsetHeight > 8) {
      top = rect.top - gap - el.offsetHeight;
    }
    const left = Math.max(8, Math.min(rect.left, window.innerWidth - el.offsetWidth - 8));
    el.style.top = `${window.scrollY + top}px`;
    el.style.left = `${window.scrollX + left}px`;
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
    // The form is taller than the button: place it again.
    place(popup, range.getBoundingClientRect());
    note.focus({ preventScroll: true });
    // A phone's keyboard shrinks the viewport after the focus: keep the form in view.
    const keepInView = () => popup && popup.scrollIntoView({ block: "nearest" });
    window.visualViewport?.addEventListener("resize", keepInView, { once: true });
    keepInView();
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

  // ---------- Contents ----------

  // The server marks the section the part starts in; while scrolling the mark follows
  // the section of the block at the top of the screen (when scrolled to the end of the
  // page, of the last block, so a short last section can be reached). Block ids grow in document
  // order, like the entries.
  function setupContents() {
    const entries = [];
    for (const link of document.querySelectorAll(".toc a")) {
      const id = decodeURIComponent(new URL(link.href).hash.slice(1));
      if (BLOCK_ID.test(id)) {
        entries.push({ link, n: Number(id.slice(1)) });
      }
    }
    if (!entries.length) {
      return;
    }
    let frame = 0;
    const update = () => {
      frame = 0;
      const atEnd =
        window.scrollY > 0 &&
        window.scrollY + window.innerHeight >= document.documentElement.scrollHeight - 2;
      const all = leaves();
      const block = atEnd ? all[all.length - 1] : topBlock();
      if (!block) {
        return;
      }
      const n = Number(block.id.slice(1));
      let current = null;
      for (const entry of entries) {
        if (entry.n > n) {
          break;
        }
        current = entry.link;
      }
      for (const { link } of entries) {
        link.classList.toggle("active", link === current);
      }
    };
    const schedule = () => {
      if (!frame) {
        frame = requestAnimationFrame(update);
      }
    };
    window.addEventListener("scroll", schedule, { passive: true });
    window.addEventListener("resize", schedule);
    update();
  }

  // ---------- Reading position ----------

  // A page opened from a bookmark or quote (`from=mark`) or from a search result
  // (`hit=`) never saves the position; prev/next links do not carry them, so reading
  // on resumes saving.
  const opened = new URLSearchParams(window.location.search);
  let suspended = opened.get("from") === "mark" || opened.has("hit");
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

  // ---------- Page turns on touch screens ----------

  // A swipe to the left or a double tap on the right half turns a page forward, a
  // swipe to the right or a double tap on the left half back; scrolling stays as it
  // is. A page is the readable height (between the top bar or running head and the
  // status line) less one line, so a line stays in view. At the end of a part the next
  // part opens, at its start the previous one at its end (`#end`). Not on links and
  // controls, with a selection (its handles are being dragged), in the quote popup,
  // near the screen edges (the system's back gesture) or in a table that scrolls
  // sideways.
  const SWIPE_MIN = 60;
  const SWIPE_MS = 600;
  const TAP_SLOP = 10;
  const TAP_MS = 300;
  const DOUBLE_TAP_MS = 300;
  const DOUBLE_TAP_SLOP = 40;
  const EDGE = 24;

  function bottomLimit() {
    const view = window.visualViewport;
    let bottom = view ? view.height : window.innerHeight;
    const status = document.querySelector(".status-line");
    if (status && getComputedStyle(status).display !== "none") {
      bottom = Math.min(bottom, status.getBoundingClientRect().top);
    }
    return bottom;
  }

  function partLink(rel) {
    return article.querySelector(`.reader-nav a[rel="${rel}"]`);
  }

  function turnPage(forward) {
    const top = topLimit();
    const bottom = bottomLimit();
    const rect = text.getBoundingClientRect();
    if (forward && rect.bottom <= bottom + 2) {
      const next = partLink("next");
      if (next) {
        window.location.href = next.href;
      } else {
        toast("The end of the book.");
      }
      return;
    }
    if (!forward && rect.top >= top - 2) {
      const prev = partLink("prev");
      if (prev) {
        window.location.href = `${prev.href.split("#")[0]}#end`;
      }
      return;
    }
    const line = parseFloat(getComputedStyle(text).lineHeight) || 24;
    const page = Math.max(bottom - top - line, line);
    pageTurnUntil = Date.now() + 400;
    window.scrollBy({ top: forward ? page : -page, behavior: "instant" });
  }

  // Whether a touch at `target` may turn a page.
  function turnsPages(target, x) {
    if (!(target instanceof Element) || !article.contains(target)) {
      return false;
    }
    if (target.closest("a, button, input, textarea, select, summary, label, .mark-popup")) {
      return false;
    }
    if (x < EDGE || x > window.innerWidth - EDGE) {
      return false;
    }
    const selection = window.getSelection();
    if ((selection && !selection.isCollapsed) || popup) {
      return false;
    }
    for (let el = target; el && el !== text; el = el.parentElement) {
      if (el.scrollWidth > el.clientWidth + 1) {
        const overflow = getComputedStyle(el).overflowX;
        if (overflow === "auto" || overflow === "scroll") {
          return false;
        }
      }
    }
    return true;
  }

  function setupPaging() {
    // The previous part opened at its end.
    if (window.location.hash === "#end") {
      history.replaceState(null, "", window.location.pathname + window.location.search);
      pageTurnUntil = Date.now() + 400;
      window.scrollTo({ top: document.documentElement.scrollHeight, behavior: "instant" });
    }
    if (!coarse.matches) {
      return;
    }
    let start = null;
    let lastTap = null;
    document.addEventListener("touchstart", (event) => {
      const touch = event.touches[0];
      start = event.touches.length === 1 && turnsPages(event.target, touch.clientX)
        ? { x: touch.clientX, y: touch.clientY, t: Date.now() }
        : null;
    }, { passive: true });
    document.addEventListener("touchmove", (event) => {
      if (event.touches.length > 1) {
        start = null;
      }
    }, { passive: true });
    document.addEventListener("touchend", (event) => {
      if (!start || event.touches.length > 0) {
        return;
      }
      const touch = event.changedTouches[0];
      const dx = touch.clientX - start.x;
      const dy = touch.clientY - start.y;
      const dt = Date.now() - start.t;
      start = null;
      if (Math.abs(dx) >= SWIPE_MIN && Math.abs(dx) > 2 * Math.abs(dy) && dt <= SWIPE_MS) {
        lastTap = null;
        turnPage(dx < 0);
        return;
      }
      if (Math.abs(dx) > TAP_SLOP || Math.abs(dy) > TAP_SLOP || dt > TAP_MS) {
        lastTap = null;
        return;
      }
      const now = Date.now();
      if (
        lastTap &&
        now - lastTap.t <= DOUBLE_TAP_MS &&
        Math.abs(touch.clientX - lastTap.x) <= DOUBLE_TAP_SLOP &&
        Math.abs(touch.clientY - lastTap.y) <= DOUBLE_TAP_SLOP
      ) {
        lastTap = null;
        // No click (and no zoom) from the second tap.
        event.preventDefault();
        turnPage(touch.clientX >= window.innerWidth / 2);
        return;
      }
      lastTap = { x: touch.clientX, y: touch.clientY, t: now };
    });
  }

  // ---------- Notes: the preview and the way back ----------

  // A note link opens the note in a panel at the bottom instead of leaving the text;
  // a note in another part is taken from that part's page (fetched once). "Open in
  // notes" in the panel, and any other link in the text to a place in the book,
  // remember where the reader was: the page they lead to shows "Back to text" in the
  // status line (and above the text when the status line is hidden), which returns
  // there. Ctrl/Cmd/Shift-clicks stay plain links.
  const RETURN_KEY = "shelfgauge-return";
  const notePages = new Map();
  let notePanel = null;
  let noteRef = null;

  function here() {
    return window.location.pathname + window.location.search;
  }

  function readReturn() {
    try {
      return JSON.parse(sessionStorage.getItem(RETURN_KEY) || "null");
    } catch (e) {
      return null;
    }
  }

  function writeReturn(state) {
    try {
      if (state) {
        sessionStorage.setItem(RETURN_KEY, JSON.stringify(state));
      } else {
        sessionStorage.removeItem(RETURN_KEY);
      }
    } catch (e) {
      // Not remembered: no way back.
    }
  }

  // A link to a place in this book (this part or another one).
  function bookLink(a) {
    const url = new URL(a.href, window.location.href);
    return url.origin === window.location.origin && url.pathname === window.location.pathname
      ? url
      : null;
  }

  function rememberReturn(url) {
    const block = topBlock();
    writeReturn({
      from: here(),
      block: block ? block.id : null,
      y: window.scrollY,
      to: url.pathname + url.search,
    });
  }

  function showReturn(state) {
    const href = state.from + (state.block ? `#${state.block}` : "");
    for (const link of document.querySelectorAll(".back-to-text")) {
      link.href = href;
      link.hidden = false;
    }
    document.documentElement.classList.add("has-return");
  }

  function hideReturn() {
    for (const link of document.querySelectorAll(".back-to-text")) {
      link.hidden = true;
    }
    document.documentElement.classList.remove("has-return");
  }

  function closeNote() {
    if (!notePanel) {
      return;
    }
    notePanel.remove();
    notePanel = null;
    if (noteRef) {
      noteRef.focus({ preventScroll: true });
      noteRef = null;
    }
  }

  // The note a link leads to, as nodes for the panel (no ids, no note title); null if
  // it cannot be found.
  async function noteContent(url) {
    let doc = document;
    const page = url.pathname + url.search;
    if (page !== here()) {
      if (!notePages.has(page)) {
        notePages.set(
          page,
          fetch(page, { credentials: "same-origin" })
            .then((r) => {
              if (!r.ok) {
                throw new Error(r.statusText);
              }
              return r.text();
            })
            .then((html) => new DOMParser().parseFromString(html, "text/html")),
        );
      }
      try {
        doc = await notePages.get(page);
      } catch (e) {
        notePages.delete(page);
        return null;
      }
    }
    const target = doc.getElementById(decodeURIComponent(url.hash.slice(1)));
    if (!target) {
      return null;
    }
    // In a notes body the note is the section around its title; elsewhere the block.
    const section = target.closest(".notes section");
    const clone = (section || target).cloneNode(true);
    if (section) {
      for (const title of clone.querySelectorAll(":scope > .title")) {
        title.remove();
      }
    }
    clone.removeAttribute("id");
    for (const el of clone.querySelectorAll("[id]")) {
      el.removeAttribute("id");
    }
    return document.importNode(clone, true);
  }

  // On a wide screen the panel sits under the text column (beside the sidebar), not
  // in the middle of the window; on a phone it spans the screen (CSS).
  function alignNote() {
    if (!notePanel) {
      return;
    }
    const style = notePanel.style;
    if (window.matchMedia("(max-width: 48rem)").matches) {
      style.left = style.right = style.width = style.maxWidth = "";
      return;
    }
    const rect = text.getBoundingClientRect();
    style.left = `${Math.max(rect.left - 16, 8)}px`;
    style.right = "auto";
    style.width = `${Math.min(rect.width + 32, window.innerWidth - 16)}px`;
    style.maxWidth = "none";
  }

  async function openNote(a, url) {
    closeNote();
    noteRef = a;
    const label = a.textContent.replace(/[[\]()]/g, "").trim();
    const panel = document.createElement("div");
    panel.className = "note-panel";
    panel.setAttribute("role", "dialog");
    panel.setAttribute("aria-label", label ? `Note ${label}` : "Note");
    panel.tabIndex = -1;
    panel.lang = "en";
    const head = document.createElement("div");
    head.className = "note-head";
    const title = document.createElement("span");
    title.className = "note-label";
    title.textContent = label ? `Note ${label}` : "Note";
    const open = document.createElement("a");
    open.className = "note-open";
    open.href = url.href;
    open.textContent = "Open in notes";
    open.addEventListener("click", () => rememberReturn(url));
    const close = document.createElement("button");
    close.type = "button";
    close.className = "icon-button note-close";
    close.setAttribute("aria-label", "Close");
    close.title = "Close (Esc)";
    close.textContent = "×";
    close.addEventListener("click", closeNote);
    head.append(title, open, close);
    const body = document.createElement("div");
    body.className = "note-body";
    body.lang = document.documentElement.lang;
    body.textContent = "Loading…";
    panel.append(head, body);
    document.body.append(panel);
    notePanel = panel;
    alignNote();
    panel.focus({ preventScroll: true });
    const content = await noteContent(url);
    if (notePanel !== panel) {
      return;
    }
    if (!content) {
      // Not found here: go there, with the way back.
      closeNote();
      rememberReturn(url);
      window.location.href = url.href;
      return;
    }
    body.textContent = "";
    body.append(content);
  }

  function setupNotes() {
    const state = readReturn();
    if (state && state.to === here()) {
      showReturn(state);
    } else if (state) {
      writeReturn(null);
    }
    for (const link of document.querySelectorAll(".back-to-text")) {
      link.addEventListener("click", (event) => {
        const back = readReturn();
        writeReturn(null);
        hideReturn();
        // Within this page: back to where the reader was, without a reload.
        if (back && back.from === here()) {
          event.preventDefault();
          window.scrollTo({ top: back.y, behavior: "instant" });
        }
      });
    }
    text.addEventListener("click", (event) => {
      const a = event.target.closest("a[href]");
      if (!a || event.ctrlKey || event.metaKey || event.shiftKey || event.altKey || event.button !== 0) {
        return;
      }
      const url = bookLink(a);
      if (!url || !url.hash) {
        return;
      }
      if (a.classList.contains("note-ref")) {
        event.preventDefault();
        openNote(a, url);
        return;
      }
      rememberReturn(url);
      if (url.pathname + url.search === here()) {
        // A jump within this part: the browser scrolls, the way back shows now.
        setTimeout(() => showReturn(readReturn()), 0);
      }
    });
    document.addEventListener("click", (event) => {
      if (
        notePanel &&
        !notePanel.contains(event.target) &&
        !(event.target instanceof Element && event.target.closest(".note-ref"))
      ) {
        closeNote();
      }
    });
    window.addEventListener("resize", alignNote);
    document.addEventListener("keydown", (event) => {
      if (notePanel && event.key === "Escape") {
        event.preventDefault();
        event.stopImmediatePropagation();
        closeNote();
      }
    }, true);
  }

  // ---------- Search in the book ----------

  // The server searches the whole book (`/book/{id}/find?q=`) and lists the results
  // in the Search tab; each links to `read?part=<n>&find=<query>&hit=<k>#<block>`.
  // Here the matches of the query in this part are highlighted, with the same
  // normalization as the server (src/annotations/find.rs; both are tested with
  // tests/fixtures/find/normalize.json): one code point at a time lowercased, ё → е,
  // ς → σ, ’ and ʼ → ', soft hyphens and zero-width characters dropped, whitespace
  // runs → one space. Offsets count code points, like anchors.
  const FIND_DEBOUNCE_MS = 250;
  const FIND_MIN = 2;
  const FIND_MAX = 100;
  const FIND_MAX_RAW = 1000;

  function findDropped(c) {
    return c === 0xad || (c >= 0x200b && c <= 0x200d) || c === 0x2060 || c === 0xfeff;
  }

  // The same explicit list as the server (JavaScript's `\s` differs from Rust's).
  function findSpace(c) {
    return (
      (c >= 0x09 && c <= 0x0d) ||
      c === 0x20 ||
      c === 0xa0 ||
      c === 0x1680 ||
      (c >= 0x2000 && c <= 0x200a) ||
      c === 0x2028 ||
      c === 0x2029 ||
      c === 0x202f ||
      c === 0x205f ||
      c === 0x3000
    );
  }

  // The normalized text, and for each of its UTF-16 units the original code point
  // range it comes from.
  function findNormalize(s) {
    let out = "";
    const starts = [];
    const ends = [];
    let inSpace = false;
    let i = 0;
    for (const ch of s) {
      const c = ch.codePointAt(0);
      if (findDropped(c)) {
        i += 1;
        continue;
      }
      if (findSpace(c)) {
        if (inSpace) {
          ends[ends.length - 1] = i + 1;
        } else {
          out += " ";
          starts.push(i);
          ends.push(i + 1);
          inSpace = true;
        }
        i += 1;
        continue;
      }
      inSpace = false;
      let lower = "";
      if (c === 0x2019 || c === 0x2bc) {
        lower = "'";
      } else if (c < 0x80) {
        lower = ch.toLowerCase();
      } else {
        // One code point at a time: no context rules (final sigma).
        for (const l of ch.toLowerCase()) {
          lower += l === "ё" ? "е" : l === "ς" ? "σ" : l;
        }
      }
      for (let k = 0; k < lower.length; k += 1) {
        starts.push(i);
        ends.push(i + 1);
      }
      out += lower;
      i += 1;
    }
    return { text: out, starts, ends };
  }

  // The normalized, trimmed query, or null if it is not searched.
  function findQuery(raw) {
    if (cpLength(raw) > FIND_MAX_RAW) {
      return null;
    }
    const query = findNormalize(raw).text.replace(/^ +| +$/g, "");
    const length = cpLength(query);
    return length >= FIND_MIN && length <= FIND_MAX ? query : null;
  }

  // The code point ranges of the matches of `query` (normalized) in `s`: left to
  // right, without overlaps.
  function findMatches(s, query) {
    const n = findNormalize(s);
    const hits = [];
    let from = 0;
    for (;;) {
      const at = n.text.indexOf(query, from);
      if (at < 0) {
        break;
      }
      hits.push([n.starts[at], n.ends[at + query.length - 1]]);
      from = at + query.length;
    }
    return hits;
  }

  // For the browser test of the shared table.
  window.shelfgaugeFind = {
    normalize: (s) => findNormalize(s).text,
    query: findQuery,
    matches: findMatches,
  };

  // The top bar's search field searches the book in reading mode.
  const findForm = document.querySelector(".topbar .find-form");
  const findInput = findForm && findForm.querySelector(".find-input");
  // The query whose matches are highlighted ("" for none).
  let findShown = "";

  function clearFindMarks() {
    for (const mark of text.querySelectorAll("mark.sg-find")) {
      mark.replaceWith(...mark.childNodes);
    }
    text.normalize();
  }

  // The result followed to this page: its list item, if it is in this part.
  function currentFindItem() {
    return sidebar ? sidebar.querySelector(".find-hits li.current") : null;
  }

  // Highlights the matches of `raw` in this part; the followed result stronger.
  function highlightFind(raw) {
    const query = raw ? findQuery(raw) : null;
    findShown = query ? raw : "";
    if (!canHighlight) {
      clearFindMarks();
    }
    const ranges = [];
    let current = null;
    if (query) {
      const item = currentFindItem();
      for (const leaf of leaves()) {
        for (const [start, end] of findMatches(blockText(leaf), query)) {
          const a = domPoint(leaf, start);
          const b = domPoint(leaf, end);
          const range = document.createRange();
          range.setStart(a.node, a.offset);
          range.setEnd(b.node, b.offset);
          if (
            item &&
            !current &&
            item.dataset.block === leaf.id &&
            Number(item.dataset.start) === start
          ) {
            current = range;
          } else {
            ranges.push(range);
          }
        }
      }
    }
    if (canHighlight) {
      CSS.highlights.set("sg-find", new Highlight(...ranges));
      CSS.highlights.set("sg-find-current", new Highlight(...(current ? [current] : [])));
    } else {
      // All ranges first, then wrap from the end so earlier ranges stay valid.
      const all = ranges.map((range) => [range, "sg-find"]);
      if (current) {
        all.push([current, "sg-find current"]);
      }
      all.sort((x, y) => x[0].compareBoundaryPoints(Range.START_TO_START, y[0]));
      for (const [range, className] of all.reverse()) {
        wrapRange(range, className);
      }
    }
  }

  // The page URL with the query (none for an empty one) and without a followed result.
  function setFindUrl(raw) {
    const url = new URL(window.location.href);
    if (raw.trim()) {
      url.searchParams.set("find", raw);
    } else {
      url.searchParams.delete("find");
    }
    url.searchParams.delete("hit");
    history.replaceState(history.state, "", url);
  }

  // Clears the search: field, results, highlights and URL. Whether there was one.
  let clearFind = () => false;
  // Goes to the next (1) or previous (-1) result; whether there was one.
  let stepFind = () => false;
  // Leaves focus mode (set by setupSettings).
  let leaveFocus = () => {};

  function setupFind() {
    if (!findForm || !findInput || !sidebar) {
      return;
    }
    const results = () => sidebar.querySelector(".find-results");
    const tab = sidebar.querySelector('.reader-tabs a[data-tab="find"]');
    const panel = document.getElementById("tab-find");
    // The tab shown before the results, to go back to when the search is cleared.
    let before = null;
    let controller = null;
    let timer = null;

    // Shows or hides the Search tab: it exists only while there is a query.
    function showResults(on) {
      if (!tab || !panel) {
        return;
      }
      if (on && tab.hidden) {
        const active = sidebar.querySelector(".reader-tabs a.active");
        before = active && active !== tab ? active.dataset.tab : null;
      }
      const wasActive = tab.classList.contains("active");
      tab.hidden = !on;
      panel.hidden = !on;
      if (on) {
        showTab("find", false);
      } else if (wasActive) {
        showTab(before || storageGet(TAB_KEY) || "toc", false);
      }
    }

    // The sidebar must be visible for the results (not in focus mode).
    function openSidebar() {
      leaveFocus();
      const toggle = document.querySelector(".sidebar-toggle");
      if (toggle && toggle.getAttribute("aria-expanded") !== "true") {
        toggle.click();
      }
    }

    // The tab shows the number of matches the results carry.
    function syncTab() {
      if (!tab) {
        return;
      }
      const list = results();
      tab.querySelector(".tab-count").textContent = list.dataset.tabCount || "";
      tab.title = list.dataset.tabTitle || "Search results";
      tab.setAttribute("aria-label", tab.title);
    }

    function replaceResults(html) {
      const template = document.createElement("template");
      template.innerHTML = html.trim();
      const replacement = template.content.firstElementChild;
      if (replacement) {
        results().replaceWith(replacement);
      }
      syncTab();
      syncPager();
    }

    async function search(raw) {
      clearTimeout(timer);
      if (controller) {
        controller.abort();
        controller = null;
      }
      setFindUrl(raw);
      if (!raw.trim()) {
        replaceResults('<div class="find-results" data-tab-count="" data-tab-title="Search results"></div>');
        highlightFind("");
        showResults(false);
        return;
      }
      controller = new AbortController();
      const url = `${findForm.dataset.href}?q=${encodeURIComponent(raw)}`;
      try {
        const response = await fetch(url, {
          headers: { Accept: "text/html" },
          signal: controller.signal,
        });
        reloadIfSignedOut(response);
        if (!response.ok) {
          throw new Error(`${response.status}`);
        }
        replaceResults(await response.text());
        highlightFind(raw);
        openSidebar();
        showResults(true);
      } catch (e) {
        if (e.name !== "AbortError") {
          toast(`Could not search: ${e.message}`);
        }
      }
    }

    clearFind = () => {
      if (!findShown && !findInput.value && (!tab || tab.hidden)) {
        return false;
      }
      findInput.value = "";
      search("");
      return true;
    };

    // Without a followed result: the first result after the top of the screen (or
    // the last one before it).
    function nextFrom(items, delta) {
      const part = Number(findForm.dataset.part) || 1;
      const top = topBlock();
      const here = top ? Number(top.id.slice(1)) : 0;
      const after = (item) => {
        const p = Number(item.dataset.part);
        const n = Number(item.dataset.block.slice(1));
        return p > part || (p === part && n >= here);
      };
      if (delta > 0) {
        return items.find(after) || null;
      }
      const earlier = items.filter((item) => !after(item));
      return earlier[earlier.length - 1] || null;
    }

    stepFind = (delta) => {
      const items = Array.from(sidebar.querySelectorAll(".find-hits li[data-hit]"));
      if (!items.length) {
        return false;
      }
      const current = items.indexOf(currentFindItem());
      const target = current >= 0 ? items[current + delta] : nextFrom(items, delta);
      if (!target) {
        toast(delta > 0 ? "No more matches." : "This is the first match.");
        return true;
      }
      window.location.href = target.querySelector("a").href;
      return true;
    };

    findForm.addEventListener("submit", (event) => {
      event.preventDefault();
      search(findInput.value);
    });
    findInput.addEventListener("input", () => {
      clearTimeout(timer);
      const raw = findInput.value;
      if (!raw.trim()) {
        search("");
      } else {
        timer = setTimeout(() => search(raw), FIND_DEBOUNCE_MS);
      }
    });
    // Esc in the field only leaves it (app.js blurs it), so that `n` works next; a
    // second Esc clears the search. Chrome would clear a search field instead.
    findInput.addEventListener("keydown", (event) => {
      if (event.key === "Escape") {
        event.preventDefault();
      }
    });
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
      if (typing) {
        return;
      }
      if (event.key === "/" || event.key === "s") {
        // app.js leaves these keys alone once they are handled here.
        event.preventDefault();
        // Focus mode hides the top bar with the field.
        leaveFocus();
        findInput.focus();
        findInput.select();
      } else if (event.key === "n" || event.key === "N") {
        if (stepFind(event.key === "n" ? 1 : -1)) {
          event.preventDefault();
        }
      }
    });

    const raw = opened.get("find") || "";
    if (raw.trim()) {
      showResults(true);
      highlightFind(raw);
      // After app.js has scrolled the sidebar to the current Contents entry.
      document.addEventListener("DOMContentLoaded", () => {
        const item = currentFindItem();
        if (item) {
          const offset = item.getBoundingClientRect().top - sidebar.getBoundingClientRect().top;
          sidebar.scrollTop += offset - sidebar.clientHeight / 3;
        }
      });
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

    // Focus mode on a page that fits the window: nothing to scroll, so the top bar
    // stays. Measured without the room the class itself adds above the text.
    function updateFits() {
      const bar = document.querySelector(".topbar");
      const barHeight = bar ? bar.getBoundingClientRect().height : 0;
      const fits = root.classList.contains("page-fits");
      // The body, not <html>: the root is never shorter than the window.
      const content = document.body.scrollHeight - (fits ? barHeight : 0);
      const view = window.visualViewport ? window.visualViewport.height : window.innerHeight;
      root.classList.toggle("page-fits", focused() && content + barHeight <= view + 1);
    }

    function setFocus(on) {
      keepPlace(() => settings.setFocus(on));
      updateFits();
      // Toggled from the open panel: the top bar it hangs from stays.
      peek(panelOpen());
      sync();
      updateStatus();
    }

    leaveFocus = () => {
      if (focused()) {
        setFocus(false);
      }
    };
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
          // app.js: saves it to the profile and updates the top-bar select.
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
    // Late images, another text size or a turned phone change whether the page fits.
    updateFits();
    window.addEventListener("resize", updateFits);
    if ("ResizeObserver" in window) {
      new ResizeObserver(updateFits).observe(article);
    }

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
      if (Date.now() < pageTurnUntil) {
        lastY = y;
        return;
      }
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
          // closed itself on this key), then the search, then focus mode.
          if (
            !panelOpen() &&
            Date.now() - closedAt > 100 &&
            !document.querySelector(".mark-popup") &&
            !clearFind() &&
            focused()
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
  // `?progress=`. On a touch screen the first tap only shows the place.
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
        window.location.hash = block.id;
      }
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
  setupFind();
  setupContents();
  setupMarks();
  setupBookmarks();
  setupPosition();
  setupRescroll();
  setupPaging();
  setupNotes();
  highlight();
})();
