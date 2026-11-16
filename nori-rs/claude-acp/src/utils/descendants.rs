//! Reaping the processes a session leaves behind.
//!
//! Killing the Claude CLI orphans whatever its tools started (a running shell
//! command, a dev server), and those carry on after the adapter is gone. The
//! tree is read before the CLI is stopped, because afterwards the orphans are
//! re-parented and can no longer be traced back to us.

use std::collections::HashMap;
use std::time::Duration;

/// Every live descendant of this process, children before their parents' siblings.
#[cfg(unix)]
pub fn descendants() -> Vec<i32> {
    let Ok(output) = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=,ppid="])
        .output()
    else {
        return Vec::new();
    };
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.split_whitespace().map(str::parse::<i32>);
        if let (Some(Ok(pid)), Some(Ok(ppid))) = (fields.next(), fields.next()) {
            children.entry(ppid).or_default().push(pid);
        }
    }
    let mut found = Vec::new();
    let mut frontier = vec![i32::try_from(std::process::id()).unwrap_or(i32::MAX)];
    while let Some(parent) = frontier.pop() {
        for &child in children.get(&parent).into_iter().flatten() {
            found.push(child);
            frontier.push(child);
        }
    }
    found
}

/// Whether `pid` is still a running process.
#[cfg(unix)]
fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Ask `pids` to stop, then force whichever are still there after `grace`.
///
/// Returns as soon as they are all gone, so a clean shutdown costs nothing.
#[cfg(unix)]
pub async fn terminate(pids: &[i32], grace: Duration) {
    let send = |pids: &[i32], signal: i32| {
        for &pid in pids {
            // SAFETY: kill(2) only sends a signal; a stale pid yields ESRCH.
            unsafe { libc::kill(pid, signal) };
        }
    };
    let mut remaining: Vec<i32> = pids.iter().copied().filter(|&pid| alive(pid)).collect();
    if remaining.is_empty() {
        return;
    }
    send(&remaining, libc::SIGTERM);
    let deadline = tokio::time::Instant::now() + grace;
    while tokio::time::Instant::now() < deadline {
        remaining.retain(|&pid| alive(pid));
        if remaining.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    send(&remaining, libc::SIGKILL);
}

#[cfg(not(unix))]
pub fn descendants() -> Vec<i32> {
    Vec::new()
}

#[cfg(not(unix))]
pub async fn terminate(_pids: &[i32], _grace: Duration) {}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn finds_and_stops_a_process_the_adapter_started() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = i32::try_from(child.id()).expect("pid fits");

        let found = descendants();
        assert!(found.contains(&pid), "{pid} not among {found:?}");

        terminate(&[pid], Duration::from_millis(50)).await;
        let status = child.wait().expect("wait");
        assert!(!status.success(), "sleep should have been signalled");
    }
}
