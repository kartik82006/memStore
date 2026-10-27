// Activities tab: groups of files MemStor worked out belong to the same piece of work
// (a video edit, a report, a set of photos), with no folders to set up.
// Uses invoke, el, hm, when, KIND_LABEL and showToast from app.js.

(() => {
  const RANGES = [["today", "Today"], ["yesterday", "Yesterday"], ["week", "Last 7 days"]];
  const SHOW_FILES = 6; // files shown before "Show all"

  const st = { range: "today", list: [], open: new Set(), busy: false, loaded: false };
  let root;

  function button(text, cls, onClick, title) {
    const b = el("button", cls, text);
    b.type = "button";
    if (title) b.title = title;
    b.addEventListener("click", onClick);
    return b;
  }

  async function act(fn) {
    if (st.busy) return;
    st.busy = true;
    try {
      await fn();
    } catch (e) {
      showToast(String(e));
    } finally {
      st.busy = false;
    }
  }

  async function refresh() {
    try {
      st.list = await invoke("list_activities", { range: st.range });
    } catch (e) {
      st.list = [];
      showToast(String(e));
    }
    st.loaded = true;
    render();
  }

  function span(a) {
    const days = new Date(a.start * 1000).toDateString() === new Date(a.end * 1000).toDateString();
    return days ? `${when(a.start)}–${hm(a.end)}` : `${when(a.start)} – ${when(a.end)}`;
  }

  function fileRow(a, f) {
    const row = el("div", "row ac-file");
    row.title = f.path;
    const body = el("div", "pj-file-body");
    const top = el("div", "pj-file-top");
    top.append(el("span", "name", f.name), el("span", "when", f.first === f.last ? hm(f.first) : `${hm(f.first)}–${hm(f.last)}`));
    const bottom = el("div", "pj-file-top");
    const folder = el("span", "folder");
    folder.append(el("bdi", null, f.folder));
    const chips = el("span", "chips");
    f.kinds.forEach((k) => chips.append(el("span", "chip " + k, KIND_LABEL[k] || k)));
    bottom.append(folder, chips);
    body.append(top, bottom);
    const drop = button("×", "pj-x", () =>
      act(async () => {
        await invoke("drop_from_activity", { path: f.path, others: a.files.map((x) => x.path) });
        showToast(`Won't group ${f.name} with these files again.`);
        await refresh();
      }),
      "Not part of this. MemStor won't group it with these files again."
    );
    row.append(body, drop);
    row.addEventListener("dblclick", () => invoke("open_file", { path: f.path }).catch((e) => showToast(String(e))));
    return row;
  }

  function card(a, i) {
    const key = a.label + a.start;
    const box = el("article", "ac-card");
    const head = el("header", "ac-head");
    const title = el("div", "ac-title");
    title.append(el("span", "ac-work " + a.work, a.label.split(" · ")[0]), el("h3", null, a.topic));
    head.append(title, el("span", "ac-meta", `${a.files.length} files · ${span(a)}`));
    box.append(head);

    const files = el("div", "ac-files");
    const all = st.open.has(key);
    (all ? a.files : a.files.slice(0, SHOW_FILES)).forEach((f) => files.append(fileRow(a, f)));
    box.append(files);

    const bar = el("div", "ac-bar");
    bar.append(
      button(`Gather ${a.files.length} files`, "btn primary", () =>
        act(async () => {
          const g = await invoke("gather_activity", { title: a.topic, range: st.range, paths: a.files.map((f) => f.path) });
          const missing = g.missing ? ` ${g.missing} file(s) no longer exist and were skipped.` : "";
          showToast(`Made ${g.linked} shortcut(s) in ${g.folder}.${missing}`);
        }),
        "Makes a folder of shortcuts in Documents\\MemStor Gathered. Your files stay where they are."
      ),
      button("Save as project", "btn", () =>
        act(async () => {
          await invoke("save_activity_as_project", { name: a.topic, paths: a.files.map((f) => f.path) });
          showToast(`Saved "${a.topic}" as a project. Rename or edit it in Projects.`);
        }),
        "Keeps these files' folders as a named project"
      )
    );
    if (a.files.length > SHOW_FILES) {
      bar.append(
        button(all ? "Show fewer" : `Show all ${a.files.length}`, "btn subtle", () => {
          all ? st.open.delete(key) : st.open.add(key);
          render();
        })
      );
    }
    box.append(bar);
    return box;
  }

  function render() {
    if (!root || view !== "activities") return;
    const wrap = el("div", "ac");
    const top = el("div", "ac-top");
    const seg = el("div", "seg");
    RANGES.forEach(([k, label]) =>
      seg.append(button(label, k === st.range ? "on" : "", () => { st.range = k; refresh(); }))
    );
    top.append(seg, el("span", "pj-muted", "Grouped from when, where and how you used each file. Nothing leaves this PC."));
    wrap.append(top);

    if (st.loaded && !st.list.length) {
      const box = el("div", "empty");
      box.append(el("strong", null, "No activities yet"));
      box.append(
        "When you work on several files together, like footage, music and an editor project, they show up here as one activity you can gather into a folder."
      );
      wrap.append(box);
    }
    st.list.forEach((a, i) => wrap.append(card(a, i)));
    root.replaceChildren(wrap);
  }

  window.Activities = {
    show(container) {
      root = container;
      render();
      return refresh();
    },
  };
})();
