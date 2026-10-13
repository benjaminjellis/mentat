mod oauth;
mod store;
pub use store::Store;

use anyhow::{Context, Result, bail, ensure};
use lineark_sdk::{Client, LinearError};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::future::Future;
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

pub const REDIRECT_URI: &str = "http://127.0.0.1:43821/oauth/callback";

#[derive(Clone)]
pub struct Config {
    pub client_id: String,
    pub api_key: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Workspace {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Issue {
    pub id: String,
    pub identifier: String,
    pub title: String,
    pub description: Option<String>,
    pub url: String,
    pub state: Named,
    pub team: Named,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Named {
    pub name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Snapshot {
    pub viewer: String,
    pub issues: Vec<Issue>,
}

pub enum Progress {
    Browser(String),
    Status(String),
}

pub struct SyncResult {
    pub snapshot: Snapshot,
    pub closed: Vec<String>,
    pub workspace: Workspace,
}

/// Never format SDK errors: their payloads can contain response bodies and secrets.
fn log_api_error(operation: &'static str, error: &LinearError) {
    let (kind, status) = match error {
        LinearError::Authentication(_) => ("authentication", Some(401)),
        LinearError::Forbidden(_) => ("forbidden", Some(403)),
        LinearError::RateLimited { .. } => ("rate_limited", Some(429)),
        LinearError::HttpError { status, .. } => ("http", Some(*status)),
        LinearError::Network(error) if error.is_timeout() => ("timeout", None),
        LinearError::Network(_) => ("network", None),
        LinearError::GraphQL { .. } => ("graphql", None),
        LinearError::MissingData(_) => ("missing_data", None),
        LinearError::InvalidInput(_) => ("invalid_input", None),
        LinearError::AuthConfig(_) => ("auth_config", None),
        LinearError::Internal(_) => ("internal", None),
    };
    tracing::warn!(
        operation,
        error_kind = kind,
        http_status = status,
        "Linear API request failed"
    );
}

/// Query tracked tickets separately: absence from the assigned list does not mean closed.
#[tracing::instrument(skip_all)]
pub async fn sync(
    config: &Config,
    store: &Store,
    connect: bool,
    tracked: &[String],
    cancel: &AtomicBool,
    progress: impl Fn(Progress),
) -> Result<SyncResult> {
    let started = std::time::Instant::now();
    tracing::debug!(
        connect,
        tracked_count = tracked.len(),
        "Linear sync started"
    );
    store.require_connection(connect).await?;
    let mut oauth_tokens = None;
    let token = if let Some(api_key) = &config.api_key {
        api_key.clone()
    } else {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(15)))
            .max_redirects(0)
            .build()
            .new_agent();
        let mut tokens = if connect {
            oauth::authorize(&agent, config, cancel, &progress)?
        } else {
            store.load_tokens(config).await?
        };
        if tokens.expires_at <= oauth::now() + 60 {
            progress(Progress::Status("Refresh Linear access".into()));
            tokens = oauth::refresh(&agent, config, &tokens)?;
        }
        let token = format!("Bearer {}", tokens.access_token);
        oauth_tokens = Some(tokens);
        token
    };
    progress(Progress::Status("Fetch assigned issues".into()));
    let client = Client::from_token(token)?;
    let workspace = fetch_workspace(&client, cancel).await?;
    store.bind_workspace(&workspace, connect).await?;
    // Do not overwrite credentials when reconnecting to the wrong workspace.
    if let Some(tokens) = oauth_tokens {
        store.save_tokens(config, &tokens).await?;
    }
    let snapshot = fetch(&client, cancel).await?;
    let closed = closed_tickets(&client, tracked, cancel).await?;
    store.save_cache(config, &snapshot).await?;
    tracing::info!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        issue_count = snapshot.issues.len(),
        closed_count = closed.len(),
        "Linear sync finished"
    );
    Ok(SyncResult {
        snapshot,
        closed,
        workspace,
    })
}

/// Inspect an API key without changing repository mappings or fetching tickets.
pub async fn api_key_workspace(config: &Config, cancel: &AtomicBool) -> Result<Workspace> {
    let key = config
        .api_key
        .as_ref()
        .context("Configure linear.api_key to discover its workspace")?;
    let client = Client::from_token(key.clone())?;
    fetch_workspace(&client, cancel).await
}

#[tracing::instrument(skip_all)]
async fn fetch_workspace(client: &Client, cancel: &AtomicBool) -> Result<Workspace> {
    request(cancel, Duration::from_secs(15), async {
        client
            .execute(
                "query MentatWorkspace { organization { id name } }",
                json!({}),
                "organization",
            )
            .await
            .map_err(|error| {
                log_api_error("workspace", &error);
                anyhow::anyhow!("Cannot identify the Linear workspace; check access and reconnect")
            })
    })
    .await
}

async fn closed_tickets(
    client: &Client,
    tracked: &[String],
    cancel: &AtomicBool,
) -> Result<Vec<String>> {
    #[derive(Deserialize)]
    struct TicketState {
        state: State,
    }
    #[derive(Deserialize)]
    struct State {
        r#type: String,
    }
    let mut closed = Vec::new();
    for id in tracked {
        let ticket: TicketState = request(cancel, Duration::from_secs(15), async {
            client
                .execute(
                    "query MentatTicketState($id: String!) { issue(id: $id) { state { type } } }",
                    json!({"id": id}),
                    "issue",
                )
                .await
                .map_err(|error| {
                    log_api_error("ticket_status", &error);
                    anyhow::anyhow!("Cannot verify tracked ticket status")
                })
        })
        .await?;
        if matches!(ticket.state.r#type.as_str(), "completed" | "canceled") {
            closed.push(id.clone());
        }
    }
    Ok(closed)
}

async fn fetch(client: &Client, cancel: &AtomicBool) -> Result<Snapshot> {
    let mut issues = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page: Viewer = request(cancel, Duration::from_secs(15), async {
            client
                .execute(QUERY, json!({"after": cursor}), "viewer")
                .await
                // SDK errors can contain remote response bodies. Keep them out of the UI.
                .map_err(|error| {
                    log_api_error("issues", &error);
                    anyhow::anyhow!("Linear request failed; check access and reconnect if needed")
                })
        })
        .await?;
        issues.extend(page.assigned_issues.nodes);
        if !page.assigned_issues.page_info.has_next_page {
            return Ok(Snapshot {
                viewer: page.name,
                issues,
            });
        }
        let next = page
            .assigned_issues
            .page_info
            .end_cursor
            .context("Linear omitted its page cursor")?;
        ensure!(
            cursor.as_ref() != Some(&next),
            "Linear repeated its page cursor"
        );
        cursor = Some(next);
        ensure!(
            issues.len() < 10_000,
            "Too many assigned issues; narrow the Linear query"
        );
    }
}

const QUERY: &str = r#"query MentatIssues($after: String) {
  viewer {
    name
    assignedIssues(first: 50, after: $after, orderBy: updatedAt,
      filter: {state: {type: {nin: ["completed", "canceled"]}}}) {
      nodes { id identifier title description url state { name } team { name } }
      pageInfo { hasNextPage endCursor }
    }
  }
}"#;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Viewer {
    name: String,
    assigned_issues: Connection,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Connection {
    nodes: Vec<Issue>,
    page_info: PageInfo,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

async fn request<T>(
    cancel: &AtomicBool,
    timeout: Duration,
    operation: impl Future<Output = Result<T>>,
) -> Result<T> {
    ensure!(!cancel.load(Ordering::Relaxed), "Linear request cancelled");
    tokio::select! {
        biased;
        _ = async {
            loop {
                if cancel.load(Ordering::Relaxed) { break; }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        } => bail!("Linear request cancelled"),
        result = tokio::time::timeout(timeout, operation) => {
            if result.is_err() { tracing::warn!(timeout_ms = timeout.as_millis() as u64, "Linear request timed out"); }
            result.context("Linear request timed out")?
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_partial_json, header},
    };

    fn page(id: &str, next: bool) -> serde_json::Value {
        json!({"data": {"viewer": {"name": "A", "assignedIssues": {
            "nodes": [{"id": id, "identifier": id, "title": "Ticket", "description": null,
                "url": "https://linear.app/test", "state": {"name": "Todo"}, "team": {"name": "Team"}}],
            "pageInfo": {"hasNextPage": next, "endCursor": id}
        }}}})
    }

    #[tokio::test]
    async fn reads_authenticated_workspace_identity() {
        let server = MockServer::start().await;
        Mock::given(body_partial_json(
            json!({"query": "query MentatWorkspace { organization { id name } }"}),
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!({"data": {"organization": {"id": "forge-id", "name": "Forge"}}}),
            ),
        )
        .expect(1)
        .mount(&server)
        .await;
        let mut client = Client::from_token("test").unwrap();
        client.set_base_url(server.uri());
        assert_eq!(
            fetch_workspace(&client, &AtomicBool::new(false))
                .await
                .unwrap(),
            Workspace {
                id: "forge-id".into(),
                name: "Forge".into()
            }
        );
    }

    #[tokio::test]
    async fn cleanup_requires_an_explicit_terminal_state() {
        let server = MockServer::start().await;
        let mut client = Client::from_token("test").unwrap();
        client.set_base_url(server.uri());
        for state in ["completed", "canceled", "started", "backlog"] {
            Mock::given(body_partial_json(json!({"variables": {"id": state}})))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"data":{"issue":{"state":{"type":state}}}})),
                )
                .mount(&server)
                .await;
        }
        let tracked = ["completed", "canceled", "started", "backlog"].map(str::to_owned);
        assert_eq!(
            closed_tickets(&client, &tracked, &AtomicBool::new(false))
                .await
                .unwrap(),
            ["completed", "canceled"]
        );
        assert!(
            closed_tickets(&client, &["missing".into()], &AtomicBool::new(false))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn authenticates_and_reads_all_pages() {
        let server = MockServer::start().await;
        for token in ["lin_api_test", "Bearer oauth-test"] {
            Mock::given(header("authorization", token))
                .and(body_partial_json(json!({"variables": {"after": null}})))
                .respond_with(ResponseTemplate::new(200).set_body_json(page("one", true)))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(header("authorization", token))
                .and(body_partial_json(json!({"variables": {"after": "one"}})))
                .respond_with(ResponseTemplate::new(200).set_body_json(page("two", false)))
                .expect(1)
                .mount(&server)
                .await;
            let mut client = Client::from_token(token).unwrap();
            client.set_base_url(server.uri());
            let snapshot = fetch(&client, &AtomicBool::new(false)).await.unwrap();
            assert_eq!(
                snapshot
                    .issues
                    .iter()
                    .map(|i| i.id.as_str())
                    .collect::<Vec<_>>(),
                ["one", "two"]
            );
        }
    }

    #[tokio::test]
    async fn rejects_partial_errors_and_repeated_cursors() {
        let server = MockServer::start().await;
        let mut client = Client::from_token("test").unwrap();
        client.set_base_url(server.uri());
        let mut response = page("one", false);
        response["errors"] = json!([{"message": "secret remote body"}]);
        Mock::given(header("authorization", "test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .mount(&server)
            .await;
        let error = fetch(&client, &AtomicBool::new(false)).await.err().unwrap();
        assert!(!format!("{error:#}").contains("secret remote body"));
        server.reset().await;
        Mock::given(header("authorization", "test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page("one", true)))
            .mount(&server)
            .await;
        assert!(
            fetch(&client, &AtomicBool::new(false))
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("repeated")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_and_cancellation_stop_pending_requests() {
        let cancel = AtomicBool::new(false);
        let error = request::<()>(&cancel, Duration::from_secs(15), std::future::pending())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        let work = request::<()>(&cancel, Duration::from_secs(15), std::future::pending());
        let stop = async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel.store(true, Ordering::Relaxed);
        };
        let (result, _) = tokio::join!(work, stop);
        assert!(result.unwrap_err().to_string().contains("cancelled"));
    }
}
