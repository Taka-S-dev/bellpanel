//! Finding the window a session runs in, and bringing it forward
//! (DESIGN.md §5).

use crate::state::folder_of;

/// Processes whose windows are never a session's host. `explorer` owns the
/// desktop and is the ancestor of nearly everything started from it.
const NOT_HOSTS: [&str; 4] = ["explorer", "conhost", "openconsole", "dwm"];
const EDITORS: [&str; 4] = ["code", "code - insiders", "cursor", "windsurf"];

/// Whether `title` names `folder` as one of its ` - ` separated parts, the
/// way editors title their windows (`file - folder - Visual Studio Code`).
pub fn title_names_folder(title: &str, folder: &str) -> bool {
    !folder.is_empty()
        && title
            .split(" - ")
            .any(|part| part.trim().eq_ignore_ascii_case(folder))
}

pub fn is_editor(exe: &str) -> bool {
    EDITORS.contains(&exe.to_lowercase().as_str())
}

/// Where a session runs: the process owning its window, and the agent's own
/// process under it when known (needed to find its tab).
#[derive(Debug, Clone, PartialEq)]
pub struct Host {
    pub pid: u32,
    pub exe: String,
    pub agent_pid: Option<u32>,
}

fn is_host_candidate(exe: &str) -> bool {
    !NOT_HOSTS.contains(&exe.to_lowercase().as_str())
}

/// Whether two Windows paths name the same folder, ignoring case, slash
/// direction and a trailing separator.
pub fn same_dir(a: &str, b: &str) -> bool {
    let norm = |p: &str| p.replace('/', "\\").trim_end_matches('\\').to_lowercase();
    norm(a) == norm(b)
}

#[cfg(windows)]
mod platform {
    use std::collections::{HashMap, HashSet};
    use std::time::Duration;

    use windows::core::{BOOL, PWSTR};
    use windows::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation};
    use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM};
    use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::{
        AttachThreadInput, GetCurrentProcessId, GetCurrentThreadId, OpenProcess,
        QueryFullProcessImageNameW, PROCESS_BASIC_INFORMATION, PROCESS_NAME_WIN32,
        PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, EnumWindows, GetForegroundWindow, GetWindowTextLengthW, GetWindowTextW,
        GetWindowThreadProcessId, IsIconic, IsWindowVisible, SetForegroundWindow, ShowWindow,
        SW_RESTORE,
    };

    use super::{is_editor, is_host_candidate, title_names_folder, Host};

    pub struct Window {
        pub hwnd: isize,
        pub pid: u32,
        pub title: String,
    }

    fn stem(exe_file: &str) -> String {
        let file = exe_file.rsplit(['\\', '/']).next().unwrap_or(exe_file);
        let lower = file.to_lowercase();
        if lower.ends_with(".exe") {
            file[..file.len() - 4].to_string()
        } else {
            file.to_string()
        }
    }

    /// pid -> (parent pid, executable stem) for every running process.
    pub fn processes() -> HashMap<u32, (u32, String)> {
        let mut map = HashMap::new();
        unsafe {
            let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
                return map;
            };
            let mut entry = PROCESSENTRY32W {
                dwSize: size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            let mut ok = Process32FirstW(snapshot, &mut entry).is_ok();
            while ok {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
                map.insert(
                    entry.th32ProcessID,
                    (entry.th32ParentProcessID, stem(&name)),
                );
                ok = Process32NextW(snapshot, &mut entry).is_ok();
            }
            let _ = CloseHandle(snapshot);
        }
        map
    }

    fn title_of(hwnd: HWND) -> String {
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

    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let found = unsafe { &mut *(lparam.0 as *mut Vec<Window>) };
        if unsafe { IsWindowVisible(hwnd) }.as_bool() {
            let title = title_of(hwnd);
            if !title.trim().is_empty() {
                let mut pid = 0u32;
                unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
                found.push(Window {
                    hwnd: hwnd.0 as isize,
                    pid,
                    title,
                });
            }
        }
        BOOL(1)
    }

    /// Visible top-level windows that have a title.
    pub fn windows() -> Vec<Window> {
        let mut found = Vec::new();
        unsafe {
            let _ = EnumWindows(Some(collect), LPARAM(&mut found as *mut _ as isize));
        }
        found
    }

    pub struct Ancestor {
        pub pid: u32,
        pub exe: String,
        pub has_window: bool,
    }

    type Processes = HashMap<u32, (u32, String)>;

    /// `start` and its parent, grandparent and so on, as far as they still run.
    fn chain_from(start: u32, procs: &Processes, with_windows: &HashSet<u32>) -> Vec<Ancestor> {
        let mut chain = Vec::new();
        let mut seen = HashSet::new();
        let mut pid = start;
        while pid != 0 && seen.insert(pid) && seen.len() < 64 {
            let Some((parent, exe)) = procs.get(&pid) else {
                break;
            };
            chain.push(Ancestor {
                pid,
                exe: exe.clone(),
                has_window: with_windows.contains(&pid),
            });
            pid = *parent;
        }
        chain
    }

    /// The first window owner in `chain`, with the agent's process if the
    /// chain passes through it.
    fn first_host(chain: Vec<Ancestor>, agent_exe: &str) -> Option<Host> {
        let agent_pid = chain
            .iter()
            .find(|a| a.exe.eq_ignore_ascii_case(agent_exe))
            .map(|a| a.pid);
        chain
            .into_iter()
            .find(|a| a.has_window && is_host_candidate(&a.exe))
            .map(|a| Host {
                pid: a.pid,
                exe: a.exe,
                agent_pid,
            })
    }

    fn window_owners() -> HashSet<u32> {
        windows().into_iter().map(|w| w.pid).collect()
    }

    /// This process's ancestors, for `BELLPANEL_TRACE`.
    pub fn ancestry() -> Vec<Ancestor> {
        let procs = processes();
        let Some(parent) = procs.get(&unsafe { GetCurrentProcessId() }).map(|p| p.0) else {
            return Vec::new();
        };
        chain_from(parent, &procs, &window_owners())
    }

    use super::same_dir;

    /// The window-owning process a session runs under.
    ///
    /// First this hook's own ancestors. That leads nowhere when the agent runs
    /// hooks from a process with no parent left: Codex runs them in its
    /// background app-server, and a shell that has since exited cuts the
    /// chain too. Then it looks for the agent's own process (`agent_exe`)
    /// working in `cwd` and walks up from that. Two such processes in the same
    /// folder cannot be told apart; the first one found wins.
    pub fn host_process(agent_exe: &str, cwd: &str) -> Option<Host> {
        let procs = processes();
        let with_windows = window_owners();
        let parent = procs.get(&unsafe { GetCurrentProcessId() }).map(|p| p.0)?;
        if let Some(host) = first_host(chain_from(parent, &procs, &with_windows), agent_exe) {
            if host.agent_pid.is_some() {
                return Some(host);
            }
            // The window is right but the agent's process was not on the way
            // (it ran the hook through a helper): look it up by folder.
            let agent_pid = agent_host_in(&procs, &with_windows, agent_exe, cwd)
                .filter(|found| found.pid == host.pid)
                .and_then(|found| found.agent_pid);
            return Some(Host { agent_pid, ..host });
        }
        agent_host_in(&procs, &with_windows, agent_exe, cwd)
    }

    /// The second way of `host_process` on its own: the agent's process
    /// working in `cwd`, and the window it runs under.
    pub fn agent_host(agent_exe: &str, cwd: &str) -> Option<Host> {
        agent_host_in(&processes(), &window_owners(), agent_exe, cwd)
    }

    fn agent_host_in(
        procs: &Processes,
        with_windows: &HashSet<u32>,
        agent_exe: &str,
        cwd: &str,
    ) -> Option<Host> {
        if cwd.is_empty() {
            return None;
        }
        let mut agents: Vec<u32> = procs
            .iter()
            .filter(|(_, (_, exe))| exe.eq_ignore_ascii_case(agent_exe))
            .map(|(&pid, _)| pid)
            .collect();
        agents.sort_unstable();
        agents
            .into_iter()
            .filter(|&pid| process_cwd(pid).is_some_and(|dir| same_dir(&dir, cwd)))
            .find_map(|pid| {
                first_host(chain_from(pid, procs, with_windows), agent_exe).map(|host| Host {
                    agent_pid: Some(pid),
                    ..host
                })
            })
    }

    /// Full path of a process's executable.
    pub fn exe_path(pid: u32) -> Option<std::path::PathBuf> {
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut buffer = vec![0u16; 1024];
            let mut length = buffer.len() as u32;
            let result = QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                PWSTR(buffer.as_mut_ptr()),
                &mut length,
            );
            let _ = CloseHandle(handle);
            result.ok()?;
            Some(std::path::PathBuf::from(String::from_utf16_lossy(
                &buffer[..length as usize],
            )))
        }
    }

    /// Reads one value out of another process's memory.
    unsafe fn read_remote<T: Copy + Default>(process: HANDLE, address: usize) -> Option<T> {
        let mut value = T::default();
        let target = &mut value as *mut T as *mut core::ffi::c_void;
        unsafe { ReadProcessMemory(process, address as *const _, target, size_of::<T>(), None) }
            .ok()?;
        Some(value)
    }

    /// The current directory of another process of this user, read from its
    /// PEB (64-bit layout: `ProcessParameters` at 0x20, and in it
    /// `CurrentDirectory.DosPath` at 0x38). Windows offers no direct call.
    pub fn process_cwd(pid: u32) -> Option<String> {
        const PARAMETERS: usize = 0x20;
        const CURRENT_DIRECTORY: usize = 0x38;
        unsafe {
            let process =
                OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid).ok()?;
            let result = (|| {
                let mut info = PROCESS_BASIC_INFORMATION::default();
                let status = NtQueryInformationProcess(
                    process,
                    ProcessBasicInformation,
                    &mut info as *mut _ as *mut core::ffi::c_void,
                    size_of::<PROCESS_BASIC_INFORMATION>() as u32,
                    std::ptr::null_mut(),
                );
                if status.is_err() || info.PebBaseAddress.is_null() {
                    return None;
                }
                let parameters: usize =
                    read_remote(process, info.PebBaseAddress as usize + PARAMETERS)?;
                let length: u16 = read_remote(process, parameters + CURRENT_DIRECTORY)?;
                let buffer: usize = read_remote(process, parameters + CURRENT_DIRECTORY + 8)?;
                if length == 0 || buffer == 0 {
                    return None;
                }
                let mut chars = vec![0u16; length as usize / 2];
                let target = chars.as_mut_ptr() as *mut core::ffi::c_void;
                ReadProcessMemory(process, buffer as *const _, target, length as usize, None)
                    .ok()?;
                Some(String::from_utf16_lossy(&chars))
            })();
            let _ = CloseHandle(process);
            result
        }
    }

    fn exe_of(pid: u32) -> Option<String> {
        processes().remove(&pid).map(|(_, exe)| exe)
    }

    /// The window to bring forward for a session, if one can be found.
    pub fn pick_window(
        host_pid: Option<u32>,
        host_exe: Option<&str>,
        folder: &str,
    ) -> Option<isize> {
        let all = windows();
        if let (Some(pid), Some(exe)) = (host_pid, host_exe) {
            // A reused pid would point at an unrelated program.
            if exe_of(pid).is_some_and(|now| now.eq_ignore_ascii_case(exe)) {
                let own: Vec<&Window> = all.iter().filter(|w| w.pid == pid).collect();
                if let Some(w) = own
                    .iter()
                    .find(|w| title_names_folder(&w.title, folder))
                    .or(own.first())
                {
                    return Some(w.hwnd);
                }
            }
        }
        let procs = processes();
        let named: Vec<&Window> = all
            .iter()
            .filter(|w| title_names_folder(&w.title, folder))
            .collect();
        named
            .iter()
            .find(|w| procs.get(&w.pid).is_some_and(|(_, exe)| is_editor(exe)))
            .or(named.first())
            .map(|w| w.hwnd)
    }

    fn try_focus(hwnd: HWND) {
        unsafe {
            if IsIconic(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
            // A process running under a terminal is not the foreground process,
            // so a bare SetForegroundWindow is refused. Joining the foreground
            // thread's input queue lifts that (measured in spike/FINDINGS.md).
            let foreground_thread = GetWindowThreadProcessId(GetForegroundWindow(), None);
            let own_thread = GetCurrentThreadId();
            let attached = foreground_thread != 0
                && foreground_thread != own_thread
                && AttachThreadInput(own_thread, foreground_thread, true).as_bool();
            let _ = BringWindowToTop(hwnd);
            let _ = SetForegroundWindow(hwnd);
            if attached {
                let _ = AttachThreadInput(own_thread, foreground_thread, false);
            }
        }
    }

    /// Brings the window forward and checks that it stayed there, retrying once.
    pub fn focus(handle: isize) -> bool {
        let hwnd = HWND(handle as *mut core::ffi::c_void);
        for _ in 0..2 {
            try_focus(hwnd);
            std::thread::sleep(Duration::from_millis(250));
            if unsafe { GetForegroundWindow() } == hwnd {
                return true;
            }
        }
        false
    }
}

#[cfg(not(windows))]
mod platform {
    pub struct Ancestor {
        pub pid: u32,
        pub exe: String,
        pub has_window: bool,
    }
    pub fn ancestry() -> Vec<Ancestor> {
        Vec::new()
    }
    pub fn processes() -> std::collections::HashMap<u32, (u32, String)> {
        std::collections::HashMap::new()
    }
    pub fn process_cwd(_pid: u32) -> Option<String> {
        None
    }
    pub fn host_process(_agent_exe: &str, _cwd: &str) -> Option<super::Host> {
        None
    }
    pub fn agent_host(_agent_exe: &str, _cwd: &str) -> Option<super::Host> {
        None
    }
    pub fn exe_path(_pid: u32) -> Option<std::path::PathBuf> {
        None
    }
    pub fn pick_window(_: Option<u32>, _: Option<&str>, _: &str) -> Option<isize> {
        None
    }
    pub fn focus(_: isize) -> bool {
        false
    }
}

pub use platform::{
    agent_host, ancestry, exe_path, focus, host_process, pick_window, process_cwd, processes,
    Ancestor,
};

/// What the dashboard knows about where a session runs. Owned, so it can be
/// handed to the thread that does the (slow) focusing.
pub struct Target {
    pub agent_exe: String,
    pub host: Option<Host>,
    /// WezTerm pane id, when the session runs in WezTerm.
    pub pane: Option<String>,
    pub cwd: String,
}

/// Brings a session's window forward, and its tab when the terminal allows.
/// Returns a note about the tab for the status line.
///
/// Uses the window recorded by the hook; failing that, the window over the
/// agent's process still working in that folder (for events whose hook found
/// no window); failing that, a window titled with the folder name.
pub fn focus_session(target: &Target) -> Result<Option<String>, String> {
    let folder = folder_of(&target.cwd);
    let looked_up = || agent_host(&target.agent_exe, &target.cwd);
    let host = match &target.host {
        None => looked_up(),
        // The tab needs the agent's process; records from before it was kept lack it.
        Some(h) if h.agent_pid.is_none() => looked_up()
            .filter(|found| found.pid == h.pid)
            .or(Some(h.clone())),
        Some(h) => Some(h.clone()),
    };
    let hwnd = pick_window(
        host.as_ref().map(|h| h.pid),
        host.as_ref().map(|h| h.exe.as_str()),
        folder,
    )
    .ok_or_else(|| format!("no window found for {folder}"))?;

    // The tab first: selecting it in a window still behind others works, and
    // the window then comes forward showing the right one.
    let tab = host
        .as_ref()
        .and_then(|h| crate::tabs::select(h, target.pane.as_deref(), hwnd));
    if focus(hwnd) {
        Ok(tab)
    } else {
        Err(format!("Windows kept the focus away from {folder}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_titles_name_their_folder() {
        let title = "main.rs - bellpanel - Visual Studio Code";
        assert!(title_names_folder(title, "bellpanel"));
        assert!(title_names_folder(title, "BellPanel"));
        assert!(!title_names_folder(title, "agent"));
        assert!(!title_names_folder(title, ""));
    }

    #[test]
    fn the_desktop_shell_is_never_a_host() {
        assert!(!is_host_candidate("explorer"));
        assert!(is_host_candidate("WindowsTerminal"));
    }

    #[cfg(windows)]
    #[test]
    fn this_test_process_has_some_ancestry() {
        // Runs under cargo, which may or may not sit under a windowed terminal;
        // the point is that walking the tree does not fail or loop.
        let _ = host_process("no-such-agent", "C:\\");
        assert!(!ancestry().is_empty());
    }
}
