use super::*;

#[derive(Clone, Debug, Deserialize)]
pub struct Choice {
    pub id: String,
    pub name: String,
}
#[derive(Clone, Debug, Deserialize)]
pub struct Nodes<T> {
    pub nodes: Vec<T>,
}
#[derive(Clone, Debug, Deserialize)]
pub struct EditTeam {
    pub states: Nodes<Choice>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Comment {
    pub body: String,
    pub created_at: String,
    pub user: Option<Named>,
}
#[derive(Clone, Debug)]
pub struct Details {
    pub issue: Issue,
    pub priority: u8,
    pub state: Choice,
    pub team: EditTeam,
    pub comments: Vec<Comment>,
}
// The detail wire shape shares state/team fields with Issue; decode those separately.
#[derive(Clone, Debug)]
pub enum EditOperation {
    Load,
    Save(serde_json::Value),
    Comment(String),
}
pub struct EditResult {
    pub details: Option<Details>,
    pub submitted: bool,
    pub warning: Option<String>,
}

pub async fn edit(
    config: &Config,
    store: &Store,
    id: &str,
    operation: EditOperation,
    cancel: &AtomicBool,
) -> Result<EditResult> {
    store.require_connection(false).await?;
    let token = if let Some(key) = &config.api_key {
        key.clone()
    } else {
        let mut tokens = store.load_tokens(config).await?;
        if tokens.expires_at <= oauth::now() + 60 {
            let agent = ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(15)))
                .max_redirects(0)
                .build()
                .new_agent();
            tokens = oauth::refresh(&agent, config, &tokens)?;
            store.save_tokens(config, &tokens).await?;
        }
        format!("Bearer {}", tokens.access_token)
    };
    let client = Client::from_token(token)?;
    let workspace = fetch_workspace(&client, cancel).await?;
    store.bind_workspace(&workspace, false).await?;
    let submitted = !matches!(operation, EditOperation::Load);
    submit(&client, id, operation, cancel).await?;
    match load(&client, id, cancel).await {
        Ok(details) => {
            let cache = async {
                if let Some(mut snapshot) = store.cached(config).await? {
                    if let Some(issue) = snapshot.issues.iter_mut().find(|i| i.id == id) {
                        *issue = details.issue.clone();
                    }
                    store.save_cache(config, &snapshot).await?;
                }
                Ok::<(), anyhow::Error>(())
            }
            .await;
            Ok(EditResult {
                details: Some(details),
                submitted,
                warning: cache.err().map(|_| {
                    "Linear data loaded, but the local cache could not be updated.".into()
                }),
            })
        }
        Err(error) if submitted => Ok(EditResult {
            details: None,
            submitted,
            warning: Some(format!(
                "Saved to Linear; refresh failed: {error}. Reopen to reload."
            )),
        }),
        Err(error) => Err(error),
    }
}
async fn submit(
    client: &Client,
    id: &str,
    operation: EditOperation,
    cancel: &AtomicBool,
) -> Result<()> {
    match operation {
        EditOperation::Load => {}
        EditOperation::Save(input) => {
            let value: serde_json::Value = call(client, cancel, "mutation MentatUpdate($id: String!, $input: IssueUpdateInput!) { issueUpdate(id: $id, input: $input) { success } }", json!({"id": id, "input": input}), "issueUpdate").await?;
            ensure!(
                value["success"] == true,
                "Linear did not confirm the update"
            );
        }
        EditOperation::Comment(body) => {
            ensure!(!body.trim().is_empty(), "Write a comment first");
            let value: serde_json::Value = call(client, cancel, "mutation MentatComment($input: CommentCreateInput!) { commentCreate(input: $input) { success } }", json!({"input": {"issueId": id, "body": body}}), "commentCreate").await?;
            ensure!(
                value["success"] == true,
                "Linear did not confirm the comment"
            );
        }
    }
    Ok(())
}
async fn call<T: serde::de::DeserializeOwned>(
    client: &Client,
    cancel: &AtomicBool,
    query: &str,
    vars: serde_json::Value,
    root: &str,
) -> Result<T> {
    request(cancel, Duration::from_secs(15), async {
        client.execute(query, vars, root).await.map_err(|error| {
            log_api_error("ticket_edit", &error);
            anyhow::anyhow!("Linear request failed. Check access; press l to reconnect if write permission is missing. For an uncertain submission, check Linear before retrying.")
        })
    }).await
}
async fn load(client: &Client, id: &str, cancel: &AtomicBool) -> Result<Details> {
    let value: serde_json::Value = call(client, cancel, "query MentatDetails($id: String!) { issue(id: $id) { id identifier title description url branchName priority state { id name } team { name states { nodes { id name } } } } }", json!({"id": id}), "issue").await?;
    let mut details = Details {
        issue: serde_json::from_value(value.clone())?,
        priority: serde_json::from_value(value["priority"].clone())?,
        state: serde_json::from_value(value["state"].clone())?,
        team: serde_json::from_value(value["team"].clone())?,
        comments: vec![],
    };
    let mut after: Option<String> = None;
    loop {
        let page: serde_json::Value = call(client, cancel, "query MentatComments($id: String!, $after: String) { issue(id: $id) { comments(first: 50, after: $after) { nodes { body createdAt user { name } } pageInfo { hasNextPage endCursor } } } }", json!({"id": id, "after": after}), "issue").await?;
        details
            .comments
            .extend(serde_json::from_value::<Vec<Comment>>(
                page["comments"]["nodes"].clone(),
            )?);
        if page["comments"]["pageInfo"]["hasNextPage"] != true {
            break;
        }
        let next = page["comments"]["pageInfo"]["endCursor"]
            .as_str()
            .context("Missing comment cursor")?
            .to_owned();
        ensure!(after.as_ref() != Some(&next), "Repeated comment cursor");
        after = Some(next);
    }
    details
        .comments
        .sort_by(|a, b| a.created_at.cmp(&b.created_at));
    Ok(details)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::body_partial_json};
    #[tokio::test]
    async fn submits_only_supplied_fields_and_comments_once() {
        let server = MockServer::start().await;
        for (operation, variables, root) in [
            (
                EditOperation::Save(json!({"title":"Changed", "stateId":"done", "priority":1})),
                json!({"id":"one","input":{"title":"Changed","stateId":"done","priority":1}}),
                "issueUpdate",
            ),
            (
                EditOperation::Comment("First\nSecond".into()),
                json!({"input":{"issueId":"one","body":"First\nSecond"}}),
                "commentCreate",
            ),
        ] {
            Mock::given(body_partial_json(json!({"variables":variables})))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"data":{root:{"success":true}}})),
                )
                .expect(1)
                .mount(&server)
                .await;
            let mut client = Client::from_token("test").unwrap();
            client.set_base_url(server.uri());
            submit(&client, "one", operation, &AtomicBool::new(false))
                .await
                .unwrap();
        }
    }
    #[tokio::test]
    async fn loads_details_and_paginated_comments() {
        let server = MockServer::start().await;
        Mock::given(body_partial_json(json!({"variables":{"id":"one"}})))
            .and(wiremock::matchers::body_string_contains("MentatDetails"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":{"issue":{"id":"one","identifier":"A-1","title":"Test","description":null,"url":"https://linear.app/test","priority":2,"state":{"id":"todo","name":"Todo"},"team":{"name":"A","states":{"nodes":[{"id":"todo","name":"Todo"}]}}}}})))
            .mount(&server).await;
        for (after, next) in [(None, true), (Some("next"), false)] {
            Mock::given(body_partial_json(json!({"variables":{"after":after}})))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":{"issue":{"comments":{"nodes":[{"body":"Comment","createdAt":"2026-01-01","user":null}],"pageInfo":{"hasNextPage":next,"endCursor":"next"}}}}})))
                .expect(1).mount(&server).await;
        }
        let mut client = Client::from_token("test").unwrap();
        client.set_base_url(server.uri());
        let details = load(&client, "one", &AtomicBool::new(false)).await.unwrap();
        assert_eq!(details.comments.len(), 2);
        assert_eq!(details.priority, 2);
        assert_eq!(details.state.id, "todo");
    }
    #[tokio::test]
    async fn mutation_errors_are_sanitized_and_never_retried() {
        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"errors":[{"message":"secret"}]})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let mut client = Client::from_token("test").unwrap();
        client.set_base_url(server.uri());
        let result = call::<serde_json::Value>(
            &client,
            &AtomicBool::new(false),
            "mutation Test { commentCreate { success } }",
            json!({}),
            "commentCreate",
        )
        .await
        .unwrap_err();
        assert!(!result.to_string().contains("secret"));
        assert!(result.to_string().contains("before retrying"));
    }
}
