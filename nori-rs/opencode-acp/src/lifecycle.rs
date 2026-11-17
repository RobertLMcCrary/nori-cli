//! Session attachment, listing, forking and cleanup for the OpenCode v2 adapter.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use acp::Client;
use acp::ConnectionTo;
use acp::schema::CloseSessionRequest;
use acp::schema::CloseSessionResponse;
use acp::schema::DeleteSessionRequest;
use acp::schema::DeleteSessionResponse;
use acp::schema::ForkSessionRequest;
use acp::schema::ForkSessionResponse;
use acp::schema::ListSessionsRequest;
use acp::schema::ListSessionsResponse;
use acp::schema::LoadSessionRequest;
use acp::schema::LoadSessionResponse;
use acp::schema::McpServer;
use acp::schema::ResumeSessionRequest;
use acp::schema::ResumeSessionResponse;
use acp::schema::SessionConfigOption;
use acp::schema::SessionInfo;
use agent_client_protocol as acp;
use reqwest::Method;
use serde_json::json;
use tokio::sync::Mutex;
use tokio::sync::Notify;

use crate::OpenCodeServer;
use crate::agent::Adapter;
use crate::agent::Session;
use crate::config::Catalog;

impl Session {
    pub(crate) fn new(server: Arc<OpenCodeServer>, catalog: Catalog) -> Self {
        Self {
            server,
            catalog: Mutex::new(catalog),
            turn: Arc::new(Mutex::new(())),
            cancelled: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            native_child: false,
            cancel_notify: Notify::new(),
            permission_reply: Mutex::new(()),
        }
    }
}

impl Adapter {
    pub(crate) async fn resolve_session(&self, id: &str) -> acp::Result<Arc<Session>> {
        if let Some(session) = self.sessions.lock().await.get(id).cloned() {
            return Ok(session);
        }
        let parent = self
            .known_children
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| acp::Error::invalid_params().data("Unknown session"))?;
        if parent.closed.load(Ordering::SeqCst) {
            return Err(acp::Error::invalid_params().data("Unknown session"));
        }
        let existing = parent
            .server
            .request(Method::GET, &["session", id], None)
            .await
            .map_err(|error| acp::Error::internal_error().data(error.to_string()))?;
        let catalog = Catalog::load(&parent.server, &existing["data"]).await?;
        let mut session = Session::new(parent.server.clone(), catalog);
        session.native_child = true;
        let mut sessions = self.sessions.lock().await;
        Ok(sessions
            .entry(id.to_owned())
            .or_insert_with(|| Arc::new(session))
            .clone())
    }

    async fn attach(
        &self,
        id: &str,
        cwd: &Path,
        additional: Option<Vec<PathBuf>>,
        mcp_servers: Vec<McpServer>,
    ) -> acp::Result<(Session, Vec<SessionConfigOption>)> {
        if !cwd.is_absolute() {
            return Err(acp::Error::invalid_params().data("OpenCode requires an absolute cwd"));
        }
        let server = OpenCodeServer::start(&self.executable, cwd, &self.environment)
            .await
            .map_err(|error| acp::Error::internal_error().data(error.to_string()))?;
        let existing = server
            .request(Method::GET, &["session", id], None)
            .await
            .map_err(|error| acp::Error::internal_error().data(error.to_string()))?;
        let actual = existing["data"]["location"]["directory"]
            .as_str()
            .ok_or_else(|| {
                acp::Error::internal_error().data("OpenCode session has no directory")
            })?;
        if std::fs::canonicalize(cwd).map_err(acp::Error::into_internal_error)?
            != std::fs::canonicalize(actual).map_err(acp::Error::into_internal_error)?
        {
            return Err(acp::Error::invalid_params().data("Session directory does not match cwd"));
        }
        if let Some(additional) = additional {
            let grant = crate::directories::update(cwd, &existing["data"], additional)?;
            server
                .request(Method::PATCH, &["session", id], Some(&grant))
                .await
                .map_err(|error| acp::Error::internal_error().data(error.to_string()))?;
        }
        crate::mcp::register(&server, mcp_servers).await?;
        let catalog = Catalog::load(&server, &existing["data"]).await?;
        let options = catalog.options()?;
        let mut session = Session::new(Arc::new(server), catalog);
        session.native_child = existing["data"]["parentID"].is_string();
        Ok((session, options))
    }

    pub(crate) async fn load_session(
        &self,
        request: LoadSessionRequest,
        client: ConnectionTo<Client>,
    ) -> acp::Result<LoadSessionResponse> {
        let id = request.session_id.to_string();
        let (session, options) = self
            .attach(
                &id,
                &request.cwd,
                Some(request.additional_directories),
                request.mcp_servers,
            )
            .await?;
        crate::replay::history(&session.server, &client, &id, &request.cwd).await?;
        self.sessions.lock().await.insert(id, Arc::new(session));
        Ok(LoadSessionResponse::new().config_options(options))
    }

    pub(crate) async fn resume_session(
        &self,
        request: ResumeSessionRequest,
    ) -> acp::Result<ResumeSessionResponse> {
        let id = request.session_id.to_string();
        let (session, options) = self
            .attach(
                &id,
                &request.cwd,
                Some(request.additional_directories),
                request.mcp_servers,
            )
            .await?;
        self.sessions.lock().await.insert(id, Arc::new(session));
        Ok(ResumeSessionResponse::new().config_options(options))
    }

    pub(crate) async fn fork_session(
        &self,
        request: ForkSessionRequest,
    ) -> acp::Result<ForkSessionResponse> {
        let source_id = request.session_id.to_string();
        let (session, _) = self
            .attach(&source_id, &request.cwd, None, request.mcp_servers)
            .await?;
        let forked = session
            .server
            .request(
                Method::POST,
                &["session", &source_id, "fork"],
                Some(&json!({})),
            )
            .await
            .map_err(|error| acp::Error::internal_error().data(error.to_string()))?;
        let id = forked["data"]["id"]
            .as_str()
            .ok_or_else(|| acp::Error::internal_error().data("OpenCode fork has no session id"))?
            .to_owned();
        let grant = crate::directories::update(
            &request.cwd,
            &forked["data"],
            request.additional_directories,
        )?;
        session
            .server
            .request(Method::PATCH, &["session", &id], Some(&grant))
            .await
            .map_err(|error| acp::Error::internal_error().data(error.to_string()))?;
        let catalog = Catalog::load(&session.server, &forked["data"]).await?;
        let options = catalog.options()?;
        self.sessions
            .lock()
            .await
            .insert(id.clone(), Arc::new(Session::new(session.server, catalog)));
        Ok(ForkSessionResponse::new(id).config_options(options))
    }

    pub(crate) async fn list_sessions(
        &self,
        request: ListSessionsRequest,
    ) -> acp::Result<ListSessionsResponse> {
        if request.cwd.as_ref().is_some_and(|cwd| !cwd.is_absolute()) {
            return Err(acp::Error::invalid_params().data("cwd must be absolute"));
        }
        let cwd = std::fs::canonicalize(
            request
                .cwd
                .clone()
                .unwrap_or(std::env::current_dir().map_err(acp::Error::into_internal_error)?),
        )
        .map_err(acp::Error::into_internal_error)?;
        let server = OpenCodeServer::start(&self.executable, &cwd, &self.environment)
            .await
            .map_err(|error| acp::Error::internal_error().data(error.to_string()))?;
        let mut query = vec![("order", "desc"), ("limit", "100")];
        if request.cwd.is_some() {
            query.push((
                "directory",
                cwd.to_str().ok_or_else(acp::Error::invalid_params)?,
            ));
        }
        if let Some(cursor) = request.cursor.as_deref() {
            query.push(("cursor", cursor));
        }
        let page = server
            .request_with_query(Method::GET, &["session"], &query, None)
            .await
            .map_err(|error| acp::Error::internal_error().data(error.to_string()))?;
        let sessions = page["data"]
            .as_array()
            .ok_or_else(|| acp::Error::internal_error().data("Invalid OpenCode session list"))?
            .iter()
            .filter_map(|session| {
                let id = session["id"].as_str()?;
                let cwd = session["location"]["directory"].as_str()?;
                let additional = session["metadata"]["opencode.acp.additionalDirectories"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|path| path.as_str().map(PathBuf::from))
                    .collect();
                Some(
                    SessionInfo::new(id.to_owned(), cwd)
                        .title(session["title"].as_str().map(ToOwned::to_owned))
                        .additional_directories(additional),
                )
            })
            .collect();
        Ok(ListSessionsResponse::new(sessions)
            .next_cursor(page["cursor"]["next"].as_str().map(ToOwned::to_owned)))
    }

    pub(crate) async fn close_session(
        &self,
        request: CloseSessionRequest,
    ) -> acp::Result<CloseSessionResponse> {
        let id = request.session_id.to_string();
        self.known_children.lock().await.remove(&id);
        let session = self.sessions.lock().await.remove(&id);
        if let Some(session) = session {
            session.closed.store(true, Ordering::SeqCst);
            self.known_children
                .lock()
                .await
                .retain(|_, parent| !Arc::ptr_eq(parent, &session));
            let _permission_reply = session.permission_reply.lock().await;
            session.cancelled.store(true, Ordering::SeqCst);
            session.cancel_notify.notify_one();
            session
                .server
                .request(
                    Method::POST,
                    &["session", &id, "interrupt"],
                    Some(&json!({})),
                )
                .await
                .map_err(|error| acp::Error::internal_error().data(error.to_string()))?;
        }
        Ok(CloseSessionResponse::new())
    }

    pub(crate) async fn delete_session(
        &self,
        request: DeleteSessionRequest,
    ) -> acp::Result<DeleteSessionResponse> {
        let id = request.session_id.to_string();
        self.known_children.lock().await.remove(&id);
        let session = self.sessions.lock().await.remove(&id);
        if let Some(session) = session {
            session.closed.store(true, Ordering::SeqCst);
            self.known_children
                .lock()
                .await
                .retain(|_, parent| !Arc::ptr_eq(parent, &session));
            let _permission_reply = session.permission_reply.lock().await;
            session.cancelled.store(true, Ordering::SeqCst);
            session.cancel_notify.notify_one();
            session
                .server
                .request(Method::DELETE, &["session", &id], None)
                .await
                .map_err(|error| acp::Error::internal_error().data(error.to_string()))?;
        } else {
            let cwd = std::env::current_dir().map_err(acp::Error::into_internal_error)?;
            let server = OpenCodeServer::start(&self.executable, &cwd, &self.environment)
                .await
                .map_err(|error| acp::Error::internal_error().data(error.to_string()))?;
            if let Err(error) = server
                .request(Method::DELETE, &["session", &id], None)
                .await
                && !error
                    .downcast_ref::<reqwest::Error>()
                    .is_some_and(|error| error.status() == Some(reqwest::StatusCode::NOT_FOUND))
            {
                return Err(acp::Error::internal_error().data(error.to_string()));
            }
        }
        Ok(DeleteSessionResponse::new())
    }
}
