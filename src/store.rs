//! Where events live, appending one, and reading new ones as they arrive
//! (DESIGN.md §2, §10).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::event::Event;

/// How much of an existing file the dashboard replays at start-up.
const REPLAY_BYTES: u64 = 2 * 1024 * 1024;

pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("BELLPANEL_HOME") {
        return PathBuf::from(dir);
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(local).join("bellpanel");
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .unwrap_or_default();
    PathBuf::from(home).join(".bellpanel")
}

/// Where the tool kept its data under its earlier name, `agentdash`.
/// None when `BELLPANEL_HOME` points elsewhere (tests).
pub fn legacy_data_dir() -> Option<PathBuf> {
    if std::env::var_os("BELLPANEL_HOME").is_some() {
        return None;
    }
    std::env::var_os("LOCALAPPDATA").map(|local| PathBuf::from(local).join("agentdash"))
}

pub fn events_path() -> PathBuf {
    data_dir().join("events.jsonl")
}

pub fn settings_path() -> PathBuf {
    data_dir().join("settings.json")
}

pub fn marks_path() -> PathBuf {
    data_dir().join("marks.jsonl")
}

/// Past this size the events file is moved aside before the next append.
pub const ROTATE_BYTES: u64 = 10 * 1024 * 1024;

/// Appends one event as a single write to a file opened for appending, which
/// is what keeps lines from concurrent hooks apart (relied on, not measured).
pub fn append(path: &Path, event: &Event) -> io::Result<()> {
    rotate_if_large(path, ROTATE_BYTES);
    append_line(path, event)
}

/// Moves a file over `limit` bytes to `<name>.1.jsonl`, replacing the one
/// before it, so the events file never grows without bound.
///
/// Best effort: if another process has the file open the rename fails and
/// the append goes to the large file this time. The dashboard notices the
/// new file by its first bytes (see `Tail`) and reads it from the start.
pub fn rotate_if_large(path: &Path, limit: u64) {
    if fs::metadata(path).map(|m| m.len()).unwrap_or(0) <= limit {
        return;
    }
    let _ = fs::rename(path, path.with_extension("1.jsonl"));
}

/// Appends any record as one JSON line, in a single write.
pub fn append_line<T: serde::Serialize>(path: &Path, record: &T) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut line = serde_json::to_string(record).map_err(io::Error::other)?;
    line.push('\n');
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(line.as_bytes())
}

/// Every record in a JSON-lines file; a missing file is empty, bad lines are skipped.
pub fn read_all<T: serde::de::DeserializeOwned>(path: &Path) -> Vec<T> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Follows the events file from a byte offset.
pub struct Tail {
    path: PathBuf,
    offset: u64,
    /// Bytes after the last newline: a line still being written.
    pending: Vec<u8>,
    /// The file's first bytes when last read, to notice it being replaced by
    /// a new file of the same size or larger (rotation). Every line starts
    /// with its own millisecond timestamp, so two files never share them.
    head: Vec<u8>,
}

/// How many leading bytes identify the file.
const HEAD_BYTES: usize = 64;

impl Tail {
    /// Starts at most `REPLAY_BYTES` before the end, at a line boundary.
    pub fn open(path: PathBuf) -> Tail {
        let len = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let mut tail = Tail {
            path,
            offset: len.saturating_sub(REPLAY_BYTES),
            pending: Vec::new(),
            head: Vec::new(),
        };
        if tail.offset > 0 {
            // Landed mid-line: drop the partial first line.
            let _ = tail.read_raw();
            if let Some(nl) = tail.pending.iter().position(|&b| b == b'\n') {
                tail.pending.drain(..=nl);
            }
        }
        tail
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read_raw(&mut self) -> io::Result<()> {
        let mut file = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        let len = file.metadata()?.len();
        let mut head = vec![0u8; (len as usize).min(HEAD_BYTES)];
        file.read_exact(&mut head)?;
        let common = head.len().min(self.head.len());
        if len < self.offset || head[..common] != self.head[..common] {
            // Truncated or replaced: start over.
            self.offset = 0;
            self.pending.clear();
        }
        self.head = head;
        if len == self.offset {
            return Ok(());
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut buf = Vec::with_capacity((len - self.offset) as usize);
        file.take(len - self.offset).read_to_end(&mut buf)?;
        self.offset += buf.len() as u64;
        self.pending.extend_from_slice(&buf);
        Ok(())
    }

    /// Events written since the last call. Malformed lines are skipped.
    pub fn read_new(&mut self) -> io::Result<Vec<Event>> {
        self.read_raw()?;
        let Some(last_nl) = self.pending.iter().rposition(|&b| b == b'\n') else {
            return Ok(Vec::new());
        };
        let complete: Vec<u8> = self.pending.drain(..=last_nl).collect();
        Ok(complete
            .split(|&b| b == b'\n')
            .filter(|line| !line.is_empty())
            .filter_map(|line| serde_json::from_slice::<Event>(line).ok())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Agent, Kind, FORMAT_VERSION};

    fn temp_file(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("bellpanel-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir.join("events.jsonl")
    }

    fn event(session: &str, at: u64) -> Event {
        Event {
            v: FORMAT_VERSION,
            at,
            agent: Agent::Claude,
            kind: Kind::Stop,
            event: "Stop".into(),
            session: session.into(),
            turn: None,
            cwd: String::new(),
            host_pid: None,
            host_exe: None,
            agent_pid: None,
            pane: None,
            text: None,
        }
    }

    #[test]
    fn appended_events_are_read_once() {
        let path = temp_file("once");
        let mut tail = Tail::open(path.clone());
        assert!(
            tail.read_new().unwrap().is_empty(),
            "a missing file is just empty"
        );
        append(&path, &event("a", 1)).unwrap();
        append(&path, &event("b", 2)).unwrap();
        let got: Vec<String> = tail
            .read_new()
            .unwrap()
            .into_iter()
            .map(|e| e.session)
            .collect();
        assert_eq!(got, ["a", "b"]);
        assert!(tail.read_new().unwrap().is_empty());
    }

    #[test]
    fn a_half_written_line_waits_for_its_newline() {
        let path = temp_file("partial");
        append(&path, &event("a", 1)).unwrap();
        let line = serde_json::to_string(&event("b", 2)).unwrap();
        let (head, rest) = line.split_at(10);
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(head.as_bytes())
            .unwrap();
        let mut tail = Tail::open(path.clone());
        assert_eq!(tail.read_new().unwrap().len(), 1);
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(format!("{rest}\n").as_bytes())
            .unwrap();
        assert_eq!(tail.read_new().unwrap()[0].session, "b");
    }

    #[test]
    fn garbage_lines_are_skipped() {
        let path = temp_file("garbage");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "not json\n").unwrap();
        append(&path, &event("a", 1)).unwrap();
        assert_eq!(Tail::open(path).read_new().unwrap().len(), 1);
    }

    #[test]
    fn a_large_file_is_moved_aside_and_followed_anew() {
        let path = temp_file("rotate");
        append(&path, &event("old", 1)).unwrap();
        let mut tail = Tail::open(path.clone());
        assert_eq!(tail.read_new().unwrap().len(), 1);

        rotate_if_large(&path, 10);
        assert!(!path.exists());
        assert!(path.with_extension("1.jsonl").exists());

        append(&path, &event("new", 2)).unwrap();
        let got: Vec<String> = tail
            .read_new()
            .unwrap()
            .into_iter()
            .map(|e| e.session)
            .collect();
        assert_eq!(got, ["new"]);
    }

    #[test]
    fn a_small_file_stays_put() {
        let path = temp_file("no-rotate");
        append(&path, &event("a", 1)).unwrap();
        rotate_if_large(&path, ROTATE_BYTES);
        assert!(path.exists());
    }

    #[test]
    fn truncation_restarts_from_the_top() {
        let path = temp_file("truncate");
        append(&path, &event("a", 1)).unwrap();
        append(&path, &event("b", 2)).unwrap();
        let mut tail = Tail::open(path.clone());
        tail.read_new().unwrap();
        fs::write(&path, "").unwrap();
        append(&path, &event("c", 3)).unwrap();
        assert_eq!(tail.read_new().unwrap()[0].session, "c");
    }
}
