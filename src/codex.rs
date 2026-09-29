use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

// Tokens live only in memory. Codex owns credential storage and refresh.
pub struct Auth {
    pub token: String,
    pub account_id: Option<String>,
}

pub fn authenticate(refresh: bool) -> Result<Auth> {
    let mut command = Command::new("codex");
    command.arg("app-server");
    authenticate_with(command, refresh, Duration::from_secs(30))
}

fn authenticate_with(mut command: Command, refresh: bool, timeout: Duration) -> Result<Auth> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("Could not start Codex. Install the Codex CLI and run `codex login` first")?;
    let mut input = child.stdin.take().context("Cannot open Codex input")?;
    let output = child.stdout.take().context("Cannot open Codex output")?;
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let result = (|| -> Result<Auth> {
            let mut output = BufReader::new(output);
            let mut request = |id: u64, method: &str, params: Value| -> Result<Value> {
                if id == 2 {
                    writeln!(input, "{}", json!({"method":"initialized"}))?;
                }
                writeln!(
                    input,
                    "{}",
                    json!({"id":id,"method":method,"params":params})
                )?;
                input.flush()?;
                loop {
                    let mut line = String::new();
                    if output.read_line(&mut line)? == 0 {
                        bail!("Codex app-server exited before returning authentication.");
                    }
                    // Never include protocol contents in errors: they can contain tokens.
                    let message: Value = serde_json::from_str(&line)
                        .map_err(|_| anyhow::anyhow!("Invalid response from Codex app-server"))?;
                    if message["id"].as_u64() == Some(id) {
                        if message.get("error").is_some() {
                            bail!(
                                "Codex authentication request failed. Run `codex login` and retry."
                            );
                        }
                        return Ok(message["result"].clone());
                    }
                }
            };
            request(
                1,
                "initialize",
                json!({"clientInfo":{"name":"whisp","version":env!("CARGO_PKG_VERSION")}}),
            )?;
            // This legacy method is also used by the installed desktop app.
            let result = request(
                2,
                "getAuthStatus",
                json!({"includeToken":true,"refreshToken":refresh}),
            )?;
            parse_auth(&result)
        })();
        let _ = sender.send(result);
    });
    let result = receiver.recv_timeout(timeout);
    let _ = child.kill();
    let _ = child.wait();
    let _ = worker.join();
    result.context("Codex authentication timed out or stopped. Run `codex login` and retry")?
}

fn parse_auth(result: &Value) -> Result<Auth> {
    if !matches!(
        result["authMethod"].as_str(),
        Some("chatgpt" | "chatgptAuthTokens")
    ) {
        bail!("Sign in to ChatGPT with `codex login`, then retry transcription.");
    }
    let token = result["authToken"]
        .as_str()
        .filter(|v| !v.is_empty())
        .context("Codex returned no ChatGPT token. Run `codex login` and retry")?
        .to_owned();
    // Decode only to select the account, not to verify or trust the JWT locally.
    let claims: Value = token
        .split('.')
        .nth(1)
        .and_then(|s| URL_SAFE_NO_PAD.decode(s).ok())
        .and_then(|s| serde_json::from_slice(&s).ok())
        .unwrap_or(Value::Null);
    let account_id = claims["https://api.openai.com/auth"]["chatgpt_account_id"]
        .as_str()
        .map(str::to_owned);
    Ok(Auth { token, account_id })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chatgpt_token_selects_account_but_api_keys_are_rejected() {
        let payload = URL_SAFE_NO_PAD
            .encode(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"account-123"}}"#);
        let auth = parse_auth(
            &json!({"authMethod":"chatgpt","authToken":format!("header.{payload}.signature")}),
        )
        .unwrap();
        assert_eq!(auth.account_id.as_deref(), Some("account-123"));
        let error = parse_auth(&json!({"authMethod":"apikey","authToken":"secret"}))
            .err()
            .unwrap();
        assert!(!error.to_string().contains("secret"));
    }

    #[test]
    fn subprocess_protocol_and_cleanup() {
        let mut command = Command::new("sh");
        command.args(["-c", "read -r init; echo '{\"id\":1,\"result\":{}}'; read -r initialized; read -r auth; echo '{\"id\":2,\"result\":{\"authMethod\":\"chatgpt\",\"authToken\":\"test-token\"}}'; read -r done"]);
        let auth = authenticate_with(command, false, Duration::from_secs(2)).unwrap();
        assert_eq!(auth.token, "test-token");
    }

    #[test]
    fn unresponsive_subprocess_times_out() {
        let mut command = Command::new("sh");
        command.args(["-c", "read -r init; read -r never"]);
        let result = authenticate_with(command, false, Duration::from_millis(100));
        assert!(result.is_err());
    }
}
