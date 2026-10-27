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

## Projects and gather (M2)

Open the **Projects** tab:

- **+ New** names a project and points it at the folder where its files live (you can add more folders).
  Or press **Make project** on one of the *Folders you worked in this week* suggestions.
- Pick **Today**, **Yesterday** or **Last 7 days** to see every file used for the project. Files in its
  folders are listed, and so are files elsewhere (Downloads, Desktop…) used within 30 minutes of them,
  marked **same time**. Untick anything that doesn't belong.
- **Gather** makes a new folder of shortcuts in `Documents\MemStor Gathered\<project> <date>` and opens it.
  Your files are never moved or copied.

![Projects tab](docs/projects-tab.png)

From the command line: `memstor projects`, `memstor project-add Thesis --folder D:\Uni\Thesis`,
`memstor project-files Thesis`, `memstor gather Thesis --date 2026-10-27`.

## What it records

| Source | Gives us | Needs admin? |
| --- | --- | --- |
| Folder watcher | files created, edited, renamed, deleted | no |
| Catch-up scan at start | files edited while MemStor was closed (first run: last 14 days) | no |
| Recent Items (`%APPDATA%\Microsoft\Windows\Recent`) | files you opened anywhere on disk | no |

**Where it looks:** every folder in your user folder (Desktop, Documents, Downloads, Pictures,
OneDrive…) except AppData and hidden tool folders like `.vscode`, plus other fixed drives (D:, E:…).
Windows, Program Files and ProgramData are skipped.

**Which files:** only everyday file types for now. The list is `TRACKED_TYPES` in
`crates/core/src/filter.rs`:

| Kind | Extensions |
| --- | --- |
| Documents | doc docx odt rtf txt md pdf epub pages tex |
| Spreadsheets | xls xlsx xlsm ods csv tsv numbers |
| Presentations | ppt pptx odp key |
| Images | jpg jpeg png gif bmp webp heic heif tif tiff svg psd ai |
| Audio | mp3 wav m4a aac flac ogg wma |
| Video | mp4 mov avi mkv webm wmv m4v |
| Archives | zip rar 7z |

It also skips Office lock files (`~$x.docx`), temp and partial downloads, `node_modules` and build
output. Saving through a temp file, as Word and Excel do, counts as one edit of the real file, and a
rename keeps the file's history. Only paths, names and times are stored. File contents are never read.

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

- Active-window titles as an opt-in signal.
- Start with Windows (autostart) and a settings page for watched folders.
