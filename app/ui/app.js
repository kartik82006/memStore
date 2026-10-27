// MemStor search window. Talks to the Rust side through Tauri commands.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const q = document.getElementById("q");
const list = document.getElementById("list");
const recBtn = document.getElementById("rec");
const recLabel = document.getElementById("rec-label");
const toast = document.getElementById("toast");
const tabs = [...document.querySelectorAll(".tabs button")];

let view = "search"; // "search", or days ago as a string ("0", "1")
let rows = [];
let sel = 0;
let paused = false;
let searchTimer;

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
    box.append("MemStor is watching your Desktop, Documents and Downloads. Open or save a file and it will show up here.");
  } else {
    box.append(el("strong", null, view === "0" ? "Nothing touched today yet" : "Nothing recorded yesterday"));
    box.append(paused ? "Recording is paused." : "Files you open, save or rename will be listed here.");
  }
  return box;
}

function render() {
  list.replaceChildren();
  if (!rows.length) {
    list.append(emptyState());
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
    if (r.kinds) {
      const chips = el("div", "chips");
      r.kinds.forEach((k) => chips.append(el("span", "chip " + k, k)));
      row.append(chips);
    } else {
      row.append(el("div"));
    }
    row.addEventListener("click", () => select(i));
    row.addEventListener("dblclick", () => openSelected(false));
    list.append(row);
  });
}

function select(i) {
  if (!rows.length) return;
  sel = Math.max(0, Math.min(rows.length - 1, i));
  render();
  list.children[sel]?.scrollIntoView({ block: "nearest" });
}

// ---------- data ----------

async function load() {
  try {
    rows = view === "search"
      ? await invoke("search", { query: q.value })
      : await invoke("day", { daysAgo: Number(view) });
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
  tabs.forEach((t) => t.setAttribute("aria-selected", String(t.dataset.view === v)));
  load();
}

function setPausedUi(p) {
  paused = p;
  recBtn.classList.toggle("paused", p);
  recLabel.textContent = p ? "Paused" : "Recording";
  recBtn.title = p ? "Resume recording" : "Pause recording";
}

// ---------- events ----------

q.addEventListener("input", () => {
  if (view !== "search") setView("search");
  clearTimeout(searchTimer);
  searchTimer = setTimeout(load, 120);
});

document.addEventListener("keydown", (e) => {
  if (e.key === "ArrowDown") { e.preventDefault(); select(sel + 1); }
  else if (e.key === "ArrowUp") { e.preventDefault(); select(sel - 1); }
  else if (e.key === "Enter") { e.preventDefault(); openSelected(e.ctrlKey); }
  else if (e.key === "Escape") {
    if (q.value) { q.value = ""; load(); } else { invoke("hide_window"); }
  }
});

tabs.forEach((t) => t.addEventListener("click", () => { setView(t.dataset.view); q.focus(); }));

recBtn.addEventListener("click", () => invoke("set_paused", { paused: !paused }));

listen("paused-changed", (e) => { setPausedUi(e.payload); if (view !== "search") load(); });
listen("window-shown", () => { q.focus(); q.select(); load(); });
window.addEventListener("focus", () => q.focus());

// ---------- start ----------

(async () => {
  try {
    const s = await invoke("status");
    setPausedUi(s.paused);
    document.getElementById("hotkey").textContent = s.hotkey ? `Open anywhere with ${s.hotkey}` : "Open from the tray icon";
  } catch (e) {
    showToast(String(e));
  }
  load();
})();
