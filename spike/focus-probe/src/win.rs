//! Window listing and focusing, adapted from Signalpost's switcher.rs.

use windows::core::{BOOL, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM};
use windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, EnumWindows, GetForegroundWindow, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, SetForegroundWindow, ShowWindow,
    SW_RESTORE,
};

#[derive(Clone)]
pub struct Entry {
    pub handle: isize,
    pub title: String,
    pub app: String,
    pub kind: &'static str,
}

pub struct FocusReport {
    pub returned: bool,
    pub note: String,
}

const EDITORS: [&str; 4] = ["code", "code - insiders", "cursor", "windsurf"];
const TERMINALS: [&str; 7] = [
    "windowsterminal",
    "wt",
    "pwsh",
    "powershell",
    "cmd",
    "alacritty",
    "wezterm-gui",
];

fn classify(app: &str) -> &'static str {
    let key = app.to_lowercase();
    if EDITORS.contains(&key.as_str()) {
        "editor"
    } else if TERMINALS.contains(&key.as_str()) {
        "terminal"
    } else {
        "other"
    }
}

fn window_title(hwnd: HWND) -> String {
    unsafe {
        let length = GetWindowTextLengthW(hwnd);
        if length <= 0 {
            return String::new();
        }
        let mut buffer = vec![0u16; length as usize + 1];
        let written = GetWindowTextW(hwnd, &mut buffer);
        String::from_utf16_lossy(&buffer[..written as usize])
    }
}

fn process_name(hwnd: HWND) -> Option<String> {
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return None;
        }
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buffer = vec![0u16; 512];
        let mut length = buffer.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        );
        let _ = CloseHandle(handle);
        result.ok()?;
        let path = String::from_utf16_lossy(&buffer[..length as usize]);
        let file = path.rsplit(['\\', '/']).next()?;
        Some(
            file.trim_end_matches(".exe")
                .trim_end_matches(".EXE")
                .to_string(),
        )
    }
}

unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let found = unsafe { &mut *(lparam.0 as *mut Vec<Entry>) };
    if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return BOOL(1);
    }
    let title = window_title(hwnd);
    if title.trim().is_empty() {
        return BOOL(1);
    }
    let Some(app) = process_name(hwnd) else {
        return BOOL(1);
    };
    let kind = classify(&app);
    found.push(Entry {
        handle: hwnd.0 as isize,
        title,
        app,
        kind,
    });
    BOOL(1)
}

pub fn list() -> Vec<Entry> {
    let mut found: Vec<Entry> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut found as *mut _ as isize));
    }
    // Editors and terminals first: they are what the dashboard will focus.
    found.sort_by_key(|e| (e.kind == "other", e.app.to_lowercase()));
    found
}

pub fn foreground() -> isize {
    unsafe { GetForegroundWindow() }.0 as isize
}

pub fn focus(handle: isize, attach: bool) -> FocusReport {
    let hwnd = HWND(handle as *mut core::ffi::c_void);
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        if !attach {
            return FocusReport {
                returned: SetForegroundWindow(hwnd).as_bool(),
                note: String::new(),
            };
        }
        // Joining the input queue of the current foreground thread lets this
        // thread pass the foreground lock that would otherwise refuse it.
        let foreground_thread = GetWindowThreadProcessId(GetForegroundWindow(), None);
        let own_thread = GetCurrentThreadId();
        let attached = foreground_thread != 0
            && foreground_thread != own_thread
            && AttachThreadInput(own_thread, foreground_thread, true).as_bool();
        let _ = BringWindowToTop(hwnd);
        let returned = SetForegroundWindow(hwnd).as_bool();
        if attached {
            let _ = AttachThreadInput(own_thread, foreground_thread, false);
        }
        FocusReport {
            returned,
            note: format!("(attached={attached})"),
        }
    }
}
