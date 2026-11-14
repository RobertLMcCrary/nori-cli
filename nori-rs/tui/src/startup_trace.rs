//! Startup timing for profiling the path to the first frame.
//!
//! With `NORI_STARTUP_TRACE=<path>` set, every [`mark`] appends `<label> <microseconds>` to that
//! file, counted from the first mark. Without it a mark is one relaxed atomic load.

use std::io::Write;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Instant;

struct Trace {
    start: Instant,
    file: Mutex<std::fs::File>,
}

static TRACE: OnceLock<Option<Trace>> = OnceLock::new();

/// Record that startup reached `label`.
pub fn mark(label: &str) {
    let trace = TRACE.get_or_init(|| {
        let path = std::env::var_os("NORI_STARTUP_TRACE")?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()?;
        Some(Trace {
            start: Instant::now(),
            file: Mutex::new(file),
        })
    });
    if let Some(trace) = trace
        && let Ok(mut file) = trace.file.lock()
    {
        let _ = writeln!(file, "{label} {}", trace.start.elapsed().as_micros());
    }
}
