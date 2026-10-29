//! What's happening on the PC right now, read on the PC: the app in front and its window
//! title, the folder open in Explorer and the files selected there, the piece of work going
//! on, and today's downloads. The assistant gets this as a short text card, and the user
//! sees each part as a chip they can leave out. App use is also logged locally, so MemStor
//! knows how long something has been in front and which app was used before MemStor opened.

use crate::db::Db;
use crate::projects::is_inside;
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub(crate) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS app_usage (
    id       INTEGER PRIMARY KEY,
    start_ts INTEGER NOT NULL,
    end_ts   INTEGER NOT NULL,
    app      TEXT NOT NULL,
    title    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS app_usage_end ON app_usage(end_ts);
"#;

/// Two samples of the same window this close together belong to one stretch of use.
const SAME_STRETCH_SECS: i64 = 20;

/// The window in front.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Foreground {
    /// A friendly app name ("Premiere Pro").
    pub app: String,
    /// The program file name, lower case ("adobe premiere pro.exe").
    pub exe: String,
    /// The window title; empty for private windows and password managers.
    pub title: String,
    /// For Explorer: the folder open in the active tab.
    pub folder: Option<String>,
    /// For Explorer: the files selected there (at most 20).
    pub selected: Vec<String>,
}

impl Foreground {
    /// MemStor's own window and the Windows shell aren't "what you're doing".
    pub fn is_interesting(&self) -> bool {
        !matches!(self.exe.as_str(), "" | "memstor-app.exe" | "memstor.exe" | "shellexperiencehost.exe" | "searchhost.exe" | "startmenuexperiencehost.exe" | "lockapp.exe")
            && !(self.exe == "explorer.exe" && self.folder.is_none() && self.title.is_empty())
    }
}

/// Programs people know by another name.
fn friendly_app(exe: &str) -> String {
    const NAMES: &[(&str, &str)] = &[
        ("winword.exe", "Word"),
        ("excel.exe", "Excel"),
        ("powerpnt.exe", "PowerPoint"),
        ("outlook.exe", "Outlook"),
        ("onenote.exe", "OneNote"),
        ("acrord32.exe", "Acrobat Reader"),
        ("acrobat.exe", "Acrobat"),
        ("explorer.exe", "File Explorer"),
        ("chrome.exe", "Chrome"),
        ("msedge.exe", "Edge"),
        ("firefox.exe", "Firefox"),
        ("brave.exe", "Brave"),
        ("code.exe", "VS Code"),
        ("notepad.exe", "Notepad"),
        ("photoshop.exe", "Photoshop"),
        ("illustrator.exe", "Illustrator"),
        ("adobe premiere pro.exe", "Premiere Pro"),
        ("afterfx.exe", "After Effects"),
        ("resolve.exe", "DaVinci Resolve"),
        ("capcut.exe", "CapCut"),
        ("canva.exe", "Canva"),
        ("figma.exe", "Figma"),
        ("whatsapp.exe", "WhatsApp"),
        ("telegram.exe", "Telegram"),
        ("teams.exe", "Teams"),
        ("ms-teams.exe", "Teams"),
        ("zoom.exe", "Zoom"),
        ("vlc.exe", "VLC"),
        ("photos.exe", "Photos"),
        ("microsoft.photos.exe", "Photos"),
        ("wps.exe", "WPS Writer"),
        ("et.exe", "WPS Spreadsheets"),
        ("wpp.exe", "WPS Presentation"),
    ];
    if let Some((_, n)) = NAMES.iter().find(|(e, _)| *e == exe) {
        return n.to_string();
    }
    let stem = exe.trim_end_matches(".exe");
    let mut c = stem.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

/// Window titles that shouldn't be kept or sent.
fn private_title(exe: &str, title: &str) -> bool {
    let t = title.to_lowercase();
    t.contains("inprivate") || t.contains("incognito") || t.contains("private browsing")
        || ["keepass.exe", "keepassxc.exe", "1password.exe", "bitwarden.exe", "lastpass.exe"].contains(&exe)
}

/// Build a Foreground from what Windows reports.
pub fn foreground_from(exe_path: &str, title: &str, folder: Option<String>, selected: Vec<String>) -> Foreground {
    // Split on both separators: Windows paths are also read in tests on other systems.
    let exe = exe_path.rsplit(['\\', '/']).next().unwrap_or("").to_lowercase();
    let title = if private_title(&exe, title) { String::new() } else { title.chars().take(200).collect() };
    Foreground { app: friendly_app(&exe), exe, title, folder, selected }
}

/// One part of the context, shown as a chip the user can switch off.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Chip {
    pub key: String,
    pub label: String,
}

/// What the assistant is told about the moment.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ContextCard {
    pub app: Option<String>,
    pub title: Option<String>,
    /// How long that window has been in use, in minutes.
    pub minutes: Option<i64>,
    pub folder: Option<String>,
    pub selected: Vec<String>,
    /// The piece of work going on in the last few hours.
    pub activity: Option<String>,
    pub activity_files: Vec<String>,
    /// Files that arrived in Downloads today.
    pub downloads: Vec<String>,
}

fn name(p: &str) -> String {
    Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.to_string())
}

impl ContextCard {
    pub fn chips(&self) -> Vec<Chip> {
        let mut out = Vec::new();
        if let Some(app) = &self.app {
            let label = match &self.title {
                Some(t) if !t.is_empty() => format!("{app} · {}", t.chars().take(60).collect::<String>()),
                _ => app.clone(),
            };
            out.push(Chip { key: "window".into(), label });
        }
        if let Some(f) = &self.folder {
            out.push(Chip { key: "folder".into(), label: f.clone() });
        }
        if !self.selected.is_empty() {
            let label = if self.selected.len() == 1 { format!("Selected: {}", name(&self.selected[0])) } else { format!("{} files selected", self.selected.len()) };
            out.push(Chip { key: "selected".into(), label });
        }
        if let Some(a) = &self.activity {
            out.push(Chip { key: "activity".into(), label: format!("Working on: {a}") });
        }
        if !self.downloads.is_empty() {
            out.push(Chip { key: "downloads".into(), label: format!("{} new download{}", self.downloads.len(), if self.downloads.len() == 1 { "" } else { "s" }) });
        }
        out
    }

    /// Leave out the parts the user switched off.
    pub fn without(&self, off: &[String]) -> ContextCard {
        let mut c = self.clone();
        let is_off = |k: &str| off.iter().any(|o| o == k);
        if is_off("window") {
            c.app = None;
            c.title = None;
            c.minutes = None;
        }
        if is_off("folder") {
            c.folder = None;
        }
        if is_off("selected") {
            c.selected.clear();
        }
        if is_off("activity") {
            c.activity = None;
            c.activity_files.clear();
        }
        if is_off("downloads") {
            c.downloads.clear();
        }
        c
    }

    /// The card as text for the assistant. Empty when there's nothing to say.
    pub fn to_prompt(&self) -> String {
        let mut lines = Vec::new();
        if let Some(app) = &self.app {
            let mut l = format!("App in front: {app}");
            if let Some(t) = self.title.as_ref().filter(|t| !t.is_empty()) {
                l.push_str(&format!(", window title \"{t}\""));
            }
            if let Some(m) = self.minutes.filter(|m| *m > 0) {
                l.push_str(&format!(", in use for about {m} min"));
            }
            lines.push(l);
        }
        if let Some(f) = &self.folder {
            lines.push(format!("Folder open in Explorer: {f}"));
        }
        if !self.selected.is_empty() {
            lines.push(format!("Selected in Explorer (\"this\", \"these\"): {}", self.selected.join(" | ")));
        }
        if let Some(a) = &self.activity {
            lines.push(format!("Current piece of work: {a}; its files: {}", self.activity_files.join(" | ")));
        }
        if !self.downloads.is_empty() {
            lines.push(format!("Downloaded today: {}", self.downloads.join(" | ")));
        }
        lines.join("\n")
    }
}

impl Db {
    /// Whether MemStor may read which app and window are in front. On unless turned off.
    pub fn pc_awareness(&self) -> Result<bool> {
        Ok(self.kv("pc_awareness")?.as_deref() != Some("0"))
    }

    pub fn set_pc_awareness(&self, on: bool) -> Result<()> {
        self.set_kv("pc_awareness", Some(if on { "1" } else { "0" }))?;
        self.set_kv("pc_awareness_asked", Some("1"))?;
        if !on {
            self.conn.execute("DELETE FROM app_usage", [])?;
        }
        Ok(())
    }

    /// True until the user has said yes or no to PC awareness once.
    pub fn pc_awareness_unasked(&self) -> Result<bool> {
        Ok(self.kv("pc_awareness_asked")?.is_none())
    }

    /// Note that `fg` was in front at `ts`. Consecutive samples of the same window extend one row.
    pub fn log_app(&self, fg: &Foreground, ts: i64) -> Result<()> {
        if !fg.is_interesting() {
            return Ok(());
        }
        let last: Option<(i64, String, String, i64)> = self
            .conn
            .query_row("SELECT id, app, title, end_ts FROM app_usage ORDER BY id DESC LIMIT 1", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .optional()?;
        match last {
            Some((id, app, title, end)) if app == fg.app && title == fg.title && ts - end <= SAME_STRETCH_SECS => {
                self.conn.execute("UPDATE app_usage SET end_ts = ?2 WHERE id = ?1", params![id, ts])?;
            }
            _ => {
                self.conn.execute("INSERT INTO app_usage (start_ts, end_ts, app, title) VALUES (?1, ?1, ?2, ?3)", params![ts, fg.app, fg.title])?;
            }
        }
        Ok(())
    }

    /// The last app used before MemStor was opened (for when it's opened from the tray).
    pub fn last_app(&self, since: i64) -> Result<Option<Foreground>> {
        Ok(self
            .conn
            .query_row("SELECT app, title FROM app_usage WHERE end_ts >= ?1 ORDER BY end_ts DESC LIMIT 1", [since], |r| {
                Ok(Foreground { app: r.get(0)?, title: r.get(1)?, ..Default::default() })
            })
            .optional()?)
    }

    /// Minutes the same app and title have been in front, counting short breaks.
    fn minutes_in(&self, app: &str, title: &str, now: i64) -> Result<Option<i64>> {
        let rows: Vec<(i64, i64, String, String)> = {
            let mut s = self.conn.prepare("SELECT start_ts, end_ts, app, title FROM app_usage WHERE end_ts >= ?1 ORDER BY end_ts DESC")?;
            s.query_map([now - 6 * 3600], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<_, _>>()?
        };
        let mut start = None;
        let mut edge = now;
        for (s, e, a, t) in rows {
            if edge - e > 15 * 60 {
                break; // a long gap ends it
            }
            if a == app && t == title {
                start = Some(s);
            }
            edge = s;
        }
        Ok(start.map(|s| (now - s) / 60))
    }

    fn private_path(&self, p: &str) -> bool {
        let lists = [self.excluded_folders().unwrap_or_default(), self.local_only_folders().unwrap_or_default()];
        lists.iter().flatten().any(|f| p.eq_ignore_ascii_case(f) || is_inside(p, f))
    }

    /// The context card for now. `fg` is the window that was in front when MemStor was called.
    pub fn context_card(&self, fg: Option<Foreground>, now: i64) -> Result<ContextCard> {
        let mut c = ContextCard::default();
        if self.pc_awareness()? {
            let fg = fg.filter(Foreground::is_interesting).or(self.last_app(now - 10 * 60)?);
            if let Some(fg) = fg {
                c.minutes = self.minutes_in(&fg.app, &fg.title, now)?;
                c.app = Some(fg.app);
                c.title = Some(fg.title).filter(|t| !t.is_empty());
                c.folder = fg.folder.filter(|f| !self.private_path(f));
                c.selected = fg.selected.into_iter().filter(|p| !self.private_path(p)).take(20).collect();
            }
        }
        let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(std::path::PathBuf::from);
        let acts = self.activities(now - 3 * 3600, now + 1, home.as_deref())?;
        if let Some(a) = acts.into_iter().filter(|a| a.files.len() >= 2).max_by_key(|a| a.end) {
            c.activity = Some(a.label.clone());
            c.activity_files = a.files.iter().map(|f| f.path.clone()).filter(|p| !self.private_path(p)).take(15).collect();
        }
        if let Some(dl) = dirs::download_dir() {
            let dl = dl.display().to_string();
            let start = now - 18 * 3600;
            c.downloads = self
                .touched_between(start, now + 1)?
                .into_iter()
                .filter(|t| is_inside(&t.path, &dl) && t.kinds.contains("created"))
                .map(|t| t.path)
                .filter(|p| !self.private_path(p))
                .take(8)
                .collect();
        }
        Ok(c)
    }
}

// ---------- reading the window in front (Windows) ----------

/// The window handle in front right now (0 if none).
#[cfg(windows)]
pub fn foreground_window() -> isize {
    // SAFETY: no arguments; returns a handle or null.
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow() as isize }
}

#[cfg(not(windows))]
pub fn foreground_window() -> isize {
    0
}

/// App, title and (for Explorer) folder and selection of a window. Explorer is asked on a
/// thread of its own, and given up on after a second so MemStor never hangs on it.
#[cfg(windows)]
pub fn read_window(hwnd: isize) -> Option<Foreground> {
    let (mut fg, class) = read_basic(hwnd)?;
    if class == "CabinetWClass" {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(explorer::view(hwnd));
        });
        if let Some((folder, selected)) = rx.recv_timeout(std::time::Duration::from_millis(1200)).ok().flatten() {
            fg.folder = folder;
            fg.selected = selected;
        }
    }
    Some(fg)
}

/// App and title only: cheap enough to sample every few seconds.
#[cfg(windows)]
pub fn read_window_basic(hwnd: isize) -> Option<Foreground> {
    read_basic(hwnd).map(|(fg, _)| fg)
}

#[cfg(not(windows))]
pub fn read_window_basic(_hwnd: isize) -> Option<Foreground> {
    None
}

#[cfg(windows)]
fn read_basic(hwnd: isize) -> Option<(Foreground, String)> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetClassNameW, GetWindowTextW, GetWindowThreadProcessId};
    if hwnd == 0 {
        return None;
    }
    let h = hwnd as windows_sys::Win32::Foundation::HWND;
    let mut buf = [0u16; 512];
    // SAFETY: the buffers are valid for the lengths passed; handles are checked and closed.
    let (title, class, exe) = unsafe {
        let n = GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32);
        let title = String::from_utf16_lossy(&buf[..n.max(0) as usize]);
        let n = GetClassNameW(h, buf.as_mut_ptr(), buf.len() as i32);
        let class = String::from_utf16_lossy(&buf[..n.max(0) as usize]);
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, &mut pid);
        let mut exe = String::new();
        let proc = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if !proc.is_null() {
            let mut len = buf.len() as u32;
            if QueryFullProcessImageNameW(proc, 0, buf.as_mut_ptr(), &mut len) != 0 {
                exe = String::from_utf16_lossy(&buf[..len as usize]);
            }
            CloseHandle(proc);
        }
        (title, class, exe)
    };
    Some((foreground_from(&exe, &title, None, Vec::new()), class))
}

#[cfg(not(windows))]
pub fn read_window(_hwnd: isize) -> Option<Foreground> {
    None
}

/// True when more than one keyboard layout is installed. Ctrl+Space often switches input
/// language or IME then, so MemStor doesn't take it.
#[cfg(windows)]
pub fn several_keyboard_layouts() -> bool {
    // SAFETY: with a zero count and null buffer it only returns how many layouts there are.
    unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::GetKeyboardLayoutList(0, std::ptr::null_mut()) > 1 }
}

#[cfg(not(windows))]
pub fn several_keyboard_layouts() -> bool {
    false
}

/// The window in front right now, read fully.
pub fn capture() -> Option<Foreground> {
    read_window(foreground_window())
}

#[cfg(windows)]
mod explorer {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, IServiceProvider};
    use windows::Win32::System::Variant::VARIANT;
    use windows::Win32::UI::Shell::{
        IFolderView, IPersistFolder2, IShellBrowser, IShellItemArray, IShellWindows, IWebBrowser2, GPFIDL_DEFAULT, SHGetPathFromIDListEx, SIGDN_FILESYSPATH, SVGIO_SELECTION, ShellWindows,
    };
    use windows::Win32::UI::WindowsAndMessaging::FindWindowExW;
    use windows::core::{GUID, Interface, w};

    /// SID_STopLevelBrowser: asks an Explorer window for its shell browser.
    const SID_TOP_LEVEL_BROWSER: GUID = GUID::from_u128(0x4c96be40_915c_11cf_99d3_00aa004ae837);

    /// The folder open in the active tab of the Explorer window `hwnd`, and its selection.
    pub fn view(hwnd: isize) -> Option<(Option<String>, Vec<String>)> {
        // SAFETY: COM is set up for this thread and torn down at the end; every pointer
        // comes from COM and is released by the wrappers or CoTaskMemFree.
        unsafe {
            let inited = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
            let out = read(hwnd);
            if inited {
                CoUninitialize();
            }
            out
        }
    }

    unsafe fn read(hwnd: isize) -> Option<(Option<String>, Vec<String>)> {
        unsafe {
            let shell: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_ALL).ok()?;
            // With tabs, every tab reports the same window; the active one is the first tab child.
            let tab = FindWindowExW(Some(HWND(hwnd as _)), None, w!("ShellTabWindowClass"), None).unwrap_or_default();
            let count = shell.Count().ok()?;
            for i in 0..count {
                let Ok(disp) = shell.Item(&VARIANT::from(i)) else { continue };
                let Ok(browser) = disp.cast::<IWebBrowser2>() else { continue };
                let Ok(h) = browser.HWND() else { continue };
                if h.0 as isize != hwnd {
                    continue;
                }
                let Ok(sp) = browser.cast::<IServiceProvider>() else { continue };
                let Ok(sb) = sp.QueryService::<IShellBrowser>(&SID_TOP_LEVEL_BROWSER) else { continue };
                if !tab.is_invalid() {
                    if let Ok(w) = sb.GetWindow() {
                        if w != tab {
                            continue;
                        }
                    }
                }
                let Ok(view) = sb.QueryActiveShellView() else { continue };
                let Ok(fv) = view.cast::<IFolderView>() else { continue };
                let mut folder = None;
                if let Ok(pf) = fv.GetFolder::<IPersistFolder2>() {
                    if let Ok(pidl) = pf.GetCurFolder() {
                        let mut buf = [0u16; 1024];
                        if SHGetPathFromIDListEx(pidl, &mut buf, GPFIDL_DEFAULT).as_bool() {
                            let n = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
                            folder = Some(String::from_utf16_lossy(&buf[..n]));
                        }
                        CoTaskMemFree(Some(pidl as _));
                    }
                }
                let mut selected = Vec::new();
                if let Ok(items) = fv.Items::<IShellItemArray>(SVGIO_SELECTION) {
                    let n = items.GetCount().unwrap_or(0);
                    for j in 0..n.min(20) {
                        if let Ok(item) = items.GetItemAt(j) {
                            if let Ok(name) = item.GetDisplayName(SIGDN_FILESYSPATH) {
                                if let Ok(s) = name.to_string() {
                                    selected.push(s);
                                }
                                CoTaskMemFree(Some(name.0 as _));
                            }
                        }
                    }
                }
                return Some((folder.filter(|f| !f.is_empty()), selected));
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Kind;

    #[test]
    fn friendly_names_and_private_windows() {
        let f = foreground_from("C:\\Program Files\\Adobe\\Adobe Premiere Pro.exe", "Alumni_Meet_v3.prproj", None, vec![]);
        assert_eq!((f.app.as_str(), f.title.as_str()), ("Premiere Pro", "Alumni_Meet_v3.prproj"));
        let f = foreground_from("C:\\x\\msedge.exe", "Bank - InPrivate - Microsoft Edge", None, vec![]);
        assert_eq!((f.app.as_str(), f.title.as_str()), ("Edge", ""));
        assert_eq!(foreground_from("C:\\x\\krita.exe", "t", None, vec![]).app, "Krita");
        assert!(!foreground_from("C:\\x\\memstor-app.exe", "MemStor", None, vec![]).is_interesting());
    }

    #[test]
    fn card_from_app_use_and_activity() {
        let db = Db::in_memory().unwrap();
        let word = foreground_from("C:\\x\\WINWORD.EXE", "Thesis chapter 3.docx - Word", None, vec![]);
        let now = 1_800_000_000;
        // Word in front for 30 minutes, sampled every 10 s, with a 2-minute look at Chrome.
        for t in (now - 1800..now - 600).step_by(10) {
            db.log_app(&word, t).unwrap();
        }
        let chrome = foreground_from("C:\\x\\chrome.exe", "Citation styles", None, vec![]);
        for t in (now - 600..now - 480).step_by(10) {
            db.log_app(&chrome, t).unwrap();
        }
        for t in (now - 480..now).step_by(10) {
            db.log_app(&word, t).unwrap();
        }
        let rows: i64 = db.conn.query_row("SELECT count(*) FROM app_usage", [], |r| r.get(0)).unwrap();
        assert_eq!(rows, 3, "samples merge into stretches");
        for (i, n) in ["thesis_ch3.docx", "thesis_refs.bib", "thesis_ch3_figures.xlsx"].iter().enumerate() {
            db.record(&format!("/u/Thesis/{n}"), Kind::Modified, "watcher", now - 1200 + i as i64 * 60).unwrap();
        }

        let c = db.context_card(Some(word.clone()), now).unwrap();
        assert_eq!(c.app.as_deref(), Some("Word"));
        assert_eq!(c.minutes, Some(30));
        assert!(c.activity_files.len() >= 2, "{c:?}");
        let text = c.to_prompt();
        assert!(text.contains("Thesis chapter 3.docx - Word") && text.contains("about 30 min"), "{text}");
        let keys: Vec<String> = c.chips().into_iter().map(|c| c.key).collect();
        assert!(keys.contains(&"window".to_string()) && keys.contains(&"activity".to_string()));
        assert!(!c.without(&["window".into()]).to_prompt().contains("Word"));

        // Opened from the tray: the last app used stands in.
        assert_eq!(db.context_card(None, now + 30).unwrap().app.as_deref(), Some("Word"));
        // Turned off: no app, and the log is cleared.
        db.set_pc_awareness(false).unwrap();
        assert_eq!(db.context_card(Some(word), now).unwrap().app, None);
        assert!(db.last_app(0).unwrap().is_none());
    }

    #[test]
    fn private_folders_stay_out() {
        let db = Db::in_memory().unwrap();
        db.add_local_only_folder("/u/Private").unwrap();
        let fg = foreground_from("C:\\Windows\\explorer.exe", "Private", Some("/u/Private".into()), vec!["/u/Private/id.pdf".into(), "/u/Docs/a.pdf".into()]);
        let c = db.context_card(Some(fg), 1_800_000_000).unwrap();
        assert_eq!(c.folder, None);
        assert_eq!(c.selected, vec!["/u/Docs/a.pdf".to_string()]);
    }
}
