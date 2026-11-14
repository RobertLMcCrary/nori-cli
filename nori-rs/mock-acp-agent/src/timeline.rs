//! Wall-clock event log for performance probes.
//!
//! When `MOCK_AGENT_TIMELINE_FILE=<path>` is set, [`mark`] appends one
//! `<label> <epoch microseconds>` line per call so a probe can line the agent's
//! view of time up with what it saw on the terminal.

use std::fs::File;
use std::io::Write;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

static TIMELINE: OnceLock<Option<Mutex<File>>> = OnceLock::new();

pub(crate) fn mark(label: &str) {
    let file = TIMELINE.get_or_init(|| {
        let path = std::env::var_os("MOCK_AGENT_TIMELINE_FILE")?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()?;
        Some(Mutex::new(file))
    });
    let Some(file) = file else {
        return;
    };
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_micros());
    if let Ok(mut file) = file.lock() {
        let _ = writeln!(file, "{label} {micros}");
    }
}
