use crate::{Config, Progress, REDIRECT_URI};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use url::Url;

#[derive(Deserialize, Serialize)]
pub(crate) struct Tokens {
    pub client_id: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
    token_type: String,
}

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn random_secret() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| anyhow::anyhow!("Cannot obtain secure randomness"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn authorization_url(client_id: &str, verifier: &str, state: &str) -> Url {
    let mut url = Url::parse("https://linear.app/oauth/authorize").expect("constant URL");
    url.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", client_id),
        ("redirect_uri", REDIRECT_URI),
        ("scope", "read"),
        ("state", state),
        ("actor", "user"),
        ("code_challenge_method", "S256"),
        (
            "code_challenge",
            &URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
        ),
    ]);
    url
}

pub(crate) fn authorize(
    agent: &ureq::Agent,
    config: &Config,
    cancel: &AtomicBool,
    progress: &impl Fn(Progress),
) -> Result<Tokens> {
    let verifier = random_secret()?;
    let state = random_secret()?;
    let listener = TcpListener::bind("127.0.0.1:43821")
        .context("Cannot bind OAuth callback port 43821; close the other login attempt")?;
    listener.set_nonblocking(true)?;
    progress(Progress::Browser(
        authorization_url(&config.client_id, &verifier, &state).into(),
    ));
    let deadline = Instant::now() + Duration::from_secs(180);
    let code = loop {
        ensure!(!cancel.load(Ordering::Relaxed), "Linear sign-in cancelled");
        ensure!(
            Instant::now() < deadline,
            "Linear sign-in timed out; press l to retry"
        );
        let (mut stream, _) = match listener.accept() {
            Ok(pair) => pair,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
                continue;
            }
            Err(e) => return Err(e).context("Cannot accept OAuth callback"),
        };
        stream.set_read_timeout(Some(Duration::from_secs(1)))?;
        stream.set_write_timeout(Some(Duration::from_secs(1)))?;
        let mut request = Vec::new();
        let read =
            BufReader::new(std::io::Read::take(&mut stream, 8192)).read_until(b'\n', &mut request);
        let callback = read
            .ok()
            .and_then(|_| std::str::from_utf8(&request).ok())
            .and_then(|line| parse_callback(line, &state).ok());
        let (status, message) = match &callback {
            Some(Callback::Code(_)) => (
                "200 OK",
                "Mentat received the sign-in response. Return to your terminal.",
            ),
            Some(Callback::Denied) => ("200 OK", "Linear sign-in was cancelled. Return to Mentat."),
            None => (
                "400 Bad Request",
                "Invalid sign-in response. Return to the original Linear sign-in page.",
            ),
        };
        let _ = write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{message}",
            message.len()
        );
        match callback {
            Some(Callback::Code(code)) => break code,
            Some(Callback::Denied) => bail!("Linear sign-in was declined; press l to retry"),
            None => continue,
        }
    };
    drop(listener);
    ensure!(!cancel.load(Ordering::Relaxed), "Linear sign-in cancelled");
    exchange(
        agent,
        &config.client_id,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", REDIRECT_URI),
            ("client_id", &config.client_id),
            ("code_verifier", &verifier),
        ],
    )
}

enum Callback {
    Code(String),
    Denied,
}

fn parse_callback(line: &str, state: &str) -> Result<Callback> {
    let parts: Vec<_> = line.split_whitespace().collect();
    ensure!(
        parts.len() == 3 && parts[0] == "GET" && parts[2].starts_with("HTTP/1."),
        "Invalid callback request"
    );
    ensure!(
        parts[1].starts_with("/oauth/callback?"),
        "Invalid callback path"
    );
    let url = Url::parse(&format!("http://127.0.0.1:43821{}", parts[1]))?;
    let pairs: Vec<_> = url.query_pairs().collect();
    let values = |key: &str| {
        pairs
            .iter()
            .filter(|(name, _)| name == key)
            .map(|(_, value)| value.to_string())
            .collect::<Vec<_>>()
    };
    ensure!(values("state") == [state], "Invalid callback state");
    if !values("error").is_empty() {
        return Ok(Callback::Denied);
    }
    let code = values("code");
    ensure!(
        code.len() == 1 && !code[0].is_empty(),
        "Invalid callback code"
    );
    Ok(Callback::Code(code[0].clone()))
}

pub(crate) fn refresh(agent: &ureq::Agent, config: &Config, tokens: &Tokens) -> Result<Tokens> {
    exchange(
        agent,
        &config.client_id,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", &tokens.refresh_token),
            ("client_id", &config.client_id),
        ],
    )
}

fn exchange(agent: &ureq::Agent, client_id: &str, form: &[(&str, &str)]) -> Result<Tokens> {
    let token: TokenResponse = agent
        .post("https://api.linear.app/oauth/token")
        .send_form(form.iter().copied())
        .context("Linear token exchange failed; press l to reconnect")?
        .body_mut()
        .read_json()
        .context("Linear returned an invalid token response")?;
    ensure!(
        token.token_type.eq_ignore_ascii_case("bearer")
            && !token.access_token.is_empty()
            && !token.refresh_token.is_empty(),
        "Linear returned incomplete credentials"
    );
    Ok(Tokens {
        client_id: client_id.into(),
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at: now()
            .checked_add(token.expires_in)
            .context("Invalid token expiry")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pkce_uses_s256_and_callbacks_require_exact_state() {
        let url = authorization_url(
            "client",
            "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk",
            "expected",
        );
        assert!(
            url.as_str()
                .contains("code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM")
        );
        assert!(url.as_str().contains("scope=read"));
        assert!(
            parse_callback(
                "GET /oauth/callback?code=a&state=wrong HTTP/1.1",
                "expected"
            )
            .is_err()
        );
        assert!(
            parse_callback(
                "GET /oauth/callback?code=a&state=expected&state=expected HTTP/1.1",
                "expected"
            )
            .is_err()
        );
        assert!(matches!(
            parse_callback(
                "GET /oauth/callback?code=a&state=expected HTTP/1.1",
                "expected"
            )
            .unwrap(),
            Callback::Code(_)
        ));
        assert!(matches!(
            parse_callback(
                "GET /oauth/callback?error=access_denied&state=expected HTTP/1.1",
                "expected"
            )
            .unwrap(),
            Callback::Denied
        ));
    }
}
