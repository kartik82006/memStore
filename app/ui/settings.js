// Settings tab: what MemStor records, how long it keeps it, and deleting it.
// Uses invoke, el and showToast from app.js.

(() => {
  const KEEP = [[90, "3 months"], [180, "6 months"], [365, "1 year"], [0, "Forever"]];
  const st = { s: null, confirmWipe: false, showWatching: false, busy: false };
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
    await refresh();
  }

  async function refresh() {
    try {
      st.s = await invoke("get_settings");
    } catch (e) {
      showToast(String(e));
    }
    render();
  }

  function size(bytes) {
    if (bytes < 1024 * 1024) return `${Math.max(1, Math.round(bytes / 1024))} KB`;
    return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  }

  function section(title, note) {
    const sec = el("section", "st-sec");
    sec.append(el("h3", null, title));
    if (note) sec.append(el("p", "pj-muted", note));
    return sec;
  }

  function folderChips(list, onRemove, removeTitle) {
    const box = el("div", "pj-folders");
    list.forEach((f) => {
      const chip = el("span", "pj-folder");
      chip.append(el("bdi", null, f));
      chip.title = f;
      if (onRemove) chip.append(button("×", "pj-x", () => onRemove(f), removeTitle));
      box.append(chip);
    });
    return box;
  }

  function render() {
    if (!root || view !== "settings") return;
    const s = st.s;
    const wrap = el("div", "st");
    if (!s) {
      root.replaceChildren(wrap);
      return;
    }

    // Recording
    const rec = section("Recording");
    const auto = el("label", "st-toggle");
    const cb = el("input");
    cb.type = "checkbox";
    cb.checked = s.autostart;
    cb.addEventListener("change", () =>
      act(async () => {
        const on = await invoke("set_autostart", { on: cb.checked });
        showToast(on ? "MemStor will start with Windows." : "MemStor won't start with Windows.");
      })
    );
    auto.append(cb, el("span", null, "Start MemStor when Windows starts"));
    rec.append(auto);
    const watchLine = el("div", "st-row");
    watchLine.append(
      el("span", null, `Watching ${s.watching.length} folder${s.watching.length === 1 ? "" : "s"}: your user folder (except AppData) and other drives.`),
      button(st.showWatching ? "Hide" : "Show", "btn small subtle", () => { st.showWatching = !st.showWatching; render(); })
    );
    rec.append(watchLine);
    if (st.showWatching) rec.append(folderChips(s.watching));
    rec.append(el("h4", null, "Also watch"));
    if (s.extra.length) {
      rec.append(folderChips(s.extra, (f) => act(() => invoke("remove_extra_folder", { folder: f })), "Stop watching this folder"));
    }
    rec.append(
      button("+ Add folder", "btn small", () =>
        act(async () => {
          const f = await invoke("pick_folder");
          if (f) await invoke("add_extra_folder", { folder: f });
        }),
        "For a USB or network drive that isn't watched yet"
      )
    );
    wrap.append(rec);

    // Never record
    const ex = section("Never record", "Files in these folders are never recorded. Adding a folder also deletes what MemStor already recorded there.");
    if (s.excluded.length) ex.append(folderChips(s.excluded, (f) => act(() => invoke("include_folder", { folder: f })), "Record this folder again"));
    ex.append(
      button("+ Add folder", "btn small", () =>
        act(async () => {
          const f = await invoke("pick_folder");
          if (!f) return;
          const n = await invoke("exclude_folder", { folder: f });
          showToast(`Won't record ${f}. Forgot ${n} file${n === 1 ? "" : "s"} recorded there.`);
        })
      )
    );
    wrap.append(ex);

    // Keep history
    const keep = section("Keep history for", "Older file history and memories are deleted automatically.");
    const seg = el("div", "seg");
    KEEP.forEach(([days, label]) =>
      seg.append(
        button(label, days === s.keep_days ? "on" : "", () =>
          act(async () => {
            const n = await invoke("set_keep_days", { days });
            if (n) showToast(`Deleted ${n} old event${n === 1 ? "" : "s"}.`);
          })
        )
      )
    );
    keep.append(seg);
    wrap.append(keep);

    // Your data
    const data = section("Your data");
    const facts = el("ul", "st-facts");
    [
      "Stays on this PC. MemStor has no internet access and sends nothing anywhere.",
      s.encrypted
        ? "Encrypted. The key is locked to your Windows account, so a copy of the file is unreadable elsewhere."
        : "Not encrypted.",
      "MemStor records file names, folders and times only. It never reads or copies what's inside your files.",
    ].forEach((t) => facts.append(el("li", null, t)));
    data.append(facts);
    const where = el("p", "pj-muted", `${size(s.db_bytes)} at `);
    where.append(el("bdi", "st-path", s.db_path));
    data.append(where);
    data.append(
      st.confirmWipe
        ? (() => {
            const row = el("div", "st-row");
            row.append(
              button("Delete all history", "btn danger", () =>
                act(async () => {
                  await invoke("wipe_history");
                  st.confirmWipe = false;
                  showToast("All file history and memories were deleted. Recording carries on from now.");
                })
              ),
              button("Cancel", "btn", () => { st.confirmWipe = false; render(); }),
              el("span", "pj-muted", "Projects and settings stay. Your files are not touched.")
            );
            return row;
          })()
        : button("Delete all history…", "btn", () => { st.confirmWipe = true; render(); })
    );
    wrap.append(data);

    root.replaceChildren(wrap);
  }

  window.Settings = {
    show(container) {
      root = container;
      render();
      return refresh();
    },
  };
})();
