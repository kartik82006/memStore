// MemStor assistant: one box to type into. While typing, files whose names match show up
// right away (free). Enter asks MemStor, which answers in a sentence or two with cards:
// files, pieces of work, and changes waiting for your OK.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const q = document.getElementById("q");
const feed = document.getElementById("feed");
const seeing = document.getElementById("seeing");
const aware = document.getElementById("aware");
const moreBtn = document.getElementById("more");
const menu = document.getElementById("menu");
const recDot = document.getElementById("rec-dot");
const pauseItem = document.getElementById("pause-item");
const toast = document.getElementById("toast");

const st = {
  turns: [], // { ask, loading, reply, error }
  quick: { query: "", files: [], sel: -1 },
  off: new Set(), // context chips switched off for this ask
  ctx: null, // what context_now returned
  view: "chat", // "chat" or "changes"
  changes: [],
  opened: new Set(), // turns whose "open" card already ran
};
let quickTimer;

// ---------- small helpers ----------

function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.textContent = text;
  return e;
}

function button(text, cls, onClick, title) {
  const b = el("button", cls, text);
  b.type = "button";
  if (title) b.title = title;
  b.addEventListener("click", async (e) => {
    e.stopPropagation();
    try { await onClick(); } catch (err) { showToast(String(err)); }
  });
  return b;
}

function showToast(msg) {
  toast.textContent = msg;
  toast.hidden = false;
  clearTimeout(showToast.t);
  showToast.t = setTimeout(() => (toast.hidden = true), 4000);
}

function hm(ts) {
  return new Date(ts * 1000).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

function when(ts) {
  if (!ts) return "";
  const d = new Date(ts * 1000);
  const now = new Date();
  if (d.toDateString() === now.toDateString()) return hm(ts);
  const y = new Date(now);
  y.setDate(now.getDate() - 1);
  if (d.toDateString() === y.toDateString()) return "Yesterday " + hm(ts);
  const sameYear = d.getFullYear() === now.getFullYear();
  return d.toLocaleDateString([], sameYear ? { day: "numeric", month: "short" } : { day: "numeric", month: "short", year: "numeric" });
}

function span(a, b) {
  const da = new Date(a * 1000).toDateString();
  const db = new Date(b * 1000).toDateString();
  return da === db ? `${when(a)}–${hm(b)}` : `${when(a)} to ${when(b)}`;
}

function hide() {
  closeMenu();
  invoke("hide_window");
}

async function openPath(path, reveal) {
  await invoke(reveal ? "show_in_folder" : "open_file", { path });
  setTimeout(hide, 150);
}

// ---------- context ("Seeing") ----------

async function refreshContext() {
  try {
    st.ctx = await invoke("context_now");
  } catch (e) {
    st.ctx = null;
  }
  renderContext();
  if (!st.turns.length && !q.value.trim()) render();
}

function renderContext() {
  const c = st.ctx;
  seeing.replaceChildren();
  aware.replaceChildren();
  aware.hidden = !(c && c.ask_awareness);
  if (c && c.ask_awareness) {
    aware.append(
      el("p", null, "So “this” and “what I'm working on” make sense, MemStor can note which app and window you're using, and what's selected in File Explorer. It stays on this PC and goes to Claude only as one short line with your question."),
    );
    const row = el("div", "row-btns");
    row.append(
      button("Keep it on", "btn primary small", async () => { await invoke("set_pc_awareness", { on: true }); refreshContext(); }),
      button("Turn it off", "btn small", async () => { await invoke("set_pc_awareness", { on: false }); refreshContext(); }),
    );
    aware.append(row);
  }
  const chips = c ? c.chips : [];
  seeing.hidden = !chips.length;
  if (!chips.length) return;
  seeing.append(el("span", "seeing-label", "Seeing"));
  chips.forEach((chip) => {
    const off = st.off.has(chip.key);
    const b = el("button", "ctx-chip" + (off ? " off" : ""));
    b.type = "button";
    b.title = off ? "Left out of your next question. Click to include it." : "Sent with your question. Click to leave it out.";
    b.append(el("span", "ctx-text", chip.label), el("span", "ctx-x", off ? "+" : "×"));
    b.addEventListener("click", () => {
      if (st.off.has(chip.key)) st.off.delete(chip.key);
      else st.off.add(chip.key);
      renderContext();
      q.focus();
    });
    seeing.append(b);
  });
  if (c && (c.paused || !c.recording)) {
    seeing.append(el("span", "ctx-warn", c.paused ? "Recording paused" : "Recorder starting…"));
  }
}

// ---------- rendering ----------

function fileRow(f, { onPick, selected } = {}) {
  const row = el("div", "frow" + (f.exists ? "" : " gone") + (selected ? " sel" : ""));
  row.title = f.path;
  const top = el("div", "frow-top");
  top.append(el("span", "fname", f.name));
  if (f.when) top.append(el("span", "fwhen", when(f.when)));
  row.append(top);
  const bottom = el("div", "frow-bottom");
  const folder = el("span", "ffolder");
  folder.append(el("bdi", null, f.folder));
  bottom.append(folder);
  if (f.note) bottom.append(el("span", "fnote", f.note));
  row.append(bottom);
  if (f.exists) {
    const acts = el("div", "frow-acts");
    acts.append(button("Open", "btn small", () => openPath(f.path, false)), button("Folder", "btn small subtle", () => openPath(f.path, true), "Show in folder"));
    row.append(acts);
    row.addEventListener("click", () => (onPick ? onPick() : openPath(f.path, false)));
  } else {
    bottom.append(el("span", "fnote", "no longer there"));
  }
  return row;
}

function fileList(files, limit = 5) {
  const box = el("div", "flist");
  const shown = files.slice(0, limit);
  shown.forEach((f) => box.append(fileRow(f)));
  if (files.length > limit) {
    box.append(
      button(`Show all ${files.length}`, "btn small subtle more-files", () => {
        box.replaceWith(fileList(files, files.length));
      }),
    );
  }
  return box;
}

async function gather(title, files) {
  const paths = files.filter((f) => f.exists).map((f) => f.path);
  const g = await invoke("gather_paths", { title, paths });
  showToast(`Made ${g.linked} shortcut${g.linked === 1 ? "" : "s"} in ${g.folder}. Your files stay where they are.`);
}

function workLabel(kind) {
  return { activity: "Piece of work", memory: "Memory", group: "Event or group" }[kind] || "Work";
}

function renderCard(card) {
  switch (card.type) {
    case "files": {
      const box = el("section", "card");
      const head = el("header", "card-head");
      head.append(el("h3", null, card.title), el("span", "card-meta", `${card.files.length} file${card.files.length === 1 ? "" : "s"}`));
      box.append(head, fileList(card.files));
      if (card.gather) {
        const n = card.files.filter((f) => f.exists).length;
        const bar = el("div", "card-bar");
        bar.append(button(`Gather ${n} shortcut${n === 1 ? "" : "s"}`, "btn primary", () => gather(card.gather, card.files)), el("span", "muted", "Makes a folder of shortcuts. Nothing is moved."));
        box.append(bar);
      }
      return box;
    }
    case "work": {
      const box = el("section", "card");
      const head = el("header", "card-head");
      const t = el("div", "card-title");
      t.append(el("span", "kind", workLabel(card.kind)), el("h3", null, card.title));
      head.append(t, el("span", "card-meta", span(card.start, card.end)));
      box.append(head);
      if (card.subtitle) box.append(el("p", "card-sub", card.subtitle));
      box.append(fileList(card.files, 4));
      const bar = el("div", "card-bar");
      const day = new Date(card.end * 1000).toISOString().slice(0, 10);
      bar.append(button("Gather its files", "btn", () => gather(`${card.gather || card.title} ${day}`, card.files)));
      box.append(bar);
      return box;
    }
    case "open": {
      const box = el("div", "opened");
      box.append(el("span", null, `${card.reveal ? "Showed" : "Opened"} ${card.file.name}`));
      if (card.file.exists) box.append(button("Show in folder", "btn small subtle", () => openPath(card.file.path, true)));
      return box;
    }
    case "proposal":
      return proposalCard(card.action);
    case "note":
      return el("p", "note", card.text);
    default:
      return el("p", "note", "");
  }
}

function proposalCard(a) {
  const box = el("section", "card change " + a.status);
  const head = el("header", "card-head");
  const t = el("div", "card-title");
  t.append(el("span", "kind", a.status === "proposed" ? "Check before I do this" : "Change"), el("h3", null, a.title));
  head.append(t);
  if (a.status !== "proposed") head.append(el("span", "card-meta", when(a.created)));
  box.append(head);
  const ul = el("ul", "steps");
  a.steps.slice(0, 8).forEach((s) => ul.append(el("li", null, s)));
  if (a.steps.length > 8) ul.append(el("li", "muted", `…and ${a.steps.length - 8} more`));
  box.append(ul);
  const bar = el("div", "card-bar");
  const replace = (next) => box.replaceWith(proposalCard(next));
  if (a.status === "proposed") {
    bar.append(
      button("Do it", "btn primary", async () => {
        const done = await invoke("apply_action", { id: a.id });
        showToast("Done. You can undo it for 30 days from More → Changes.");
        replace(done);
      }),
      button("Cancel", "btn", () => replace({ ...a, status: "cancelled" })),
      el("span", "muted", "Nothing changes until you press Do it."),
    );
  } else if (a.status === "done") {
    bar.append(el("span", "ok", "Done."));
    if (a.can_undo) bar.append(button("Undo", "btn small", async () => replace(await invoke("undo_action", { id: a.id }))));
  } else if (a.status === "undone") {
    bar.append(el("span", "muted", "Undone: everything is back where it was."));
  } else if (a.status === "cancelled") {
    bar.append(el("span", "muted", "Cancelled. Nothing was changed."));
  } else {
    bar.append(el("span", "muted", a.status));
  }
  box.append(bar);
  return box;
}

function turnView(t) {
  const box = el("article", "turn");
  box.append(el("p", "you", t.ask));
  const ans = el("div", "answer");
  if (t.loading) {
    ans.append(el("p", "thinking", "Looking…"));
  } else if (t.error) {
    ans.append(el("p", "note", t.error));
  } else {
    const r = t.reply;
    ans.append(el("p", "say", r.text));
    r.cards.forEach((c) => ans.append(renderCard(c)));
    const foot = [];
    if (r.steps.length) foot.push(r.steps.join(" · "));
    foot.push(r.ai ? (r.cost ? `Cost ${r.cost}` : "") : "Answered on your PC, free");
    const meta = foot.filter(Boolean).join(" · ");
    if (meta) ans.append(el("p", "meta", meta));
  }
  box.append(ans);
  return box;
}

function emptyView() {
  const box = el("div", "empty");
  box.append(el("h2", null, "What are you looking for?"));
  box.append(el("p", "muted", "Type part of a file name, or ask the way you'd ask a friend."));
  const sug = el("div", "suggest");
  const c = st.ctx;
  const ideas = [
    "What did I work on today?",
    c && c.chips.some((x) => x.key === "selected") ? "Rename these properly" : "Gather what I'm working on",
    "The photos from the last event I went to",
    "Open my latest download",
  ];
  ideas.forEach((s) => sug.append(button(s, "sugg", () => { q.value = s; ask(); })));
  box.append(sug);
  if (c && !c.ai_ready) {
    const p = el("p", "muted off-ai");
    p.append("AI is off, so questions are answered with a search on this PC. ");
    p.append(button("Turn it on in Settings", "linkish", () => (location.href = "classic.html#settings")));
    box.append(p);
  }
  return box;
}

function quickView() {
  const box = el("section", "quick");
  const head = el("header", "quick-head");
  head.append(el("span", null, st.quick.files.length ? "Files named like this" : "No file names match"), el("span", "muted", "Enter asks MemStor"));
  box.append(head);
  st.quick.files.forEach((f, i) => box.append(fileRow(f, { selected: i === st.quick.sel })));
  return box;
}

function changesView() {
  const box = el("div", "changes");
  const head = el("header", "quick-head");
  head.append(el("span", null, "Changes made with MemStor"), button("Back", "btn small subtle", () => { st.view = "chat"; render(); }));
  box.append(head);
  if (!st.changes.length) box.append(el("p", "muted pad", "No changes yet. When MemStor renames or moves files for you, they're listed here with Undo."));
  st.changes.forEach((a) => box.append(proposalCard(a)));
  return box;
}

function render() {
  feed.replaceChildren();
  if (st.view === "changes") return feed.append(changesView());
  if (q.value.trim() && st.quick.query === q.value.trim()) {
    feed.append(quickView());
    return;
  }
  if (!st.turns.length) return feed.append(emptyView());
  st.turns.forEach((t) => feed.append(turnView(t)));
  feed.scrollTop = feed.scrollHeight;
}

// ---------- actions ----------

async function quickSearch() {
  const query = q.value.trim();
  if (!query) {
    st.quick = { query: "", files: [], sel: -1 };
    return render();
  }
  try {
    const hits = await invoke("search", { query });
    if (q.value.trim() !== query) return;
    st.quick = {
      query,
      sel: -1,
      files: hits.slice(0, 7).map((h) => ({ path: h.path, name: h.name, folder: h.folder, when: h.when, note: "", exists: true })),
    };
  } catch (_) {
    st.quick = { query, files: [], sel: -1 };
  }
  render();
}

async function ask(text = q.value.trim()) {
  if (!text) return;
  st.view = "chat";
  q.value = "";
  st.quick = { query: "", files: [], sel: -1 };
  const turn = { ask: text, loading: true };
  st.turns.push(turn);
  if (st.turns.length > 12) st.turns.shift();
  render();
  try {
    turn.reply = await invoke("ask", { text, off: [...st.off] });
  } catch (e) {
    turn.error = String(e);
  }
  turn.loading = false;
  render();
  // An "open" answer opens the file at once.
  const open = turn.reply && turn.reply.cards.find((c) => c.type === "open");
  if (open && open.file.exists && !st.opened.has(turn)) {
    st.opened.add(turn);
    try { await openPath(open.file.path, open.reveal); } catch (e) { showToast(String(e)); }
  }
}

async function showChanges() {
  st.view = "changes";
  try { st.changes = await invoke("recent_changes"); } catch (e) { showToast(String(e)); }
  render();
}

function closeMenu() {
  menu.hidden = true;
  moreBtn.setAttribute("aria-expanded", "false");
}

async function go(what) {
  closeMenu();
  if (what.startsWith("ask:")) return ask(what.slice(4));
  if (what.startsWith("classic:")) return (location.href = "classic.html#" + what.slice(8));
  if (what === "changes") return showChanges();
  if (what === "new-chat") {
    await invoke("new_chat");
    st.turns = [];
    st.view = "chat";
    render();
    return q.focus();
  }
  if (what === "pause") {
    const paused = st.ctx && st.ctx.paused;
    await invoke("set_paused", { paused: !paused });
    refreshContext();
  }
}

function setPausedUi(p) {
  recDot.classList.toggle("paused", p);
  pauseItem.textContent = p ? "Resume recording" : "Pause recording";
  moreBtn.title = p ? "More (recording is paused)" : "More (recording)";
}

// ---------- events ----------

q.addEventListener("input", () => {
  st.view = "chat";
  clearTimeout(quickTimer);
  quickTimer = setTimeout(quickSearch, 120);
  if (!q.value.trim()) render();
});

document.getElementById("send").addEventListener("click", () => { ask(); q.focus(); });

moreBtn.addEventListener("click", (e) => {
  e.stopPropagation();
  const open = menu.hidden;
  menu.hidden = !open;
  moreBtn.setAttribute("aria-expanded", String(open));
  if (open) menu.querySelector("button")?.focus();
});
menu.querySelectorAll("button[data-go]").forEach((b) => b.addEventListener("click", () => go(b.dataset.go).catch((e) => showToast(String(e)))));
document.addEventListener("click", (e) => { if (!menu.hidden && !menu.contains(e.target)) closeMenu(); });

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    e.preventDefault();
    if (!menu.hidden) { closeMenu(); return moreBtn.focus(); }
    if (st.view === "changes") { st.view = "chat"; return render(); }
    if (q.value) { q.value = ""; st.quick = { query: "", files: [], sel: -1 }; return render(); }
    return hide();
  }
  if (e.target !== q) return;
  const quick = q.value.trim() && st.quick.query === q.value.trim() ? st.quick : null;
  if (e.key === "ArrowDown" && quick && quick.files.length) {
    e.preventDefault();
    quick.sel = Math.min(quick.files.length - 1, quick.sel + 1);
    render();
  } else if (e.key === "ArrowUp" && quick && quick.sel >= 0) {
    e.preventDefault();
    quick.sel -= 1;
    render();
  } else if (e.key === "Enter") {
    e.preventDefault();
    if (quick && quick.sel >= 0) openPath(quick.files[quick.sel].path, e.ctrlKey).catch((err) => showToast(String(err)));
    else ask();
  }
});

// Like the Start menu: clicking elsewhere hides MemStor.
window.addEventListener("blur", () => {
  setTimeout(() => { if (!document.hasFocus()) hide(); }, 120);
});

listen("window-shown", () => {
  st.off.clear();
  q.focus();
  q.select();
  refreshContext();
});
listen("context-ready", refreshContext);
listen("paused-changed", (e) => { setPausedUi(e.payload); refreshContext(); });

// ---------- start ----------

(async () => {
  try {
    const s = await invoke("status");
    setPausedUi(s.paused);
    document.getElementById("hotkey").textContent = s.hotkey ? `Open anywhere with ${s.hotkey}` : "Open from the tray icon";
  } catch (e) {
    showToast(String(e));
  }
  await refreshContext();
  render();
  q.focus();
})();
