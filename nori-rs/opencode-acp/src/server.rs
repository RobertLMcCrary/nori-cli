use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use eventsource_stream::Eventsource;
use futures::Stream;
use futures::StreamExt;
use reqwest::Method;
use serde_json::Value;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::Command;

/// An isolated OpenCode v2 server and its authenticated API client.
pub struct OpenCodeServer {
    child: Child,
    client: reqwest::Client,
    base_url: reqwest::Url,
    password: String,
}

impl OpenCodeServer {
    /// Start a loopback-only server in `cwd`. Its credentials are unique to this process.
    pub async fn start(
        executable: &Path,
        cwd: &Path,
        environment: &HashMap<OsString, OsString>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let server = Self::start_engine(executable, cwd, environment).await?;
        let entries = server.request(Method::GET, &["config"], None).await?;
        let inline = environment
            .get(std::ffi::OsStr::new("OPENCODE_CONFIG_CONTENT"))
            .cloned()
            .or_else(|| std::env::var_os("OPENCODE_CONFIG_CONTENT"));
        let Some(content) = crate::nesting_defaults::overlay(&entries, inline.as_deref())? else {
            return Ok(server);
        };
        server.shutdown().await?;
        let mut environment = environment.clone();
        environment.insert("OPENCODE_CONFIG_CONTENT".into(), content.into());
        Self::start_engine(executable, cwd, &environment).await
    }

    async fn start_engine(
        executable: &Path,
        cwd: &Path,
        environment: &HashMap<OsString, OsString>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let password = uuid::Uuid::new_v4().to_string();
        let mut child = Command::new(executable)
            .args(["serve", "--hostname", "127.0.0.1", "--port", "0"])
            .current_dir(cwd)
            .envs(environment)
            .env("OPENCODE_PASSWORD", &password)
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or("missing OpenCode server stdout")?;
        let mut lines = BufReader::new(stdout).lines();
        let base_url: reqwest::Url = tokio::time::timeout(Duration::from_secs(30), async {
            while let Some(line) = lines.next_line().await? {
                if let Some(address) = line.strip_prefix("server listening on ") {
                    let url: reqwest::Url = address.parse()?;
                    if url.scheme() != "http" || url.host_str() != Some("127.0.0.1") {
                        return Err("OpenCode server reported a non-loopback address".into());
                    }
                    return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(url.join("/api/")?);
                }
            }
            Err("OpenCode server exited before reporting its address".into())
        })
        .await??;
        tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
        let mut server = Self {
            child,
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            base_url,
            password,
        };
        for _ in 0..100 {
            if let Some(status) = server.child.try_wait()? {
                return Err(format!("OpenCode server exited during startup: {status}").into());
            }
            if server.request(Method::GET, &["info"], None).await.is_ok() {
                return Ok(server);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Err("OpenCode server did not become ready within 10 seconds".into())
    }

    /// Send a v2 JSON API request. Each path element is escaped as one URL segment.
    pub async fn request(
        &self,
        method: Method,
        path: &[&str],
        body: Option<&Value>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        self.request_with_query(method, path, &[], body).await
    }

    /// OpenCode can expose its HTTP server before location plugins finish activating.
    /// A 503 during catalog discovery is temporary; other failures remain visible.
    pub(crate) async fn request_catalog(
        &self,
        path: &[&str],
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let result =
                tokio::time::timeout_at(deadline, self.request(Method::GET, path, None)).await?;
            match result {
                Err(error)
                    if error.downcast_ref::<reqwest::Error>().is_some_and(|error| {
                        error.status() == Some(reqwest::StatusCode::SERVICE_UNAVAILABLE)
                    }) && tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep_until(
                        (tokio::time::Instant::now() + Duration::from_millis(200)).min(deadline),
                    )
                    .await;
                }
                result => return result,
            }
        }
    }

    /// Send a v2 JSON API request with query parameters.
    pub async fn request_with_query(
        &self,
        method: Method,
        path: &[&str],
        query: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .map_err(|_| "invalid server URL")?
            .pop_if_empty()
            .extend(path.iter().copied());
        let mut request = self
            .client
            .request(method, url)
            .query(query)
            .basic_auth("opencode", Some(&self.password))
            .timeout(Duration::from_secs(30));
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await?.error_for_status()?;
        let bytes = response.bytes().await?;
        if bytes.is_empty() {
            Ok(Value::Null)
        } else {
            Ok(serde_json::from_slice(&bytes)?)
        }
    }

    /// Subscribe to the server event stream. Consumers must preserve event ordering.
    pub async fn events(
        &self,
    ) -> Result<
        impl Stream<
            Item = Result<
                eventsource_stream::Event,
                eventsource_stream::EventStreamError<reqwest::Error>,
            >,
        > + use<>,
        Box<dyn std::error::Error + Send + Sync>,
    > {
        let url = self.base_url.join("event")?;
        let response = self
            .client
            .get(url)
            .basic_auth("opencode", Some(&self.password))
            .send()
            .await?
            .error_for_status()?;
        Ok(response.bytes_stream().eventsource().boxed())
    }

    /// Stop the server and wait until the process has exited.
    pub async fn shutdown(mut self) -> std::io::Result<()> {
        self.child.kill().await
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::path::Path;
    use std::time::Duration;

    use futures::StreamExt;
    use reqwest::Method;
    use serde_json::json;
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;

    use super::OpenCodeServer;

    #[tokio::test]
    async fn catalog_retries_only_temporary_service_unavailable() {
        for (statuses, expected_requests, succeeds) in
            [(vec![503, 503, 200], 3, true), (vec![401], 1, false)]
        {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let responder = tokio::spawn(async move {
                let mut count = 0;
                for status in statuses {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut buffer = [0; 4096];
                    let _ = stream.read(&mut buffer).await.unwrap();
                    count += 1;
                    let body = if status == 200 { "{}" } else { "" };
                    let response = format!(
                        "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                }
                count
            });
            let child = tokio::process::Command::new("/bin/sleep")
                .arg("60")
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let server = OpenCodeServer {
                child,
                client: reqwest::Client::builder().no_proxy().build().unwrap(),
                base_url: format!("http://{address}/api/").parse().unwrap(),
                password: "test".into(),
            };
            assert_eq!(server.request_catalog(&["model"]).await.is_ok(), succeeds);
            assert_eq!(responder.await.unwrap(), expected_requests);
        }
    }

    #[tokio::test]
    async fn real_server_session_and_event_stream() {
        let Ok(output) = std::process::Command::new("which").arg("opencode").output() else {
            return;
        };
        if !output.status.success() {
            return;
        }
        let executable = String::from_utf8(output.stdout).expect("executable path is UTF-8");
        let Ok(version) = std::process::Command::new(executable.trim())
            .arg("--version")
            .output()
        else {
            return;
        };
        if !String::from_utf8_lossy(&version.stdout).contains("v2.") {
            return;
        }
        let home =
            std::env::temp_dir().join(format!("nori-opencode-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&home).expect("temporary home");
        let mut environment = HashMap::<OsString, OsString>::new();
        environment.insert("HOME".into(), home.as_os_str().into());
        environment.insert("OPENCODE_CONFIG_CONTENT".into(), "{}".into());
        for (name, path) in [
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_CACHE_HOME", "cache"),
        ] {
            environment.insert(name.into(), home.join(path).into());
        }
        let server = OpenCodeServer::start(Path::new(executable.trim()), &home, &environment)
            .await
            .expect("start OpenCode v2");
        let config = server
            .request(Method::GET, &["config"], None)
            .await
            .expect("config");
        assert!(
            config
                .as_array()
                .expect("config entries")
                .iter()
                .any(|entry| {
                    entry["info"]["experimental"]["subagent_depth"]
                        == json!(9_007_199_254_740_991_i64)
                        && entry["info"]["permissions"][0]["effect"] == "allow"
                })
        );
        let mut events = server.events().await.expect("subscribe to events");
        let event = tokio::time::timeout(Duration::from_secs(5), events.next())
            .await
            .expect("event timeout")
            .expect("event stream closed")
            .expect("event parsing");
        let data: serde_json::Value = serde_json::from_str(&event.data).expect("event JSON");
        assert_eq!(data["type"], "server.connected");
        let session = server
            .request(Method::POST, &["session"], Some(&json!({})))
            .await
            .expect("create session");
        assert!(session["data"]["id"].as_str().is_some());
        crate::mcp::register(
            &server,
            vec![agent_client_protocol::schema::McpServer::Http(
                agent_client_protocol::schema::McpServerHttp::new(
                    "test-server",
                    "http://127.0.0.1:1",
                ),
            )],
        )
        .await
        .expect("register HTTP MCP server");
        drop(events);
        server.shutdown().await.expect("stop server");
        environment.insert("OPENCODE_CONFIG_CONTENT".into(), json!({"experimental":{"subagent_depth":1},"agents":{"general":{"permissions":[{"action":"subagent","resource":"*","effect":"deny"}]}}}).to_string().into());
        let server = OpenCodeServer::start(Path::new(executable.trim()), &home, &environment)
            .await
            .expect("start OpenCode with explicit restrictions");
        let config = server
            .request(Method::GET, &["config"], None)
            .await
            .expect("config");
        assert!(
            config
                .as_array()
                .expect("config entries")
                .iter()
                .any(|entry| {
                    entry["info"]["experimental"]["subagent_depth"] == 1
                        && entry["info"]["agents"]["general"]["permissions"][0]["effect"] == "deny"
                })
        );
        let mut effective = None;
        for _ in 0..100 {
            let agents = server
                .request(Method::GET, &["agent"], None)
                .await
                .expect("agents");
            effective = agents["data"].as_array().and_then(|agents| {
                agents
                    .iter()
                    .find(|agent| agent["id"] == "general")
                    .and_then(|agent| agent["permissions"].as_array())
                    .and_then(|rules| rules.iter().rev().find(|rule| rule["action"] == "subagent"))
                    .cloned()
            });
            if effective
                .as_ref()
                .is_some_and(|rule| rule["effect"] == "deny")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(
            effective.expect("general agent permission")["effect"],
            "deny"
        );
        server.shutdown().await.expect("stop restricted server");
        std::fs::remove_dir_all(home).expect("remove temporary home");
    }
}
