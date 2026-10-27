# MemStor

Find files on your computer by what you were doing, not only by what you named them.

MemStor runs quietly in the Windows tray and records which files you create, edit, rename and
open. Press **Ctrl+Shift+Space** anywhere to search that history or see what you worked on
today. Everything stays in a local database on your PC. Nothing is sent anywhere.

The plan: https://claude.ai/artifact/PQbWgqCHXuf8BgrXnNueuJ

![Search window](docs/search-window.png)

## Get it

Every push builds Windows downloads on GitHub Actions. Open the latest run under
**Actions → Build**, download `memstor-windows`, and run the `MemStor_…_x64-setup.exe` installer.

## Using the tray app (M1)

- MemStor starts hidden in the tray. **Left-click** the icon or press **Ctrl+Shift+Space** to open search.
- Type part of a file name or a folder it was in. With an empty box you see your most recent files.
- **Today** and **Yesterday** list every file you touched, with the time span and what happened.
- **Enter** opens the file, **Ctrl+Enter** shows it in Explorer, **Esc** hides the window.
- Pause or resume recording from the button in the window or the tray menu.
- Closing the window keeps MemStor running. Quit from the tray menu.
- Start it with `memstor-app.exe --show` to open the window right away.

## What it records

| Source | Gives us | Needs admin? |
| --- | --- | --- |
| Folder watcher (Desktop, Documents, Downloads) | files created, saved, renamed, deleted | no |
| Recent Items (`%APPDATA%\Microsoft\Windows\Recent`) | files you opened anywhere on disk | no |

It skips noise: Office lock files (`~$x.docx`), temp and partial downloads, hidden files,
`node_modules`, `.git`, build output and AppData. A save done through a temp file
(`A1B2.tmp` renamed over `report.docx`) counts as an edit of `report.docx`, and a rename
keeps the file's history. Only paths, names and times are stored. File contents are never read.

The database is `%LOCALAPPDATA%\MemStor\memstor.db`.

## Command line

The `memstor` CLI reads and writes the same database:

```powershell
memstor watch                 # record without the tray app
memstor today
memstor day 2026-10-27
memstor search thesis chapter
```

## Layout

```
crates/core      collector + database (shared)
  src/filter.rs    what counts as noise
  src/db.rs        SQLite schema, recording, day listing, full-text search
  src/recent.rs    Windows Recent Items shortcuts
  src/watch.rs     folder watcher, pause/stop control
crates/cli       the `memstor` command
app/src-tauri    the tray app (Tauri 2): tray menu, hotkey, commands for the window
app/ui           the search window (plain HTML/CSS/JS, no build step)
```

## Build

```bash
cargo test -p memstor-core -p memstor-cli
cargo build --release -p memstor-cli

# tray app + installer (on Windows)
cd app
npx @tauri-apps/cli@2 build        # or `npx @tauri-apps/cli@2 dev` while working on it
```

On Linux the app needs `libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev libxdo-dev`.

## Next

- M2: projects. Name a project, let MemStor suggest one from folders you use together, and
  gather "everything I used for Project A today" into a folder of shortcuts.
- Active-window titles as an opt-in signal.
- Start with Windows (autostart) and a settings page for watched folders.
