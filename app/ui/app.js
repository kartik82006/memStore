// MemStor search window. Talks to the Rust side through Tauri commands.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const q = document.getElementById("q");
const list = document.getElementById("list");
const recBtn = document.getElementById("rec");
const recLabel = document.getElementById("rec-label");
const toast = document.getElementById("toast");
const tabs = [...document.querySelectorAll(".tabs button")];
const askBtn = document.getElementById("ask");
const askHint = document.getElementById("ask-hint");

let view = "search"; // "search", "activities", "projects", "settings", or days ago as a string ("0", "1")
let rows = [];
let recalled = null; // { understood, results } for a plain-language search
let sel = 0;
let paused = false;
let watching = [];
let searchTimer;
let cloudReady = false; // cloud descriptions on with a working key
let smart = null; // { question, loading, answer } for an "Ask AI" search

// ---------- formatting ----------

function hm(ts) {
  return new Date(ts * 1000).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

function when(ts) {
  const d = new Date(ts * 1000);
  const now = new Date();
  if (d.toDateString() === now.toDateString()) return hm(ts);
  const y = new Date(now);
  y.setDate(now.getDate() - 1);
  if (d.toDateString() === y.toDateString()) return "Yesterday " + hm(ts);
  return d.toLocaleDateString([], { day: "numeric", month: "short" }) + " " + hm(ts);
}

// Words people use, not the database's event names.
const KIND_LABEL = { created: "new", modified: "edited", renamed: "renamed", opened: "opened", deleted: "deleted" };

function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.textContent = text;
  return e;
}

function showToast(msg) {
  toast.textContent = msg;
  toast.hidden = false;
  clearTimeout(showToast.t);
  showToast.t = setTimeout(() => (toast.hidden = true), 3500);
}

// ---------- rendering ----------

function emptyState() {
  const box = el("div", "empty");
  if (view === "search" && q.value.trim()) {
    box.append(el("strong", null, `Nothing matches "${q.value.trim()}"`));
    box.append("Try part of the file name, or a folder it was in.");
  } else if (view === "search") {
    box.append(el("strong", null, "No files recorded yet"));
    box.append("MemStor is watching your user folders. Open, save or edit a document, sheet, slide deck, PDF, image or video and it will show up here.");
  } else {
    box.append(el("strong", null, view === "0" ? "Nothing touched today yet" : "Nothing recorded yesterday"));
    box.append(paused ? "Recording is paused." : "Files you open, edit or rename will be listed here.");
  }
  return box;
}

// "Do you remember…" results: saved memories that fit the question.
function memoriesSection() {
  const sec = el("section", "mem-section");
  const head = el("div", "mem-head");
  head.append(el("strong", null, recalled.results.length ? "Memories" : "No memories match"));
  if (recalled.understood.length) {
    const u = el("span", "mem-understood");
    u.append("Looking for: ");
    recalled.understood.forEach((w) => u.append(el("span", "chip", w)));
    head.append(u);
  }
  sec.append(head);
  recalled.results.forEach((m) => sec.append(window.Memories.card(m, load)));
  if (rows.length) {
    const files = el("div", "mem-head");
    files.append(el("strong", null, "Files"));
    sec.append(files);
  }
  return sec;
}

// "Ask AI" results: files and memories Claude picked, each with why it matched.
function smartSection() {
  const sec = el("section", "mem-section smart");
  const head = el("div", "mem-head");
  if (smart.loading) {
    head.append(el("strong", null, "Asking Claude…"), el("span", "mem-understood", "Reading your question and checking the best matches."));
    sec.append(head);
    return sec;
  }
  const a = smart.answer;
  head.append(el("strong", null, a.results.length ? "Best matches" : "Nothing matches that memory yet"));
  if (a.understood.length) {
    const u = el("span", "mem-understood");
    u.append("Looking for: ");
    a.understood.forEach((w) => u.append(el("span", "chip", w)));
    head.append(u);
  }
  sec.append(head);
  if (a.local_only) sec.append(el("p", "st-warn", a.local_only));
  a.results.forEach((f) => {
    const card = el("article", "ac-card sm-card" + (f.path && !f.exists ? " gone" : ""));
    const top = el("header", "ac-head");
    const title = el("div", "ac-title");
    title.append(el("span", "ac-work " + (f.path ? "files" : "memory"), f.path ? "File" : "Memory"));
    title.append(el("h3", null, f.path ? f.name : f.title));
    top.append(title, el("span", "ac-meta", when(f.date)));
    card.append(top);
    if (f.why) card.append(el("p", "sm-why", f.why));
    if (f.summary) card.append(el("p", "sm-summary", f.summary));
    if (f.path) {
      const folder = el("div", "folder sm-folder");
      folder.append(el("bdi", null, f.folder));
      card.append(folder);
    }
    const bar = el("div", "ac-bar");
    const act = (text, cls, fn) => {
      const b = el("button", cls, text);
      b.type = "button";
      b.addEventListener("click", async () => { try { await fn(); } catch (e) { showToast(String(e)); } });
      bar.append(b);
    };
    if (f.path && f.exists) {
      act("Open", "btn primary", async () => { await invoke("open_file", { path: f.path }); invoke("hide_window"); });
      act("Show in folder", "btn", async () => { await invoke("show_in_folder", { path: f.path }); invoke("hide_window"); });
    } else if (f.path) {
      bar.append(el("span", "pj-muted", "This file is no longer there."));
    }
    if (f.memory_id != null) {
      act("Gather its files", f.path ? "btn" : "btn primary", async () => {
        const g = await invoke("gather_memory", { id: f.memory_id });
        showToast(`Made ${g.linked} shortcut(s) in ${g.folder}.`);
      });
    }
    card.append(bar);
    sec.append(card);
  });
  if (a.cost) sec.append(el("p", "pj-muted sm-cost", `This search cost ${a.cost}.`));
  return sec;
}

async function ask() {
  const question = q.value.trim();
  if (!question || view !== "search") return;
  smart = { question, loading: true };
  render();
  try {
    const answer = await invoke("smart_search", { question });
    if (smart?.question !== question) return;
    smart = { question, loading: false, answer };
  } catch (e) {
    smart = null;
    showToast(String(e));
  }
  render();
}

function updateAsk() {
  const show = cloudReady && view === "search" && q.value.trim().split(/\s+/).length >= 2;
  askBtn.hidden = !show;
  askHint.hidden = !show;
}

async function refreshCloud() {
  try {
    const c = await invoke("cloud_status");
    cloudReady = c.on && c.has_key && !c.key_problem;
  } catch (_) {
    cloudReady = false;
  }
  updateAsk();
}

function render() {
  list.replaceChildren();
  if (view === "search" && smart && smart.question === q.value.trim()) list.append(smartSection());
  const showMemories = view === "search" && recalled && (recalled.results.length || recalled.understood.length);
  if (showMemories) list.append(memoriesSection());
  if (!rows.length) {
    if (!showMemories || !recalled.results.length) list.append(emptyState());
    return;
  }
  rows.forEach((r, i) => {
    const row = el("div", "row" + (i === sel ? " sel" : ""));
    row.title = r.path;
    row.append(el("div", "name", r.name));
    if (view === "search") {
      row.append(el("div", "when", when(r.when)));
    } else {
      row.append(el("div", "when", r.first === r.last ? hm(r.first) : `${hm(r.first)}–${hm(r.last)}`));
    }
    // RTL keeps the end of a long folder path visible; the bdi stops punctuation from flipping.
    const folder = el("div", "folder");
    folder.append(el("bdi", null, r.folder));
    row.append(folder);
    const chips = el("div", "chips");
    (r.kinds || [r.kind]).forEach((k) => chips.append(el("span", "chip " + k, KIND_LABEL[k] || k)));
    row.append(chips);
    row.addEventListener("click", () => select(i));
    row.addEventListener("dblclick", () => openSelected(false));
    list.append(row);
  });
}

// Projects and Activities are panels with their own controls, not a keyboard list.
function isPanel() {
  return view === "projects" || view === "activities" || view === "settings";
}

function select(i) {
  if (!rows.length) return;
  sel = Math.max(0, Math.min(rows.length - 1, i));
  render();
  list.querySelectorAll(":scope > .row")[sel]?.scrollIntoView({ block: "nearest" });
}

// ---------- data ----------

async function load() {
  if (isPanel()) {
    document.body.classList.add("projects-view");
    const panel = { projects: window.Projects, activities: window.Activities, settings: window.Settings }[view];
    return panel.show(list);
  }
  document.body.classList.remove("projects-view");
  try {
    // Two or more words may be a question about the past: also ask the memories.
    const asksMemory = view === "search" && q.value.trim().split(/\s+/).length >= 2;
    const [r, m] = await Promise.all([
      view === "search" ? invoke("search", { query: q.value }) : invoke("day", { daysAgo: Number(view) }),
      asksMemory ? invoke("recall", { query: q.value }) : null,
    ]);
    rows = r;
    recalled = m;
  } catch (e) {
    rows = [];
    showToast(String(e));
  }
  sel = 0;
  render();
}

async function openSelected(inFolder) {
  const r = rows[sel];
  if (!r) return;
  try {
    await invoke(inFolder ? "show_in_folder" : "open_file", { path: r.path });
    invoke("hide_window");
  } catch (e) {
    showToast(String(e));
  }
}

function setView(v) {
  view = v;
  if (v === "search") refreshCloud();
  updateAsk();
  tabs.forEach((t) => t.setAttribute("aria-selected", String(t.dataset.view === v)));
  load();
}

function setPausedUi(p) {
  paused = p;
  recBtn.classList.toggle("paused", p);
  recLabel.textContent = p ? "Paused" : "Recording";
  recBtn.title = (p ? "Resume recording" : "Pause recording") + (watching.length ? "\n\nWatching:\n" + watching.join("\n") : "");
}

// ---------- events ----------

q.addEventListener("input", () => {
  if (view !== "search") setView("search");
  if (smart && smart.question !== q.value.trim()) smart = null;
  updateAsk();
  clearTimeout(searchTimer);
  searchTimer = setTimeout(load, 120);
});

document.addEventListener("keydown", (e) => {
  // Typing in another box (e.g. renaming a memory) must not trigger list shortcuts.
  if (e.target.tagName === "INPUT" && e.target !== q) return;
  if (isPanel()) {
    // Typing in a project's name box must not trigger list shortcuts.
    if (e.key === "Escape" && e.target.tagName !== "INPUT") invoke("hide_window");
    return;
  }
  if (e.key === "ArrowDown") { e.preventDefault(); select(sel + 1); }
  else if (e.key === "ArrowUp") { e.preventDefault(); select(sel - 1); }
  else if (e.key === "Enter" && e.shiftKey) { e.preventDefault(); if (cloudReady) ask(); }
  else if (e.key === "Enter") { e.preventDefault(); openSelected(e.ctrlKey); }
  else if (e.key === "Escape") {
    if (q.value) { q.value = ""; smart = null; updateAsk(); load(); } else { invoke("hide_window"); }
  }
});

tabs.forEach((t) => t.addEventListener("click", () => {
  setView(t.dataset.view);
  if (!isPanel()) q.focus();
}));

askBtn.addEventListener("click", () => { ask(); q.focus(); });
recBtn.addEventListener("click", () => invoke("set_paused", { paused: !paused }));

listen("paused-changed", (e) => { setPausedUi(e.payload); if (view !== "search") load(); });
listen("window-shown", () => { if (!isPanel()) { q.focus(); q.select(); } refreshCloud(); load(); });
window.addEventListener("focus", () => { if (!isPanel()) q.focus(); });

// ---------- start ----------

(async () => {
  try {
    const s = await invoke("status");
    watching = s.folders;
    setPausedUi(s.paused);
    document.getElementById("hotkey").textContent = s.hotkey ? `Open anywhere with ${s.hotkey}` : "Open from the tray icon";
  } catch (e) {
    showToast(String(e));
  }
  refreshCloud();
  load();
})();
