// Settings tab: what MemStor records, how long it keeps it, and deleting it.
// Uses invoke, el and showToast from app.js.

(() => {
  const KEEP = [[90, "3 months"], [180, "6 months"], [365, "1 year"], [0, "Forever"]];
  const st = { s: null, c: null, confirmWipe: false, confirmForget: false, showWatching: false, busy: false, keyDraft: "", checking: false };
  let poll;
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
      [st.s, st.c] = await Promise.all([invoke("get_settings"), invoke("cloud_status")]);
    } catch (e) {
      showToast(String(e));
    }
    render();
  }

  // While Settings is open, keep the describing progress fresh.
  function startPolling() {
    clearInterval(poll);
    poll = setInterval(async () => {
      if (view !== "settings") return clearInterval(poll);
      if (st.busy || st.checking || document.activeElement?.tagName === "INPUT") return;
      try {
        st.c = await invoke("cloud_status");
        render();
      } catch (_) {}
    }, 4000);
  }

  function money(micro) {
    return `$${(micro / 1e6).toFixed(2)}`;
  }

  const STATE_TEXT = {
    off: "Off.",
    no_key: "Waiting for an API key.",
    key_problem: "Stopped: the API key needs attention.",
    paused: "Paused while recording is paused.",
    scanning: "Finding photos, videos and documents…",
    waiting_idle: "Waiting until you step away from the PC (2 minutes without typing or the mouse).",
    describing: "Describing files…",
    cap: "Stopped at the spending limit.",
    done: "Up to date. New files are described as they appear.",
  };

  function cloudSection() {
    const c = st.c;
    const sec = section(
      "Search by memory (AI)",
      "Lets you search the way you remember things: \"girl in pink doing yoga\", \"the alumni meet edit\". Claude looks at a small preview of each photo and video and reads the text of documents, then writes a short description. Descriptions are stored only on this PC, encrypted."
    );
    if (!c) return sec;

    // Shown until it's on, so the choice is made knowing what is sent.
    if (!c.on) {
      const box = el("div", "st-confirm");
      const ul = el("ul", "st-facts");
      [
        "Sent to Anthropic's Claude API: a small preview (max 768 px) of photos and one frame of each video, and up to 3,000 words of each document's text.",
        "Never sent: files in \"Never record\" or \"Keep on this PC\" folders, screenshots, duplicates, and documents that look like IDs, bank or card details or passwords.",
        "Anthropic doesn't train on API data. Your files stay on this PC; only the descriptions come back.",
        "You pay Anthropic with your own API key. MemStor stops at $5 for the first pass, then $1 a month, and asks before spending more.",
      ].forEach((t) => ul.append(el("li", null, t)));
      box.append(ul);
      sec.append(box);
    }

    if (c.has_key) {
      const on = el("label", "st-toggle");
      const cb = el("input");
      cb.type = "checkbox";
      cb.checked = c.on;
      cb.addEventListener("change", () => act(() => invoke("set_cloud_on", { on: cb.checked })));
      on.append(cb, el("span", null, "Describe my files with Claude"));
      sec.append(on);
    }

    // API key
    sec.append(el("h4", null, "Claude API key"));
    if (c.has_key && !st.keyDraft) {
      const row = el("div", "st-row");
      row.append(
        el("span", "st-key", `Saved key ending in …${c.key_end}`),
        button("Change", "btn small", () => { st.keyDraft = " "; render(); }),
        button("Remove", "btn small subtle", () => act(() => invoke("clear_api_key")))
      );
      sec.append(row);
    } else {
      const form = el("form", "st-row");
      const input = el("input", "st-input");
      input.type = "password";
      input.placeholder = "sk-ant-…";
      input.value = st.keyDraft.trim();
      input.setAttribute("aria-label", "Claude API key");
      input.addEventListener("input", () => (st.keyDraft = input.value || " "));
      const label = c.has_key ? "Save and test" : "Save key and turn on";
      const save = button(st.checking ? "Checking…" : label, "btn small primary", () => form.requestSubmit());
      save.disabled = st.checking;
      form.append(input, save);
      if (c.has_key) form.append(button("Cancel", "btn small", () => { st.keyDraft = ""; render(); }));
      form.addEventListener("submit", async (e) => {
        e.preventDefault();
        st.checking = true;
        render();
        try {
          await invoke("set_api_key", { key: input.value });
          if (!c.has_key) await invoke("set_cloud_on", { on: true });
          st.keyDraft = "";
          showToast("Key works and Search by memory is on. Describing starts when you step away from the PC.");
        } catch (err) {
          showToast(String(err));
        } finally {
          st.checking = false;
        }
        refresh();
      });
      sec.append(form);
      const help = el("p", "pj-muted", "Get one at console.anthropic.com → API keys. It is stored encrypted in MemStor's database on this PC.");
      sec.append(help);
    }
    if (c.key_problem) sec.append(el("p", "st-warn", c.key_problem));

    if (!c.has_key || !c.on) return sec;

    // Progress
    sec.append(el("h4", null, "Progress"));
    sec.append(el("p", null, STATE_TEXT[c.state] || c.state));
    const total = c.pending + c.described;
    if (total) {
      const bar = el("div", "st-bar");
      const fill = el("div");
      fill.style.width = `${Math.round((c.described / total) * 100)}%`;
      bar.append(fill);
      sec.append(bar);
    }
    sec.append(
      el(
        "p",
        "pj-muted",
        `${c.described} described · ${c.pending} to go · ${c.kept_local} kept on this PC · ${c.skipped} skipped (screenshots, copies)` +
          (c.errors ? ` · ${c.errors} couldn't be read` : "")
      )
    );

    // Spending
    sec.append(el("h4", null, c.first_index_done ? "Spent this month" : "Spent on the first pass"));
    const spend = el("div", "st-row");
    const pct = c.cap ? Math.min(100, Math.round((c.spent / c.cap) * 100)) : 0;
    const bar = el("div", "st-bar st-spend" + (pct >= 100 ? " full" : ""));
    const fill = el("div");
    fill.style.width = `${pct}%`;
    bar.append(fill);
    spend.append(el("span", null, `${money(c.spent)} of ${money(c.cap)}`), bar);
    sec.append(spend);
    if (c.state === "cap" || pct >= 95) {
      const row = el("div", "st-row");
      row.append(
        button("Allow $1 more", "btn small", () => act(() => invoke("allow_more"))),
        el("span", "pj-muted", c.first_index_done ? "Or wait: the limit resets next month." : "The first pass describes everything once; after that it costs much less.")
      );
      sec.append(row);
    }
    sec.append(el("p", "pj-muted", `${money(c.spent_total)} spent in total. Charged to your Anthropic account.`));

    const actions = el("div", "st-row");
    actions.append(
      button("Describe now", "btn small", () => act(async () => { await invoke("index_now"); showToast("Describing now, even while you use the PC."); }), "Don't wait for the PC to be idle"),
      button("Look for new files", "btn small subtle", () => act(async () => { await invoke("rescan_library"); showToast("Looking through your folders again."); }))
    );
    sec.append(actions);

    // Local-only folders
    sec.append(el("h4", null, "Keep on this PC"));
    sec.append(el("p", "pj-muted", "Files in these folders are still recorded and searchable by name, but never sent to Claude."));
    if (c.local_only.length) {
      sec.append(folderChips(c.local_only, (f) => act(() => invoke("remove_local_only_folder", { folder: f })), "Allow describing this folder"));
    }
    sec.append(
      button("+ Add folder", "btn small", () =>
        act(async () => {
          const f = await invoke("pick_folder");
          if (!f) return;
          const n = await invoke("add_local_only_folder", { folder: f });
          showToast(n ? `Deleted ${n} description${n === 1 ? "" : "s"} from that folder.` : "Files there won't be sent.");
        })
      )
    );

    // Forget
    sec.append(el("h4", null, "Delete descriptions"));
    sec.append(
      st.confirmForget
        ? (() => {
            const row = el("div", "st-row");
            row.append(
              button("Delete all descriptions", "btn danger", () =>
                act(async () => {
                  await invoke("forget_cloud_descriptions");
                  st.confirmForget = false;
                  showToast("All AI descriptions were deleted. Turn this off too if you don't want them made again.");
                })
              ),
              button("Cancel", "btn", () => { st.confirmForget = false; render(); })
            );
            return row;
          })()
        : button("Delete all descriptions…", "btn small", () => { st.confirmForget = true; render(); })
    );
    return sec;
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

    wrap.append(cloudSection());

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
    const cloudOn = st.c && st.c.on;
    [
      cloudOn
        ? "Stays on this PC, except the previews and text sent to Claude for descriptions (see above). Nothing else is sent anywhere."
        : "Stays on this PC. MemStor sends nothing anywhere.",
      s.encrypted
        ? "Encrypted. The key is locked to your Windows account, so a copy of the file is unreadable elsewhere."
        : "Not encrypted.",
      cloudOn
        ? "MemStor records file names, folders and times, and reads files only to describe them. It never copies or changes your files."
        : "MemStor records file names, folders and times only. It doesn't read what's inside your files.",
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
      startPolling();
      return refresh();
    },
  };
})();
