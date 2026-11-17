use std::sync::Arc;
use std::time::Duration;

use nori_config::NoriConfig;
use nori_conversation::ConversationLog;
use nori_conversation::Event;
use nori_harness::runtime::AgentPrepareSpec;
use nori_harness::runtime::LaunchedSession;
use nori_harness::runtime::SessionStart;
use nori_harness::runtime::prepare_and_launch_session;
use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;
use pretty_assertions::assert_eq;
use serial_test::serial;

fn launch(cwd: &std::path::Path) -> LaunchedSession {
    let config = NoriConfig {
        active_agent: "mock-model".to_string(),
        cwd: cwd.to_path_buf(),
        nori_home: cwd.to_path_buf(),
        ..Default::default()
    };
    prepare_and_launch_session(
        AgentPrepareSpec {
            handoff: None,
            config: Arc::new(config),
            cli_version: "conversation-program-test".to_string(),
            session_context: None,
            initial_context: None,
        },
        SessionStart::New,
    )
}

async fn wait_started(session: &mut LaunchedSession) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match session.events.recv().await.expect("session event") {
                SessionEvent::Nori(NoriEvent::SessionStarted(_)) => return,
                SessionEvent::Nori(NoriEvent::SessionEnded(ended)) => {
                    panic!("session ended before startup: {ended:?}")
                }
                SessionEvent::Acp(_) | SessionEvent::Nori(_) => {}
            }
        }
    })
    .await
    .expect("mock ACP session should start");
}

fn commands(log: &ConversationLog) -> Vec<(String, String)> {
    log.records()
        .iter()
        .filter_map(|record| match &record.event {
            Event::Command { name, payload } => Some((name.clone(), payload.clone())),
            Event::Operation { name, args } => Some((name.clone(), args.clone())),
            Event::Message { .. }
            | Event::ToolCall { .. }
            | Event::ToolResult { .. }
            | Event::Rewind { .. }
            | Event::Fork { .. }
            | Event::SessionEvent { .. }
            | Event::Unknown { .. } => None,
        })
        .collect()
}

#[tokio::test]
#[serial]
async fn explicit_unsafe_program_runs_after_journal_append_and_reopen_is_inert() {
    let temp = tempfile::tempdir().expect("temporary test directory");
    let mut session = launch(temp.path());
    wait_started(&mut session).await;
    let recorder = session
        .handle
        .conversation_recorder()
        .await
        .expect("conversation recorder")
        .expect("event log is enabled");
    let journal = recorder.transcript_path().with_extension("pkl");
    let marker = temp.path().join("effect-ran");
    let shell = format!(
        "grep -q pkl-operation {} && printf x >> {}",
        journal.display(),
        marker.display()
    );
    let source = format!("unsafe {{ program = \"/bin/sh\"; args = List(\"-c\", \"{shell}\") }}");

    let result = session
        .handle
        .execute_conversation_program(source)
        .await
        .expect("Pkl program should execute");
    assert_eq!(result.applied, 1);
    assert!(marker.exists(), "explicit process should run after append");

    recorder.flush().await.expect("flush transcript");
    let log_path = journal.clone();
    let before_reopen = std::fs::read(&log_path).expect("read journal");
    let log = ConversationLog::open(&log_path).expect("reopen append-only journal");
    assert!(
        commands(&log)
            .iter()
            .any(|(name, _)| name == "pkl-operation")
    );
    assert_eq!(
        std::fs::read(&log_path).expect("read journal after replay"),
        before_reopen
    );
    assert_eq!(
        std::fs::read_to_string(marker).expect("read effect marker"),
        "x"
    );

    session.handle.shutdown().await.expect("shut down session");
}

#[tokio::test]
#[serial]
async fn path_imports_resolve_relative_to_program_file() {
    let temp = tempfile::tempdir().expect("temporary test directory");
    let mut session = launch(temp.path());
    wait_started(&mut session).await;
    let directory = temp.path().join("program");
    std::fs::create_dir_all(&directory).expect("create program directory");
    std::fs::write(directory.join("base.pkl"), "commands = new Listing {}\n")
        .expect("write imported module");
    std::fs::write(
        directory.join("main.pkl"),
        "import \"base.pkl\"\ncommands = base.commands\n",
    )
    .expect("write main module");

    let result = session
        .handle
        .execute_conversation_program_path(directory.join("main.pkl"))
        .await
        .expect("relative import should resolve from the source file");
    assert_eq!(result.applied, 0);
    session.handle.shutdown().await.expect("shut down session");
}

#[tokio::test]
#[serial]
async fn failed_host_tool_call_is_recorded_as_an_error() {
    let temp = tempfile::tempdir().expect("temporary test directory");
    let mut session = launch(temp.path());
    wait_started(&mut session).await;
    let recorder = session
        .handle
        .conversation_recorder()
        .await
        .expect("conversation recorder")
        .expect("event log is enabled");
    let source = r#"commands = new Listing {
  new Dynamic { op = "tool-request"; id = "missing-tool-1"; tool = "not_a_registered_tool"; input = "{}" }
}"#;

    let error = session
        .handle
        .execute_conversation_program(source.to_string())
        .await
        .expect_err("unknown host tool should fail");
    assert!(error.to_string().contains("tool"));

    let invalid_input = r#"commands = new Listing {
  new Dynamic { op = "tool-request"; id = "invalid-tool-json-1"; tool = "not_a_registered_tool"; input = "invalid json" }
}"#;
    let input_error = session
        .handle
        .execute_conversation_program(invalid_input.to_string())
        .await
        .expect_err("invalid host-tool input should fail");
    assert!(input_error.to_string().contains("invalid input"));

    let log = ConversationLog::open(recorder.transcript_path().with_extension("pkl"))
        .expect("open conversation journal");
    let recorded = commands(&log);
    let tool_results = recorded
        .iter()
        .filter(|(name, _)| name == "tool-result")
        .map(|(_, payload)| {
            serde_json::from_str::<serde_json::Value>(payload).expect("tool result JSON")
        })
        .collect::<Vec<_>>();
    assert_eq!(tool_results.len(), 2);
    assert!(tool_results.iter().all(|result| result["is_error"] == true));
    assert!(
        tool_results
            .iter()
            .any(|result| result["id"] == "missing-tool-1")
    );
    assert!(
        tool_results
            .iter()
            .any(|result| result["id"] == "invalid-tool-json-1")
    );
    session.handle.shutdown().await.expect("shut down session");
}

#[tokio::test]
#[serial]
async fn failed_operation_has_failure_receipt_and_stops_later_effects() {
    let temp = tempfile::tempdir().expect("temporary test directory");
    let mut session = launch(temp.path());
    wait_started(&mut session).await;
    let recorder = session
        .handle
        .conversation_recorder()
        .await
        .expect("conversation recorder")
        .expect("event log is enabled");
    let marker = temp.path().join("later-effect");
    let source = format!(
        "commands = new Listing {{\n  new Dynamic {{ op = \"unsafe\"; program = \"/nori-test-no-such-executable\" }}\n  new Dynamic {{ op = \"unsafe\"; program = \"/bin/sh\"; args = List(\"-c\", \"touch {}\") }}\n}}",
        marker.display()
    );

    let error = session
        .handle
        .execute_conversation_program(source)
        .await
        .expect_err("missing executable should fail the program");
    assert!(error.to_string().contains("nori-test-no-such-executable"));

    let log = ConversationLog::open(recorder.transcript_path().with_extension("pkl"))
        .expect("open conversation journal");
    let failed_receipt = commands(&log)
        .into_iter()
        .filter(|(name, _)| name == "pkl-operation-result")
        .filter_map(|(_, payload)| serde_json::from_str::<serde_json::Value>(&payload).ok())
        .find(|receipt| receipt["success"] == false)
        .expect("failed operation receipt should be persisted");
    assert!(failed_receipt["error"].as_str().is_some());
    assert!(commands(&log).iter().any(|(name, payload)| {
        name == "conversation-program-result"
            && serde_json::from_str::<serde_json::Value>(payload).is_ok_and(|receipt| {
                receipt["program_id"] == failed_receipt["program_id"] && receipt["success"] == false
            })
    }));
    assert!(
        !marker.exists(),
        "later operation must not run after failure"
    );
    session.handle.shutdown().await.expect("shut down session");
}

#[tokio::test]
#[serial]
async fn actor_alias_survives_a_program_boundary_for_send() {
    let temp = tempfile::tempdir().expect("temporary test directory");
    let prompt_log = temp.path().join("prompts.jsonl");
    // SAFETY: this test is serialized with other mock-agent environment users.
    unsafe { std::env::set_var("MOCK_AGENT_PROMPT_LOG", &prompt_log) };
    struct PromptLogEnvGuard;
    impl Drop for PromptLogEnvGuard {
        fn drop(&mut self) {
            // SAFETY: this test is serialized with other mock-agent environment users.
            unsafe { std::env::remove_var("MOCK_AGENT_PROMPT_LOG") };
        }
    }
    let _guard = PromptLogEnvGuard;

    let mut session = launch(temp.path());
    wait_started(&mut session).await;
    let spawn = r#"commands = new Listing {
  new Dynamic { op = "spawn"; actor = "reviewer"; agent = "mock-model" }
}"#;
    let spawned = session
        .handle
        .execute_conversation_program(spawn.to_string())
        .await
        .expect("spawn actor");
    assert_eq!(spawned.applied, 1);

    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let prompts = std::fs::read_to_string(&prompt_log)
                .unwrap_or_default()
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .collect::<Vec<_>>();
            if prompts.iter().any(|prompt| {
                prompt["text"]
                    .as_str()
                    .is_some_and(|text| text.starts_with("Act as the general sub-agent"))
            }) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("child should receive its initial task");

    let send = r#"commands = new Listing {
  new Dynamic { op = "send"; recipient = "reviewer"; message = "send through actor alias" }
}"#;
    session
        .handle
        .execute_conversation_program(send.to_string())
        .await
        .expect("send to actor alias in the next program");

    let prompts = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let prompts = std::fs::read_to_string(&prompt_log)
                .unwrap_or_default()
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .collect::<Vec<_>>();
            if prompts
                .iter()
                .any(|prompt| prompt["text"] == "send through actor alias")
            {
                return prompts;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("child receives follow-up through the persisted alias");
    let child_sessions = prompts
        .iter()
        .filter(|prompt| {
            prompt["text"] == "send through actor alias"
                || prompt["text"]
                    .as_str()
                    .is_some_and(|text| text.starts_with("Act as the general sub-agent"))
        })
        .filter_map(|prompt| prompt["session"].as_str())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        child_sessions.len(),
        1,
        "both turns should target one child"
    );

    session.handle.shutdown().await.expect("shut down session");
}

#[tokio::test]
#[serial]
async fn program_receipts_cover_evaluation_failure_and_empty_success() {
    let temp = tempfile::tempdir().expect("temporary test directory");
    let mut session = launch(temp.path());
    wait_started(&mut session).await;
    let recorder = session
        .handle
        .conversation_recorder()
        .await
        .unwrap()
        .unwrap();
    let error = session
        .handle
        .execute_conversation_program("commands = (".into())
        .await
        .expect_err("invalid Pkl");
    assert!(!error.to_string().is_empty());
    let empty = session
        .handle
        .execute_conversation_program("commands = List()".into())
        .await
        .expect("empty program");
    assert_eq!(empty.applied, 0);

    let log = ConversationLog::open(recorder.transcript_path().with_extension("pkl")).unwrap();
    let records = commands(&log);
    let intents: Vec<serde_json::Value> = records
        .iter()
        .filter(|(name, _)| name == "conversation-program")
        .map(|(_, payload)| serde_json::from_str(payload).unwrap())
        .collect();
    let receipts: Vec<serde_json::Value> = records
        .iter()
        .filter(|(name, _)| name == "conversation-program-result")
        .map(|(_, payload)| serde_json::from_str(payload).unwrap())
        .collect();
    let failed_id = intents
        .iter()
        .find(|intent| intent["source"] == "commands = (")
        .unwrap()["program_id"]
        .clone();
    let failed = receipts
        .iter()
        .find(|receipt| receipt["program_id"] == failed_id)
        .unwrap();
    assert_eq!(failed["success"], false);
    assert!(
        failed["error"]
            .as_str()
            .is_some_and(|error| !error.is_empty())
    );
    let succeeded = receipts
        .iter()
        .find(|receipt| receipt["program_id"] == empty.program_id)
        .unwrap();
    assert_eq!(succeeded["success"], true);
    assert_eq!(succeeded["applied"], 0);
    assert!(
        !records.iter().any(|(name, payload)| name == "pkl-operation"
            && payload.contains(failed_id.as_str().unwrap()))
    );
    session.handle.shutdown().await.unwrap();
}

#[tokio::test]
#[serial]
async fn concurrent_programs_keep_their_own_actor_bindings() {
    // The official Pkl JVM can take minutes to start under filesystem contention.
    // These bounds guard rendezvous, not Pkl startup performance.
    let temp = tempfile::tempdir().expect("temporary test directory");
    let mut session = launch(temp.path());
    wait_started(&mut session).await;
    let recorder = session
        .handle
        .conversation_recorder()
        .await
        .unwrap()
        .unwrap();
    let ready = temp.path().join("actor-a-ready");
    let release = temp.path().join("actor-b-ready");
    let wait_script = format!(
        "touch '{}' ; n=0; while [ ! -e '{}' ] && [ $n -lt 6000 ]; do sleep 0.05; n=$((n + 1)); done; test -e '{}'",
        ready.display(),
        release.display(),
        release.display()
    );
    let wait_script = serde_json::to_string(&wait_script).unwrap();
    let first = format!(
        r#"commands = new Listing {{
        new Dynamic {{ op = "spawn"; actor = "reviewer"; agent = "mock-model" }}
        new Dynamic {{ op = "unsafe"; program = "/bin/sh"; args = List("-c", {wait_script}) }}
        new Dynamic {{ op = "send"; recipient = "reviewer"; message = "follow-up A" }}
    }}"#
    );
    let first_handle = session.handle.clone();
    let first_task =
        tokio::spawn(async move { first_handle.execute_conversation_program(first).await });
    tokio::time::timeout(Duration::from_secs(180), async {
        while !ready.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("first actor must start before second program");
    let release_script = serde_json::to_string(&format!("touch '{}'", release.display())).unwrap();
    let second = format!(
        r#"commands = new Listing {{
        new Dynamic {{ op = "spawn"; actor = "reviewer"; agent = "mock-model" }}
        new Dynamic {{ op = "send"; recipient = "reviewer"; message = "follow-up B" }}
        new Dynamic {{ op = "unsafe"; program = "/bin/sh"; args = List("-c", {release_script}) }}
    }}"#
    );
    let second_result = session
        .handle
        .execute_conversation_program(second)
        .await
        .expect("second program");
    let first_result = first_task.await.unwrap().expect("first program");
    let session_id = |result: &nori_harness::runtime::ConversationProgramResult| -> String {
        let spawn = result.results.first().unwrap()["result"].as_str().unwrap();
        serde_json::from_str::<serde_json::Value>(spawn).unwrap()["session_id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let first_id = session_id(&first_result);
    let second_id = session_id(&second_result);
    assert_ne!(first_id, second_id);
    let log = ConversationLog::open(recorder.transcript_path().with_extension("pkl")).unwrap();
    let sends: Vec<serde_json::Value> = commands(&log)
        .iter()
        .filter(|(name, _)| name == "pkl-actor-send")
        .map(|(_, payload)| serde_json::from_str(payload).unwrap())
        .collect();
    assert_eq!(
        sends
            .iter()
            .find(|send| send["message"] == "follow-up A")
            .unwrap()["session_id"],
        first_id
    );
    assert_eq!(
        sends
            .iter()
            .find(|send| send["message"] == "follow-up B")
            .unwrap()["session_id"],
        second_id
    );
    session.handle.shutdown().await.unwrap();
}

#[tokio::test]
#[serial]
async fn rewind_rejects_an_in_flight_program_effect() {
    let temp = tempfile::tempdir().unwrap();
    let mut session = launch(temp.path());
    wait_started(&mut session).await;
    let recorder = session
        .handle
        .conversation_recorder()
        .await
        .unwrap()
        .unwrap();
    let ready = temp.path().join("effect-ready");
    let release = temp.path().join("release-effect");
    let script = format!(
        "touch '{}' ; n=0; while [ ! -e '{}' ] && [ $n -lt 6000 ]; do sleep 0.05; n=$((n + 1)); done; test -e '{}'",
        ready.display(),
        release.display(),
        release.display()
    );
    let script = serde_json::to_string(&script).unwrap();
    let source = format!(r#"unsafe {{ program = "/bin/sh"; args = List("-c", {script}) }}"#);
    let handle = session.handle.clone();
    let program = tokio::spawn(async move { handle.execute_conversation_program(source).await });
    tokio::time::timeout(Duration::from_secs(180), async {
        while !ready.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("program effect starts");
    let error = session
        .handle
        .rewind_and_rebind(recorder.session_id().to_string(), None)
        .await
        .expect_err("running effect blocks rewind");
    assert!(error.to_string().contains("program effect"));
    std::fs::write(release, "go").unwrap();
    program.await.unwrap().expect("program completes");
    session.handle.shutdown().await.unwrap();
}

#[tokio::test]
#[serial]
async fn program_can_continue_after_its_own_rewind() {
    let temp = tempfile::tempdir().unwrap();
    let mut session = launch(temp.path());
    wait_started(&mut session).await;
    let source = r#"commands = new Listing {
        new Dynamic { op = "rewind"; head = null }
        new Dynamic { op = "tool-result"; id = "after-own-rewind"; output = "continued" }
    }"#;
    let result = session
        .handle
        .execute_conversation_program(source.into())
        .await
        .expect("program continues after its own rewind");
    assert_eq!(result.applied, 2);
    assert_eq!(result.results[0]["result"], "continued");
    session.handle.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
#[serial]
async fn program_compiling_during_rewind_cannot_dispatch_into_new_session() {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let temp = tempfile::tempdir().unwrap();
    let mut session = launch(temp.path());
    wait_started(&mut session).await;
    let recorder = session
        .handle
        .conversation_recorder()
        .await
        .unwrap()
        .unwrap();
    let journal = recorder.transcript_path().with_extension("pkl");
    let fifo = temp.path().join("compile-input");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let source = format!(
        r#"commands = List(new Dynamic {{ op = "tool-result"; id = "old-generation"; output = read("file://{}").text }})"#,
        fifo.display()
    );
    let handle = session.handle.clone();
    let program_source = source.clone();
    let task =
        tokio::spawn(async move { handle.execute_conversation_program(program_source).await });
    let program_id = tokio::time::timeout(Duration::from_secs(180), async {
        loop {
            if let Ok(log) = ConversationLog::open(&journal)
                && let Some(id) = commands(&log)
                    .into_iter()
                    .filter(|(name, _)| name == "conversation-program")
                    .filter_map(|(_, payload)| {
                        serde_json::from_str::<serde_json::Value>(&payload).ok()
                    })
                    .find(|intent| intent["source"] == source)
                    .and_then(|intent| intent["program_id"].as_str().map(str::to_owned))
            {
                break id;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("program intent recorded before compilation");
    let mut writer = tokio::time::timeout(Duration::from_secs(180), async {
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&fifo)
            {
                Ok(writer) => break writer,
                Err(error) if error.raw_os_error() == Some(libc::ENXIO) => {
                    tokio::time::sleep(Duration::from_millis(20)).await
                }
                Err(error) => panic!("open FIFO: {error}"),
            }
        }
    })
    .await
    .expect("Pkl evaluation reads FIFO");
    session
        .handle
        .rewind_and_rebind(recorder.session_id().to_string(), None)
        .await
        .expect("rewind while program evaluates");
    writer.write_all(b"go").unwrap();
    drop(writer);
    let error = tokio::time::timeout(Duration::from_secs(180), task)
        .await
        .unwrap()
        .unwrap()
        .expect_err("stale program must not run");
    assert!(error.to_string().contains("rewound"));
    let log = ConversationLog::open(&journal).unwrap();
    let recorded = commands(&log);
    assert!(
        !recorded
            .iter()
            .any(|(name, payload)| name == "tool-result" && payload.contains("old-generation"))
    );
    assert!(
        recorded
            .iter()
            .filter(|(name, _)| name == "conversation-program-result")
            .any(|(_, payload)| {
                serde_json::from_str::<serde_json::Value>(payload).is_ok_and(|receipt| {
                    receipt["program_id"] == program_id && receipt["success"] == false
                })
            })
    );
    session.handle.shutdown().await.unwrap();
}
