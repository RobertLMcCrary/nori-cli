use crate::ConversationLog;
use crate::Event;
use crate::SliceRef;
use pretty_assertions::assert_eq;
use std::fs;
use std::io;
use std::io::Write;
use std::sync::Arc;
use std::sync::Barrier;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;

fn message(content: &str) -> Event {
    Event::Message {
        role: "user".into(),
        content: content.into(),
    }
}

#[test]
fn rewind_keeps_history_and_replay_reconstructs_branch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.pkl");
    let mut log = ConversationLog::open(&path).unwrap();
    log.append(message("first")).unwrap();
    log.append(message("hidden")).unwrap();
    log.append(Event::Rewind { head: Some(1) }).unwrap();
    log.append(message("replacement\n\"quoted\"")).unwrap();
    let expected = log.projection().clone();
    assert_eq!(
        expected.visible().iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![1, 4]
    );
    let replay = ConversationLog::open(&path).unwrap();
    assert_eq!(replay.projection(), &expected);
    assert_eq!(replay.records().len(), 4);
    assert!(fs::read_to_string(path).unwrap().contains("hidden"));
}

#[test]
fn slices_are_inclusive_and_require_visible_ordered_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = ConversationLog::open(dir.path().join("s.pkl")).unwrap();
    for text in ["one", "two", "three"] {
        log.append(message(text)).unwrap();
    }
    let reference = SliceRef { start: 1, end: 2 };
    assert_eq!(
        log.projection()
            .slice(reference)
            .unwrap()
            .iter()
            .map(|record| record.id)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert!(
        log.projection()
            .slice(SliceRef { start: 2, end: 1 })
            .is_none()
    );
    log.append(Event::Fork { source: reference }).unwrap();
    log.append(Event::Rewind { head: Some(1) }).unwrap();
    assert!(log.projection().slice(reference).is_none());
    assert_eq!(
        log.slice_at(3, reference)
            .unwrap()
            .unwrap()
            .iter()
            .map(|record| record.id)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert!(log.append(Event::Fork { source: reference }).is_err());
    assert_eq!(log.records().len(), 5);
}

#[test]
fn partial_tail_and_unknown_events_are_safe_to_replay() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.pkl");
    fs::write(&path, b"event_1 = new { kind = \"future-op\"; data = \"{}\" }\nevent_2 = new { kind = \"message\"").unwrap();
    let mut log = ConversationLog::open(&path).unwrap();
    assert_eq!(log.records().len(), 1);
    log.append(message("recovered")).unwrap();
    let replay = ConversationLog::open(&path).unwrap();
    assert_eq!(replay.records(), log.records());
    assert_eq!(replay.projection(), log.projection());
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(file, "bad record").unwrap();
    assert!(ConversationLog::open(path).is_err());
}

#[test]
fn replay_ignores_unknown_fields_in_known_events() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("future.pkl");
    let payload = serde_json::json!({"role": "user", "content": "hello", "future": {"nested": 42}})
        .to_string();
    let encoded = serde_json::to_string(&payload).unwrap();
    fs::write(
        &path,
        format!("event_1 = new {{ kind = \"message\"; data = {encoded} }}\n"),
    )
    .unwrap();
    let log = ConversationLog::open(path).unwrap();
    assert_eq!(log.projection().visible()[0].event, message("hello"));
}

#[test]
fn replay_preserves_operations_as_data_without_running_them() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("operations.pkl");
    let effect = dir.path().join("must-not-exist");
    let event = Event::Operation {
        name: "shell".into(),
        args: format!("touch {}", effect.display()),
    };
    let mut log = ConversationLog::open(&path).unwrap();
    log.append(event.clone()).unwrap();

    let reopened = ConversationLog::open(path).unwrap();
    assert_eq!(reopened.projection().visible()[0].event, event);
    assert!(!effect.exists());
}

#[test]
fn shared_transport_checks_head_and_deduplicates_commands_across_reloads() {
    let store = crate::MemoryStore::default();
    let mut first = ConversationLog::with_store(store.clone()).unwrap();
    let mut second = ConversationLog::with_store(store.clone()).unwrap();
    let original = first
        .append_with_id(None, "client-1", message("one"))
        .unwrap();
    assert_eq!(
        second
            .append_with_id(None, "client-1", message("one"))
            .unwrap(),
        original
    );
    assert_eq!(
        second
            .append_with_id(None, "client-2", message("two"))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::WouldBlock
    );
    let second_record = second
        .append_with_id(Some(1), "client-2", message("two"))
        .unwrap();
    assert_eq!(second_record.id, 2);
    assert!(
        first
            .append_with_id(Some(2), "client-2", message("different"))
            .is_err()
    );
    let reopened = ConversationLog::with_store(store).unwrap();
    assert_eq!(reopened.records().len(), 2);
    assert_eq!(reopened.projection().head(), Some(2));
}

#[test]
fn file_store_atomically_compares_and_appends_across_instances() {
    use crate::ConversationStore;
    use std::sync::Arc;
    use std::sync::Barrier;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("race.pkl");
    let barrier = Arc::new(Barrier::new(2));
    let handles = ["first", "second"].map(|content| {
        let path = path.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            let mut store = crate::FileStore::new(path);
            let record = crate::Record {
                id: 1,
                command_id: None,
                event: message(content),
            };
            let encoded = crate::codec::encode(&record).unwrap();
            barrier.wait();
            store.append(0, encoded.as_bytes())
        })
    });
    let results = handles.map(|handle| handle.join().unwrap());
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter_map(|result| result.as_ref().err())
            .map(std::io::Error::kind)
            .collect::<Vec<_>>(),
        vec![std::io::ErrorKind::WouldBlock]
    );
    let replay = ConversationLog::open(path).unwrap();
    assert_eq!(replay.records().len(), 1);
    assert_eq!(replay.projection().head(), Some(1));
}

#[derive(Clone)]
struct RacingStore(Arc<RacingStoreState>);

struct RacingStoreState {
    bytes: Mutex<Vec<u8>>,
    first_appends: AtomicUsize,
    barrier: Barrier,
}

impl RacingStore {
    fn pair() -> (Self, Self) {
        let state = Arc::new(RacingStoreState {
            bytes: Mutex::new(Vec::new()),
            first_appends: AtomicUsize::new(0),
            barrier: Barrier::new(2),
        });
        (Self(state.clone()), Self(state))
    }
}

impl crate::ConversationStore for RacingStore {
    fn read(&self) -> io::Result<Vec<u8>> {
        Ok(self
            .0
            .bytes
            .lock()
            .map_err(|_| io::Error::other("test store lock poisoned"))?
            .clone())
    }

    fn append(&mut self, expected_version: i64, bytes: &[u8]) -> io::Result<()> {
        if self
            .0
            .first_appends
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            < 2
        {
            self.0.barrier.wait();
        }
        let mut current = self
            .0
            .bytes
            .lock()
            .map_err(|_| io::Error::other("test store lock poisoned"))?;
        if i64::try_from(current.len()).map_err(io::Error::other)? != expected_version {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "journal changed"));
        }
        current.extend_from_slice(bytes);
        Ok(())
    }
}

#[test]
fn log_can_refresh_and_retry_after_a_concurrent_compare_and_append_conflict() {
    let (first_store, second_store) = RacingStore::pair();
    let first = ConversationLog::with_store(first_store).unwrap();
    let second = ConversationLog::with_store(second_store).unwrap();
    let handles = [first, second].map(|mut log| {
        std::thread::spawn(move || {
            let result = log.append(message("race"));
            (log, result)
        })
    });
    let mut results = handles.map(|handle| handle.join().unwrap()).into_iter();
    let (first_log, first_result) = results.next().unwrap();
    let (second_log, second_result) = results.next().unwrap();
    let (mut loser, conflict) = match (first_result, second_result) {
        (Ok(_), Err(error)) => (second_log, error),
        (Err(error), Ok(_)) => (first_log, error),
        _ => panic!("exactly one concurrent append must win"),
    };
    assert_eq!(conflict.kind(), io::ErrorKind::WouldBlock);

    let retried = loser
        .append_with_id(Some(1), "retry-after-race", message("retry"))
        .unwrap();
    assert_eq!(retried.id, 2);
    assert_eq!(loser.projection().head(), Some(2));
}
