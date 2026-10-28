// Memory cards: saved activities, shown in search ("the edit before Diwali") and in the
// Activities tab under "All memories". Uses invoke, el, when and showToast from app.js.

(() => {
  const SHOW = 4; // files shown before "Show all"
  const open = new Set(); // memory ids with all files shown
  let renaming = null; // memory id being renamed

  function button(text, cls, onClick, title) {
    const b = el("button", cls, text);
    b.type = "button";
    if (title) b.title = title;
    b.addEventListener("click", (e) => { e.stopPropagation(); onClick(); });
    return b;
  }

  function dateOnly(ts) {
    return new Date(ts * 1000).toLocaleDateString([], { day: "numeric", month: "short", year: "numeric" });
  }

  function span(m) {
    const a = dateOnly(m.start), b = dateOnly(m.end);
    return a === b ? a : `${a} – ${b}`;
  }

  function effort(m) {
    const h = m.active_minutes >= 60 ? `${Math.round(m.active_minutes / 6) / 10} h` : `${m.active_minutes} min`;
    const parts = [`${h} over ${m.days} day${m.days === 1 ? "" : "s"}`];
    if (m.late_minutes >= 30) parts.push("late nights");
    return parts.join(" · ");
  }

  // `refresh` reloads whichever view the card is in after a change.
  function card(m, refresh) {
    const box = el("article", "ac-card mem-card");
    const head = el("header", "ac-head");
    const title = el("div", "ac-title");
    title.append(el("span", "ac-work " + m.work, m.label.split(" · ")[0]));
    if (renaming === m.id) {
      const form = el("form", "mem-rename");
      const input = el("input");
      input.value = m.topic;
      input.setAttribute("aria-label", "New name");
      form.append(input, button("Save", "btn small primary", () => form.requestSubmit()), button("Cancel", "btn small", () => { renaming = null; refresh(); }));
      form.addEventListener("submit", async (e) => {
        e.preventDefault();
        try {
          await invoke("rename_memory", { id: m.id, name: input.value });
          renaming = null;
          refresh();
        } catch (err) {
          showToast(String(err));
        }
      });
      title.append(form);
      setTimeout(() => { input.focus(); input.select(); }, 0);
    } else {
      title.append(el("h3", null, m.topic));
    }
    head.append(title, el("span", "ac-meta", `${span(m)} · ${effort(m)}`));
    box.append(head);
    if (m.story) box.append(el("p", "mem-story", m.story));

    if (m.why.length) {
      const why = el("div", "mem-why");
      m.why.forEach((w) => why.append(el("span", "chip", w)));
      box.append(why);
    }

    const files = el("div", "ac-files");
    const all = open.has(m.id);
    (all ? m.files : m.files.slice(0, SHOW)).forEach((f) => {
      const row = el("div", "mem-file" + (f.exists ? "" : " gone"));
      row.title = f.exists ? f.path : `${f.path}\n(no longer there)`;
      row.append(el("span", "name", f.name));
      const folder = el("span", "folder");
      folder.append(el("bdi", null, f.folder));
      row.append(folder);
      if (f.exists) row.addEventListener("dblclick", () => invoke("open_file", { path: f.path }).catch((e) => showToast(String(e))));
      files.append(row);
    });
    box.append(files);

    const bar = el("div", "ac-bar");
    bar.append(
      button(`Gather ${m.files.length} files`, "btn primary", async () => {
        try {
          const g = await invoke("gather_memory", { id: m.id });
          const missing = g.missing ? ` ${g.missing} file(s) no longer exist and were skipped.` : "";
          showToast(`Made ${g.linked} shortcut(s) in ${g.folder}.${missing}`);
        } catch (e) {
          showToast(String(e));
        }
      }, "Makes a folder of shortcuts in Documents\\MemStor Gathered. Your files stay where they are."),
      button("Rename", "btn", () => { renaming = m.id; refresh(); }, "Name it the way you'd search for it, e.g. the club or client"),
      button("Forget", "btn subtle", async () => {
        try {
          await invoke("forget_memory", { id: m.id });
          showToast(`Forgot "${m.topic}". Your files are not touched.`);
          refresh();
        } catch (e) {
          showToast(String(e));
        }
      }, "Removes this memory from MemStor. Your files are not touched.")
    );
    if (m.files.length > SHOW) {
      bar.append(button(all ? "Show fewer" : `Show all ${m.files.length}`, "btn subtle", () => {
        all ? open.delete(m.id) : open.add(m.id);
        refresh();
      }));
    }
    box.append(bar);
    return box;
  }

  window.Memories = { card, isRenaming: () => renaming != null };
})();
