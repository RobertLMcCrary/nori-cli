//! Helper that owns the scheduling and cancellation of `@` file searches.
//!
//! `ChatComposer` publishes *every* change of the `@token` as
//! `AppEvent::StartFileSearch(query)`.
//! This struct receives those events and decides when to actually run the
//! search (on a thread of its own). It ensures:
//!
//! - The first keystroke is searched at once: results show as soon as one walk
//!   of the directory takes, with no timer in front of it.
//! - At most one search is in flight at any time. While one runs, keystrokes
//!   that extend its query only update the latest query; when it finishes, the
//!   latest query is searched straight away. So a burst of typing costs one
//!   search per search duration rather than one per keystroke, and the results
//!   are never more than one search behind what was typed.
//! - A running search whose query the latest one does not extend (a deletion,
//!   a different token) is cancelled and replaced at once: its results could
//!   only be wrong.

use codex_file_search as file_search;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;

const MAX_FILE_SEARCH_RESULTS: NonZeroUsize = NonZeroUsize::new(20).unwrap();
const NUM_FILE_SEARCH_THREADS: NonZeroUsize = NonZeroUsize::new(2).unwrap();

/// State machine for file-search orchestration.
pub(crate) struct FileSearchManager {
    /// Unified state guarded by one mutex.
    state: Arc<Mutex<SearchState>>,

    search_dir: PathBuf,
    app_tx: AppEventSender,
}

struct SearchState {
    /// Latest query typed by user (updated every keystroke).
    latest_query: String,

    /// If there is an active search, this will be the query being searched.
    active_search: Option<ActiveSearch>,
}

struct ActiveSearch {
    query: String,
    cancellation_token: Arc<AtomicBool>,
}

impl SearchState {
    /// Make `latest_query` the active search, returning its cancellation token.
    fn start_latest(&mut self) -> (String, Arc<AtomicBool>) {
        let cancellation_token = Arc::new(AtomicBool::new(false));
        self.active_search = Some(ActiveSearch {
            query: self.latest_query.clone(),
            cancellation_token: cancellation_token.clone(),
        });
        (self.latest_query.clone(), cancellation_token)
    }
}

impl FileSearchManager {
    pub fn new(search_dir: PathBuf, tx: AppEventSender) -> Self {
        Self {
            state: Arc::new(Mutex::new(SearchState {
                latest_query: String::new(),
                active_search: None,
            })),
            search_dir,
            app_tx: tx,
        }
    }

    /// Call whenever the user edits the `@` token.
    pub fn on_user_query(&self, query: String) {
        let (query, cancellation_token) = {
            #[expect(clippy::unwrap_used)]
            let mut st = self.state.lock().unwrap();
            if query == st.latest_query {
                // No change, nothing to do.
                return;
            }

            // Update latest query.
            st.latest_query.clear();
            st.latest_query.push_str(&query);

            if let Some(active_search) = &st.active_search {
                if query.starts_with(&active_search.query) {
                    // Its results are still worth showing; the search that is running searches
                    // the latest query next.
                    return;
                }
                // The search in flight is definitely obsolete: cancel it and search now.
                active_search
                    .cancellation_token
                    .store(true, Ordering::Relaxed);
            }
            st.start_latest()
        };

        FileSearchManager::spawn_file_search(
            query,
            self.search_dir.clone(),
            self.app_tx.clone(),
            cancellation_token,
            self.state.clone(),
        );
    }

    /// Search `query` on a new thread, then keep searching the latest query for as long as the
    /// user typed something new while the previous search ran.
    fn spawn_file_search(
        query: String,
        search_dir: PathBuf,
        tx: AppEventSender,
        cancellation_token: Arc<AtomicBool>,
        search_state: Arc<Mutex<SearchState>>,
    ) {
        let compute_indices = true;
        std::thread::spawn(move || {
            let mut query = query;
            let mut cancellation_token = cancellation_token;
            loop {
                let matches = file_search::run(
                    &query,
                    MAX_FILE_SEARCH_RESULTS,
                    &search_dir,
                    Vec::new(),
                    NUM_FILE_SEARCH_THREADS,
                    cancellation_token.clone(),
                    compute_indices,
                    true,
                )
                .map(|res| res.matches)
                .unwrap_or_default();

                let is_cancelled = cancellation_token.load(Ordering::Relaxed);
                if !is_cancelled {
                    tx.send(AppEvent::FileSearchResult {
                        query: query.clone(),
                        matches,
                    });
                }

                // Hand over to the next search, unless this one was replaced. Do a pointer
                // comparison to verify that the active search is still the one this thread runs.
                #[expect(clippy::unwrap_used)]
                let mut st = search_state.lock().unwrap();
                let still_active = st.active_search.as_ref().is_some_and(|active_search| {
                    Arc::ptr_eq(&active_search.cancellation_token, &cancellation_token)
                });
                if !still_active {
                    return;
                }
                if st.latest_query == query {
                    st.active_search = None;
                    return;
                }
                (query, cancellation_token) = st.start_latest();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::time::Duration;

    fn manager() -> (
        tempfile::TempDir,
        FileSearchManager,
        tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in ["alpha.rs", "alphabet.rs", "gamma.rs"] {
            std::fs::write(dir.path().join(name), "").expect("write");
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let manager = FileSearchManager::new(dir.path().to_path_buf(), AppEventSender::new(tx));
        (dir, manager, rx)
    }

    /// The query of the next search result, waiting at most long enough for a loaded machine.
    fn next_result(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
    ) -> (String, Vec<String>) {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            match rx.try_recv() {
                Ok(AppEvent::FileSearchResult { query, matches }) => {
                    let mut paths: Vec<String> =
                        matches.into_iter().map(|found| found.path).collect();
                    paths.sort();
                    return (query, paths);
                }
                Ok(_) | Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
        panic!("no search result");
    }

    #[test]
    fn the_first_keystroke_is_searched_at_once() {
        let (_dir, manager, mut rx) = manager();

        manager.on_user_query("alph".to_string());

        // The search started before `on_user_query` returned: nothing waits on a timer.
        let started = manager.state.lock().unwrap().active_search.is_some() || !rx.is_empty();
        assert!(started);
        assert_eq!(
            next_result(&mut rx),
            (
                "alph".to_string(),
                vec!["alpha.rs".to_string(), "alphabet.rs".to_string()]
            )
        );
    }

    #[test]
    fn typing_on_while_a_search_runs_searches_the_latest_query_next() {
        let (_dir, manager, mut rx) = manager();

        for query in ["a", "al", "alp", "alphab"] {
            manager.on_user_query(query.to_string());
        }

        let mut last = next_result(&mut rx);
        while last.0 != "alphab" {
            assert!("alphab".starts_with(&last.0), "{last:?}");
            last = next_result(&mut rx);
        }
        assert_eq!(last.1, vec!["alphabet.rs".to_string()]);
    }

    #[test]
    fn a_query_that_does_not_extend_the_running_one_replaces_it() {
        let (_dir, manager, mut rx) = manager();

        manager.on_user_query("alpha".to_string());
        manager.on_user_query("gam".to_string());

        let mut last = next_result(&mut rx);
        while last.0 != "gam" {
            assert_eq!(last.0, "alpha");
            last = next_result(&mut rx);
        }
        assert_eq!(last.1, vec!["gamma.rs".to_string()]);
    }
}
