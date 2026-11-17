use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;

use serde_json::Value;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::ChildStdin;
use tokio::process::ChildStdout;
use tokio::sync::Mutex;

pub(crate) struct Engine {
    child: Mutex<Child>,
    pub(crate) catalog: Value,
    input: Mutex<ChildStdin>,
    output: Mutex<BufReader<ChildStdout>>,
}

impl Engine {
    pub(crate) async fn start(
        node: &Path,
        module: &Path,
        cwd: &Path,
        environment: &HashMap<OsString, OsString>,
        init: Value,
    ) -> std::io::Result<Self> {
        let mut command = tokio::process::Command::new(node);
        command.args(["--input-type=module", "-e", include_str!("engine.mjs")]);
        command
            .envs(environment)
            .env("NORI_GEMINI_CORE_MODULE", module)
            .current_dir(cwd);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let input = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("Missing engine stdin"))?;
        let output = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("Missing engine stdout"))?;
        let mut engine = Self {
            child: Mutex::new(child),
            catalog: Value::Null,
            input: Mutex::new(input),
            output: Mutex::new(BufReader::new(output)),
        };
        engine.send(&init).await?;
        let response = engine.receive().await?;
        if response["type"] != "ready" {
            return Err(std::io::Error::other(format!(
                "Gemini engine failed to initialize: {response}"
            )));
        }
        engine.catalog = response;
        Ok(engine)
    }

    pub(crate) async fn stop(&self) {
        let _ = self.child.lock().await.kill().await;
    }

    pub(crate) async fn send(&self, message: &Value) -> std::io::Result<()> {
        let mut input = self.input.lock().await;
        let mut bytes = serde_json::to_vec(message)?;
        bytes.push(b'\n');
        input.write_all(&bytes).await
    }

    pub(crate) async fn receive(&self) -> std::io::Result<Value> {
        let mut line = String::new();
        if self.output.lock().await.read_line(&mut line).await? == 0 {
            return Err(std::io::Error::other("Gemini engine exited"));
        }
        serde_json::from_str(&line).map_err(std::io::Error::other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::path::PathBuf;

    // These tests use Gemini Core's fakeResponses transport and never call a model API.
    async fn test_engine(fixture: &str) -> Engine {
        let node = std::env::var_os("NORI_GEMINI_TEST_NODE").expect("set NORI_GEMINI_TEST_NODE");
        let module =
            std::env::var_os("NORI_GEMINI_TEST_MODULE").expect("set NORI_GEMINI_TEST_MODULE");
        let cwd = std::env::temp_dir().join(format!("nori-gemini-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&cwd).unwrap();
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(fixture);
        let data = std::fs::read_to_string(source).unwrap().replace(
            "/nori-gemini-nonexistent-test-file",
            &cwd.join("absent").to_string_lossy(),
        );
        let fixture = cwd.join("responses.jsonl");
        std::fs::write(&fixture, data).unwrap();
        let env = HashMap::from([(
            OsString::from("GEMINI_API_KEY"),
            OsString::from("offline-fake-api-key"),
        )]);
        Engine::start(Path::new(&node), Path::new(&module), &cwd, &env, json!({"type":"init", "sessionId":uuid::Uuid::new_v4().to_string(), "cwd":cwd, "auth":"gemini", "model":"gemini-2.5-pro", "fakeResponses":fixture})).await.unwrap()
    }

    #[tokio::test]
    #[ignore = "requires a local installation of official Gemini Core 0.68.0"]
    async fn fake_response_streams_and_completes() {
        let engine = test_engine("text.jsonl").await;
        engine
            .send(&json!({"type":"prompt", "parts":[{"text":"hello"}]}))
            .await
            .unwrap();
        let content = engine.receive().await.unwrap();
        assert_eq!(
            content,
            json!({"type":"content", "value":"hello from official Gemini Core fake transport"})
        );
        assert_eq!(
            engine.receive().await.unwrap(),
            json!({"type":"done", "reason":"end_turn"})
        );
    }

    #[tokio::test]
    #[ignore = "requires a local installation of official Gemini Core 0.68.0"]
    async fn fake_tool_waits_for_permission_and_returns_result() {
        let engine = test_engine("tool.jsonl").await;
        engine
            .send(&json!({"type":"prompt", "parts":[{"text":"read file"}]}))
            .await
            .unwrap();
        let first = engine.receive().await.unwrap();
        assert_eq!(first["status"], "pending");
        let permission = engine.receive().await.unwrap();
        assert_eq!(permission["type"], "permission");
        engine
            .send(&json!({"type":"permission", "id":permission["id"], "allow":false}))
            .await
            .unwrap();
        loop {
            let event = engine.receive().await.unwrap();
            if event["type"] == "done" {
                assert_eq!(event["reason"], "end_turn");
                break;
            }
            assert_ne!(event["type"], "error");
        }
    }
    #[tokio::test]
    #[ignore = "requires a local installation of official Gemini Core 0.68.0"]
    async fn cancellation_interrupts_pending_permission() {
        let engine = test_engine("tool.jsonl").await;
        engine
            .send(&json!({"type":"prompt", "parts":[{"text":"read file"}]}))
            .await
            .unwrap();
        assert_eq!(engine.receive().await.unwrap()["status"], "pending");
        assert_eq!(engine.receive().await.unwrap()["type"], "permission");
        engine.send(&json!({"type":"cancel"})).await.unwrap();
        loop {
            let event = engine.receive().await.unwrap();
            if event["type"] == "done" {
                assert_eq!(event["reason"], "cancelled");
                break;
            }
            assert_ne!(event["type"], "error");
        }
    }

    #[tokio::test]
    #[ignore = "requires a local installation of official Gemini Core 0.68.0"]
    async fn model_selection_updates_the_engine() {
        let engine = test_engine("text.jsonl").await;
        engine
            .send(&json!({"type":"set_model", "model":"gemini-2.5-flash"}))
            .await
            .unwrap();
        assert_eq!(
            engine.receive().await.unwrap(),
            json!({"type":"configured", "model":"gemini-2.5-flash"})
        );
    }

    #[tokio::test]
    #[ignore = "requires a local installation of official Gemini Core 0.68.0"]
    async fn stopping_engine_closes_its_output() {
        let engine = test_engine("text.jsonl").await;
        engine.stop().await;
        assert!(engine.receive().await.is_err());
    }
}
