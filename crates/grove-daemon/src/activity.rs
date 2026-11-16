//! Activity log: JSON lines on disk (rotated at 5 MiB), a ring buffer in memory and live
//! subscribers (the IPC `subscribe` command).

use serde::Serialize;
use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const RING: usize = 500;
const MAX_FILE: u64 = 5 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Event {
    pub ts: u64,
    pub kind: String,
    pub repo: Option<String>,
    pub msg: String,
}

pub struct Activity {
    file: Option<PathBuf>,
    ring: Mutex<VecDeque<Event>>,
    subs: Mutex<Vec<Sender<Event>>>,
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Activity {
    pub fn new(file: Option<PathBuf>) -> Arc<Activity> {
        Arc::new(Activity {
            file,
            ring: Mutex::new(VecDeque::new()),
            subs: Mutex::new(Vec::new()),
        })
    }

    pub fn log(&self, kind: &str, repo: Option<&str>, msg: impl Into<String>) {
        let ev = Event {
            ts: now_secs(),
            kind: kind.to_string(),
            repo: repo.map(str::to_string),
            msg: msg.into(),
        };
        self.append(&ev);
        {
            let mut ring = self.ring.lock().unwrap();
            if ring.len() == RING {
                ring.pop_front();
            }
            ring.push_back(ev.clone());
        }
        self.subs
            .lock()
            .unwrap()
            .retain(|s| s.send(ev.clone()).is_ok());
    }

    fn append(&self, ev: &Event) {
        let Some(path) = &self.file else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_FILE) {
            let _ = std::fs::rename(path, path.with_extension("jsonl.1"));
        }
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path)
            && let Ok(line) = serde_json::to_string(ev)
        {
            let _ = writeln!(f, "{line}");
        }
    }

    pub fn recent(&self, limit: usize) -> Vec<Event> {
        let ring = self.ring.lock().unwrap();
        let skip = ring.len().saturating_sub(limit);
        ring.iter().skip(skip).cloned().collect()
    }

    pub fn subscribe(&self) -> Receiver<Event> {
        let (tx, rx) = channel();
        self.subs.lock().unwrap().push(tx);
        rx
    }
}
