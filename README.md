# MemStor

Find files on your computer by what you were doing, not only by what you named them.

This is **M0**, a command-line test of the background collector. It quietly records which files
you create, edit, rename and open, keeps that history in a local SQLite database, and lets you
list what you touched on a given day or search by name and folder. Nothing leaves your computer.

The plan: https://claude.ai/artifact/PQbWgqCHXuf8BgrXnNueuJ

## Try it on Windows

```powershell
# start recording (watches Desktop, Documents, Downloads + Windows Recent Items)
memstor watch

# in another window
memstor today
memstor day 2026-10-27
memstor search thesis chapter
```

Watch other folders with `--folder D:\Uni --folder D:\Work`. The database lives in
`%LOCALAPPDATA%\MemStor\memstor.db` (change it with `--db`).

## What it records

| Source | Gives us | Needs admin? |
| --- | --- | --- |
| Folder watcher | files created, saved, renamed, deleted in watched folders | no |
| Recent Items (`%APPDATA%\Microsoft\Windows\Recent`) | files you opened anywhere on disk | no |

It skips noise: Office lock files (`~$x.docx`), temp and partial downloads, hidden files,
`node_modules`, `.git`, build output and AppData. A save done through a temp file
(`A1B2.tmp` renamed over `report.docx`) counts as an edit of `report.docx`, and a rename
keeps the file's history.

Only paths, names and times are stored. File contents are never read.

## Build

```bash
cargo test
cargo build --release
# Windows .exe from Linux:
rustup target add x86_64-pc-windows-gnu   # needs gcc-mingw-w64-x86-64
cargo build --release --target x86_64-pc-windows-gnu
```

## Layout

- `src/filter.rs` decides what counts as noise
- `src/db.rs` SQLite schema, recording, day listing, full-text search (FTS5)
- `src/recent.rs` reads Windows Recent Items shortcuts
- `src/watch.rs` turns folder-watcher events into records
- `src/main.rs` the command line

## Next (M1)

Wrap this in a Tauri 2 app: tray icon with pause, a hotkey search window, and a
today/yesterday timeline. Active-window titles come in as an opt-in signal.
