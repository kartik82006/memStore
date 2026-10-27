// Projects tab: name a project, point it at its folders, see the files used for it on a day
// (including files elsewhere used around the same time), and gather them into one folder.
// Uses invoke, el, hm, KIND_LABEL and showToast from app.js.

(() => {
  const RANGES = [["today", "Today"], ["yesterday", "Yesterday"], ["week", "Last 7 days"]];

  const st = {
    projects: [],
    suggestions: [],
    selected: null, // project id
    range: "today",
    files: [],
    unchecked: new Set(), // paths the user left out of the next gather
    creating: false,
    newFolder: null,
    confirmDelete: false,
    busy: false,
  };
  let root;

  async function refresh() {
    try {
      [st.projects, st.suggestions] = await Promise.all([invoke("list_projects"), invoke("suggest_projects")]);
      if (st.selected != null && !st.projects.some((p) => p.id === st.selected)) st.selected = null;
      if (st.selected == null && st.projects.length) st.selected = st.projects[0].id;
      st.files = st.selected != null ? await invoke("project_files", { id: st.selected, range: st.range }) : [];
    } catch (e) {
      showToast(String(e));
    }
    render();
  }

  function selected() {
    return st.projects.find((p) => p.id === st.selected);
  }

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

  // ---------- side: project list, new project, suggestions ----------

  function newProjectForm() {
    const form = el("form", "pj-new");
    const name = el("input");
    name.id = "pj-new-name";
    name.placeholder = "Project name";
    name.autocomplete = "off";
    const folderLine = el("div", "pj-new-folder", st.newFolder || "No folder chosen yet");
    folderLine.title = st.newFolder || "";
    const pick = button("Choose folder…", "btn", () =>
      act(async () => {
        const f = await invoke("pick_folder");
        if (f) {
          st.newFolder = f;
          if (!name.value.trim()) name.value = f.split(/[\\/]/).pop();
          folderLine.textContent = f;
          folderLine.title = f;
        }
      })
    );
    const create = el("button", "btn primary", "Create");
    create.type = "submit";
    const cancel = button("Cancel", "btn", () => { st.creating = false; st.newFolder = null; render(); });
    form.append(name, pick, folderLine, el("div", "pj-row", null));
    form.lastChild.append(create, cancel);
    form.addEventListener("submit", (e) => {
      e.preventDefault();
      act(async () => {
        st.selected = await invoke("create_project", { name: name.value, folder: st.newFolder });
        st.creating = false;
        st.newFolder = null;
        await refresh();
      });
    });
    setTimeout(() => name.focus(), 0);
    return form;
  }

  function side() {
    const aside = el("aside", "pj-side");
    const head = el("div", "pj-head");
    head.append(el("h3", null, "Projects"));
    if (!st.creating) head.append(button("+ New", "btn", () => { st.creating = true; render(); }));
    aside.append(head);
    if (st.creating) aside.append(newProjectForm());

    const ul = el("div", "pj-list");
    st.projects.forEach((p) => {
      const b = button("", "pj-item" + (p.id === st.selected ? " on" : ""), () => {
        st.selected = p.id;
        st.unchecked.clear();
        st.confirmDelete = false;
        refresh();
      });
      b.append(el("span", "pj-name", p.name), el("span", "pj-count", p.today ? String(p.today) : ""));
      b.title = p.today ? `${p.today} file(s) used today` : "No files used today";
      ul.append(b);
    });
    if (!st.projects.length && !st.creating) {
      ul.append(el("p", "pj-muted", "No projects yet. Create one, or pick a suggested folder below."));
    }
    aside.append(ul);

    if (st.suggestions.length) {
      aside.append(el("h4", null, "Folders you worked in this week"));
      st.suggestions.forEach((s) => {
        const row = el("div", "pj-sug");
        const text = el("div", "pj-sug-text");
        text.append(el("span", "pj-name", s.name), el("span", "pj-muted", `${s.files} files`));
        text.title = s.folder;
        row.append(
          text,
          button("Make project", "btn small", () =>
            act(async () => {
              st.selected = await invoke("create_project", { name: s.name, folder: s.folder });
              await refresh();
              showToast(`Created project "${s.name}"`);
            }),
            s.folder
          )
        );
        aside.append(row);
      });
    }
    return aside;
  }

  // ---------- main: one project ----------

  function folders(p) {
    const box = el("div", "pj-folders");
    p.folders.forEach((f) => {
      const chip = el("span", "pj-folder");
      chip.append(el("bdi", null, f));
      chip.title = f;
      chip.append(
        button("×", "pj-x", () =>
          act(async () => {
            await invoke("remove_project_folder", { id: p.id, folder: f });
            await refresh();
          }),
          "Remove this folder from the project"
        )
      );
      box.append(chip);
    });
    box.append(
      button("+ Add folder", "btn small", () =>
        act(async () => {
          const f = await invoke("pick_folder");
          if (!f) return;
          await invoke("add_project_folder", { id: p.id, folder: f });
          await refresh();
        })
      )
    );
    return box;
  }

  function fileRow(f) {
    const row = el("label", "row pj-file" + (st.unchecked.has(f.path) ? " off" : ""));
    row.title = f.path;
    const cb = el("input");
    cb.type = "checkbox";
    cb.checked = !st.unchecked.has(f.path);
    cb.addEventListener("change", () => {
      if (cb.checked) st.unchecked.delete(f.path);
      else st.unchecked.add(f.path);
      render();
    });
    const body = el("div", "pj-file-body");
    const top = el("div", "pj-file-top");
    top.append(el("span", "name", f.name), el("span", "when", f.first === f.last ? hm(f.first) : `${hm(f.first)}–${hm(f.last)}`));
    const bottom = el("div", "pj-file-top");
    const folder = el("span", "folder");
    folder.append(el("bdi", null, f.folder));
    const chips = el("span", "chips");
    if (f.reason === "nearby") {
      const c = el("span", "chip nearby", "same time");
      c.title = "Not in the project's folders, but used within 30 minutes of its files";
      chips.append(c);
    }
    f.kinds.forEach((k) => chips.append(el("span", "chip " + k, KIND_LABEL[k] || k)));
    bottom.append(folder, chips);
    body.append(top, bottom);
    row.append(cb, body);
    row.addEventListener("dblclick", (e) => {
      e.preventDefault();
      invoke("open_file", { path: f.path }).catch((err) => showToast(String(err)));
    });
    return row;
  }

  function main() {
    const sec = el("section", "pj-main");
    const p = selected();
    if (!p) {
      const box = el("div", "empty");
      box.append(el("strong", null, "Group files by project"));
      box.append(
        "Give a project a name and the folder where its files live. MemStor then lists everything you used for it each day, " +
          "including files in Downloads or on the Desktop that you used around the same time, and can gather them into one folder."
      );
      sec.append(box);
      return sec;
    }

    const head = el("div", "pj-title");
    head.append(el("h2", null, p.name));
    head.append(
      st.confirmDelete
        ? button("Click again to delete", "btn danger", () =>
            act(async () => {
              await invoke("delete_project", { id: p.id });
              st.confirmDelete = false;
              st.selected = null;
              await refresh();
            })
          )
        : button("Delete project", "btn subtle", () => { st.confirmDelete = true; render(); }, "Your files are not touched")
    );
    sec.append(head, folders(p));

    const seg = el("div", "seg");
    RANGES.forEach(([key, label]) =>
      seg.append(
        button(label, key === st.range ? "on" : "", () => {
          st.range = key;
          st.unchecked.clear();
          refresh();
        })
      )
    );
    sec.append(seg);

    const listBox = el("div", "pj-files");
    if (!p.folders.length) {
      listBox.append(el("p", "pj-muted", "Add the folder where this project's files live, and its files will show up here."));
    } else if (!st.files.length) {
      const range = RANGES.find(([k]) => k === st.range)[1].toLowerCase();
      listBox.append(el("p", "pj-muted", `No files used for ${p.name} ${range === "last 7 days" ? "in the last 7 days" : range}.`));
    } else {
      st.files.forEach((f) => listBox.append(fileRow(f)));
    }
    sec.append(listBox);

    const chosen = st.files.filter((f) => !st.unchecked.has(f.path));
    const bar = el("div", "pj-bar");
    const gather = button(`Gather ${chosen.length} file${chosen.length === 1 ? "" : "s"} into a folder`, "btn primary", () =>
      act(async () => {
        const g = await invoke("gather_files", { id: p.id, range: st.range, paths: chosen.map((f) => f.path) });
        const missing = g.missing ? ` ${g.missing} file(s) no longer exist and were skipped.` : "";
        showToast(`Made ${g.linked} shortcut(s) in ${g.folder}.${missing}`);
      }),
      "Makes a new folder of shortcuts in Documents\\MemStor Gathered. Your files stay where they are."
    );
    gather.disabled = !chosen.length;
    bar.append(gather, el("span", "pj-muted", "Shortcuts only. Your files stay where they are."));
    sec.append(bar);
    return sec;
  }

  function render() {
    if (!root || view !== "projects") return;
    root.replaceChildren(el("div", "pj"));
    root.firstChild.append(side(), main());
  }

  window.Projects = {
    show(container) {
      root = container;
      render();
      return refresh();
    },
  };
})();
