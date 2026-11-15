//! Bringing a session's terminal tab forward, not just its window
//! (DESIGN.md §5).
//!
//! - **WezTerm** has a supported way: every program in a pane gets
//!   `WEZTERM_PANE`, which the hook records, and `wezterm cli` shows the tab
//!   holding that pane, then the pane.
//! - **Windows Terminal** has none; Microsoft declined to add one
//!   (microsoft/terminal#19783). The workaround, as in perch: read the tab's
//!   title from the agent's console, then select the tab of that name through
//!   UI Automation.
//!
//! Every failure here is quiet: the window is still brought forward, only the
//! tab is left as it was.

use crate::host::Host;

/// Ignores the status glyph agents put in front of their titles
/// (`✳ Fix login`, `◐ Fix login`), so a title read a moment earlier still
/// matches. A title with no letters or digits matches nothing.
pub fn normalize_title(title: &str) -> String {
    title
        .trim_start_matches(|c: char| !c.is_alphanumeric())
        .trim()
        .to_string()
}

/// The single tab whose name matches `wanted`, or why there is none.
pub fn pick_tab(names: &[String], wanted: &str) -> Result<usize, &'static str> {
    let wanted = normalize_title(wanted);
    if wanted.is_empty() {
        return Err("its tab has no title to look for");
    }
    let matches: Vec<usize> = names
        .iter()
        .enumerate()
        .filter(|(_, name)| normalize_title(name) == wanted)
        .map(|(i, _)| i)
        .collect();
    match matches.as_slice() {
        [one] => Ok(*one),
        [] => Err("no tab carries its title"),
        _ => Err("several tabs carry the same title"),
    }
}

/// Whether `path` is WezTerm's GUI executable, so the CLI beside it is
/// WezTerm's own.
pub fn is_wezterm_gui(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("wezterm-gui.exe"))
}

/// The tab id holding `pane`, from `wezterm cli list --format json`.
pub fn wezterm_tab_of(listing: &str, pane: &str) -> Option<String> {
    let panes: Vec<serde_json::Value> = serde_json::from_str(listing).ok()?;
    panes
        .iter()
        .find(|p| p.get("pane_id").map(|id| id.to_string()) == Some(pane.to_string()))
        .and_then(|p| p.get("tab_id"))
        .map(|tab| tab.to_string())
}

/// Selects the session's tab when its terminal allows it. Returns a short note
/// for the status line, or None when the window has no tabs to choose from.
pub fn select(host: &Host, pane: Option<&str>, hwnd: isize) -> Option<String> {
    match host.exe.to_lowercase().as_str() {
        "wezterm-gui" => Some(match pane {
            Some(pane) => match platform::activate_wezterm_pane(host.pid, pane) {
                Ok(()) => "pane selected".to_string(),
                Err(e) => format!("pane left as is: {e}"),
            },
            None => "pane left as is: its pane was not recorded".to_string(),
        }),
        "windowsterminal" => Some(match host.agent_pid {
            Some(agent) => match console_title(agent) {
                Some(title) => match platform::select_windows_terminal_tab(hwnd, &title) {
                    Ok(()) => "tab selected".to_string(),
                    Err(e) => format!("tab left as is: {e}"),
                },
                None => "tab left as is: could not read its title".to_string(),
            },
            None => "tab left as is: its process was not found".to_string(),
        }),
        _ => None,
    }
}

/// The title of the console `pid` runs in, read by a helper process
/// (`bellpanel console-title <pid>`). Reading it means attaching to that
/// console, which a process can only do after leaving its own, and the
/// dashboard must keep its own.
pub fn console_title(pid: u32) -> Option<String> {
    platform::console_title_via_helper(pid)
}

pub use platform::attach_and_read_title;

#[cfg(windows)]
mod platform {
    use std::io::Read;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use windows::core::BOOL;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
    };
    use windows::Win32::System::Console::{
        AttachConsole, FreeConsole, GetConsoleTitleW, GetStdHandle, SetConsoleCtrlHandler,
        SetStdHandle, STD_OUTPUT_HANDLE,
    };
    use windows::Win32::System::Threading::{CREATE_NO_WINDOW, DETACHED_PROCESS};
    use windows::Win32::System::Variant::VARIANT;
    use windows::Win32::UI::Accessibility::{
        CUIAutomation, IUIAutomation, IUIAutomationSelectionItemPattern, TreeScope_Descendants,
        UIA_ControlTypePropertyId, UIA_SelectionItemPatternId, UIA_TabItemControlTypeId,
    };

    /// How long the helper and `wezterm cli` may take before they are given up on.
    const HELPER_TIMEOUT: Duration = Duration::from_millis(1500);

    fn run_with_timeout(mut command: Command) -> Result<String, String> {
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        let started = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let mut out = String::new();
                    if let Some(mut stdout) = child.stdout.take() {
                        let _ = stdout.read_to_string(&mut out);
                    }
                    if status.success() {
                        return Ok(out);
                    }
                    let mut err = String::new();
                    if let Some(mut stderr) = child.stderr.take() {
                        let _ = stderr.read_to_string(&mut err);
                    }
                    return Err(err.lines().next().unwrap_or("failed").to_string());
                }
                Ok(None) if started.elapsed() < HELPER_TIMEOUT => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                Ok(None) => {
                    let _ = child.kill();
                    return Err("timed out".into());
                }
                Err(e) => return Err(e.to_string()),
            }
        }
    }

    pub fn console_title_via_helper(pid: u32) -> Option<String> {
        let exe = std::env::current_exe().ok()?;
        let mut command = Command::new(exe);
        // No console of its own, so it is free to attach to the agent's.
        command
            .args(["console-title", &pid.to_string()])
            .creation_flags(DETACHED_PROCESS.0);
        run_with_timeout(command)
            .ok()
            .filter(|title| !title.is_empty())
    }

    /// A Ctrl+C or Ctrl+Break pressed in the agent's console while attached
    /// would otherwise end this process.
    unsafe extern "system" fn swallow_ctrl(_: u32) -> BOOL {
        BOOL(1)
    }

    /// Attaches to the console of `pid`, reads its title and leaves again.
    ///
    /// Only for the `console-title` helper process: it detaches the calling
    /// process from its own console first.
    pub fn attach_and_read_title(pid: u32) -> Option<String> {
        unsafe {
            let stdout = GetStdHandle(STD_OUTPUT_HANDLE).ok();
            let _ = SetConsoleCtrlHandler(Some(swallow_ctrl), true);
            let _ = FreeConsole();
            let attached = AttachConsole(pid).is_ok();
            let title = attached.then(|| {
                let mut buffer = vec![0u16; 1024];
                let length = GetConsoleTitleW(&mut buffer) as usize;
                String::from_utf16_lossy(&buffer[..length.min(buffer.len())])
            });
            let _ = FreeConsole();
            // Attaching may have pointed stdout at the console just left; the
            // dashboard reads the answer from the pipe it gave us.
            if let Some(handle) = stdout {
                let _ = SetStdHandle(STD_OUTPUT_HANDLE, handle);
            }
            title
        }
    }

    pub fn select_windows_terminal_tab(hwnd: isize, title: &str) -> Result<(), String> {
        unsafe {
            // Harmless when COM is already set up on this thread.
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let automation: IUIAutomation =
                CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
                    .map_err(|e| e.message())?;
            let window = automation
                .ElementFromHandle(HWND(hwnd as *mut core::ffi::c_void))
                .map_err(|e| e.message())?;
            let is_tab = automation
                .CreatePropertyCondition(
                    UIA_ControlTypePropertyId,
                    &VARIANT::from(UIA_TabItemControlTypeId.0),
                )
                .map_err(|e| e.message())?;
            let found = window
                .FindAll(TreeScope_Descendants, &is_tab)
                .map_err(|e| e.message())?;
            let count = found.Length().map_err(|e| e.message())?;
            let mut tabs = Vec::new();
            let mut names = Vec::new();
            for i in 0..count {
                let Ok(tab) = found.GetElement(i) else {
                    continue;
                };
                names.push(tab.CurrentName().map(|n| n.to_string()).unwrap_or_default());
                tabs.push(tab);
            }
            let index = super::pick_tab(&names, title).map_err(str::to_string)?;
            let pattern: IUIAutomationSelectionItemPattern = tabs[index]
                .GetCurrentPatternAs(UIA_SelectionItemPatternId)
                .map_err(|e| e.message())?;
            // Selecting the selected tab again moves the keyboard to the tab strip.
            if pattern
                .CurrentIsSelected()
                .map(|b| b.as_bool())
                .unwrap_or(false)
            {
                return Ok(());
            }
            pattern.Select().map_err(|e| e.message())
        }
    }

    /// `wezterm cli <args>` aimed at the GUI process `wezterm_gui`.
    fn wezterm_cli(wezterm_gui: u32, args: &[&str]) -> Result<String, String> {
        // The pid comes from a recorded event. If WezTerm has exited since,
        // the pid may belong to another program now, and the file beside
        // that program is not WezTerm's CLI: run nothing then.
        let gui = crate::host::exe_path(wezterm_gui)
            .filter(|path| super::is_wezterm_gui(path))
            .ok_or_else(|| "WezTerm is no longer running".to_string())?;
        // The CLI sits next to `wezterm-gui.exe`; fall back to PATH.
        let cli = Some(gui.with_file_name("wezterm.exe"))
            .filter(|cli| cli.exists())
            .map_or_else(|| "wezterm".into(), |cli| cli.into_os_string());
        let mut command = Command::new(cli);
        // --no-auto-start: when the GUI cannot be reached, fail rather than
        // start a mux server of its own in the background.
        command
            .args(["cli", "--no-auto-start"])
            .args(args)
            .creation_flags(CREATE_NO_WINDOW.0);
        // Left to find the GUI by itself, the CLI (20240203 on Windows)
        // connects to the bare name `gui-sock-<pid>`, relative to wherever it
        // runs, and fails. Name the socket in full, and run beside it too.
        // Measured 2026-11-15: either way reaches the GUI.
        if let Some(home) = std::env::var_os("USERPROFILE") {
            let dir = std::path::PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("wezterm");
            let socket = dir.join(format!("gui-sock-{wezterm_gui}"));
            if socket.exists() {
                command
                    .env("WEZTERM_UNIX_SOCKET", &socket)
                    .current_dir(&dir);
            }
        }
        // Its errors read "<time>  ERROR  wezterm > <message>"; keep the message.
        run_with_timeout(command)
            .map_err(|e| e.rsplit("> ").next().unwrap_or(&e).trim().to_string())
    }

    /// Shows the tab holding `pane`, then the pane itself (it may be one of
    /// several splits in that tab).
    pub fn activate_wezterm_pane(wezterm_gui: u32, pane: &str) -> Result<(), String> {
        let listing = wezterm_cli(wezterm_gui, &["list", "--format", "json"])?;
        if let Some(tab) = super::wezterm_tab_of(&listing, pane) {
            wezterm_cli(wezterm_gui, &["activate-tab", "--tab-id", &tab])?;
        }
        wezterm_cli(wezterm_gui, &["activate-pane", "--pane-id", pane]).map(|_| ())
    }
}

#[cfg(not(windows))]
mod platform {
    pub fn console_title_via_helper(_: u32) -> Option<String> {
        None
    }
    pub fn attach_and_read_title(_: u32) -> Option<String> {
        None
    }
    pub fn select_windows_terminal_tab(_: isize, _: &str) -> Result<(), String> {
        Err("not supported here".into())
    }
    pub fn activate_wezterm_pane(_: u32, _: &str) -> Result<(), String> {
        Err("not supported here".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn status_glyphs_do_not_stop_a_match() {
        assert_eq!(normalize_title("✳ Fix login"), "Fix login");
        assert_eq!(normalize_title("◐ Fix login"), "Fix login");
        assert_eq!(normalize_title("⠂ "), "");
    }

    #[test]
    fn the_one_tab_with_the_title_is_picked() {
        let tabs = names(&[
            "PowerShell",
            "aaa - GitHub Copilot",
            "✳ Clarify aaa | myproject",
        ]);
        assert_eq!(pick_tab(&tabs, "aaa - GitHub Copilot"), Ok(1));
        assert_eq!(pick_tab(&tabs, "◐ Clarify aaa | myproject"), Ok(2));
    }

    #[test]
    fn a_wezterm_pane_is_found_in_its_tab() {
        // Trimmed from `wezterm cli list --format json` (20240203).
        let listing = r#"[{"window_id":0,"tab_id":0,"pane_id":0,"title":"zzz - GitHub Copilot"},
                          {"window_id":0,"tab_id":3,"pane_id":1,"title":"Undefined Task - GitHub Copilot"}]"#;
        assert_eq!(wezterm_tab_of(listing, "1").as_deref(), Some("3"));
        assert_eq!(wezterm_tab_of(listing, "7"), None);
        assert_eq!(wezterm_tab_of("not json", "1"), None);
    }

    #[test]
    fn only_wezterm_gui_vouches_for_the_cli_beside_it() {
        use std::path::Path;
        assert!(is_wezterm_gui(Path::new(
            r"C:\Program Files\WezTerm\wezterm-gui.exe"
        )));
        assert!(is_wezterm_gui(Path::new(
            r"D:\tools\wezterm\WEZTERM-GUI.EXE"
        )));
        // A reused pid pointing at some other program.
        assert!(!is_wezterm_gui(Path::new(
            r"C:\Users\me\Downloads\setup.exe"
        )));
        assert!(!is_wezterm_gui(Path::new(
            r"C:\Users\me\Downloads\wezterm.exe"
        )));
    }

    #[test]
    fn no_match_or_two_matches_pick_nothing() {
        let tabs = names(&["Claude Code", "Claude Code", "PowerShell"]);
        assert!(pick_tab(&tabs, "Claude Code").is_err());
        assert!(pick_tab(&tabs, "Something else").is_err());
        assert!(pick_tab(&tabs, "✳").is_err());
    }
}
