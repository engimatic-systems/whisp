use crate::{
    codex,
    config::{Backend, Paths, Settings},
};
use anyhow::{Context, Result, bail};
use reqwest::blocking::{Client, multipart};
use serde::Deserialize;
use std::{fs, path::Path, time::Duration};

const ENDPOINT: &str = "https://api.openai.com/v1/audio/transcriptions";
const MAX_BYTES: u64 = 25_000_000;

const CODEX_ENDPOINT: &str = "https://chatgpt.com/backend-api/transcribe";

#[derive(Debug)]
struct ExpiredLogin;
impl std::fmt::Display for ExpiredLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChatGPT sign-in expired. Run `codex login` and retry.")
    }
}
impl std::error::Error for ExpiredLogin {}

pub fn transcribe(path: &Path, paths: &Paths, settings: &Settings) -> Result<String> {
    match settings.backend {
        Backend::ApiKey => send(ENDPOINT, path, &paths.api_key()?, None, settings),
        Backend::Codex => {
            let auth = codex::authenticate(false)?;
            let result = send(
                CODEX_ENDPOINT,
                path,
                &auth.token,
                auth.account_id.as_deref(),
                settings,
            );
            if result
                .as_ref()
                .err()
                .is_some_and(|e| e.is::<ExpiredLogin>())
            {
                let auth = codex::authenticate(true)?;
                send(
                    CODEX_ENDPOINT,
                    path,
                    &auth.token,
                    auth.account_id.as_deref(),
                    settings,
                )
            } else {
                result
            }
        }
    }
}

fn send(
    endpoint: &str,
    path: &Path,
    key: &str,
    account_id: Option<&str>,
    settings: &Settings,
) -> Result<String> {
    let size = fs::metadata(path)
        .context("Cannot read the saved recording")?
        .len();
    if size < 44 {
        bail!("The recording is empty or incomplete.");
    }
    if size > MAX_BYTES {
        bail!("The recording exceeds the API's 25 MB limit. It is still saved locally.");
    }
    let file = multipart::Part::file(path)?
        .file_name("dictation.wav")
        .mime_str("audio/wav")?;
    let mut form = multipart::Form::new().part("file", file);
    if settings.backend == Backend::ApiKey {
        form = form
            .text("model", settings.model.clone())
            .text("response_format", "json");
        if !settings.prompt.trim().is_empty() {
            form = form.text("prompt", settings.prompt.clone());
        }
    }
    let mut request = Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(180))
        .redirect(reqwest::redirect::Policy::none())
        .build()?
        .post(endpoint)
        .bearer_auth(key)
        .multipart(form);
    if settings.backend == Backend::Codex {
        // Match the desktop's transport: the default HTTP User-Agent is rejected.
        request = request.header("originator", "Codex Desktop").header(
            reqwest::header::USER_AGENT,
            "Codex Desktop/26.901.51231 (X11; Linux; x64)",
        );
        if let Some(account_id) = account_id {
            request = request.header("ChatGPT-Account-Id", account_id);
        }
    }
    let response = request
        .send()
        .context("Could not reach OpenAI. Recording saved; check your connection and retry")?;
    let status = response.status();
    let body = response
        .text()
        .context("Could not read OpenAI's response. Recording saved for retry")?;
    if status == reqwest::StatusCode::UNAUTHORIZED && settings.backend == Backend::Codex {
        return Err(ExpiredLogin.into());
    }
    if !status.is_success() {
        let detail = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v["error"]["message"].as_str().map(str::to_owned))
            .unwrap_or_else(|| "Please try again later.".into())
            .replace(key, "[redacted]");
        let detail: String = detail.chars().take(400).collect();
        bail!("OpenAI {status}: {detail}");
    }
    #[derive(Deserialize)]
    struct Transcript {
        text: String,
    }
    let transcript: Transcript = serde_json::from_str(&body)
        .context("OpenAI returned an unexpected response. Recording saved for retry")?;
    if transcript.text.trim().is_empty() {
        bail!("No speech was recognized. Retry or discard the recording.");
    }
    Ok(transcript.text.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    fn mock(status: &str, response: &str) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/v1/audio/transcriptions",
            listener.local_addr().unwrap()
        );
        let reply = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
            response.len()
        );
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buf = [0; 4096];
            loop {
                let n = stream.read(&mut buf).unwrap();
                assert!(n > 0);
                request.extend_from_slice(&buf[..n]);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]);
                    let len: usize = headers
                        .lines()
                        .find_map(|line| {
                            line.to_lowercase()
                                .strip_prefix("content-length: ")
                                .map(str::to_owned)
                        })
                        .unwrap()
                        .parse()
                        .unwrap();
                    if request.len() >= end + 4 + len {
                        break;
                    }
                }
            }
            stream.write_all(reply.as_bytes()).unwrap();
            String::from_utf8_lossy(&request).into_owned()
        });
        (url, worker)
    }

    #[test]
    fn multipart_upload_preserves_vocabulary_and_unicode_response() {
        let mut audio = tempfile::NamedTempFile::new().unwrap();
        audio.write_all(&[0; 100]).unwrap();
        let (url, server) = mock("200 OK", r#"{"text":" QEMU and བོད་. "}"#);
        let settings = Settings {
            backend: Backend::ApiKey,
            model: "gpt-transcribe".into(),
            prompt: "QEMU, བོད་".into(),
            ..Settings::default()
        };
        assert_eq!(
            send(&url, audio.path(), "test-key", None, &settings).unwrap(),
            "QEMU and བོད་."
        );
        let request = server.join().unwrap();
        assert!(request.contains("authorization: Bearer test-key"));
        assert!(request.contains("name=\"model\"\r\n\r\ngpt-transcribe"));
        assert!(request.contains("name=\"prompt\"\r\n\r\nQEMU, བོད་"));
        assert!(request.contains("filename=\"dictation.wav\""));
        assert!(audio.path().exists());
    }

    #[test]
    fn codex_upload_uses_account_and_omits_api_only_fields() {
        let mut audio = tempfile::NamedTempFile::new().unwrap();
        audio.write_all(&[0; 100]).unwrap();
        let (url, server) = mock("200 OK", r#"{"text":"Front center."}"#);
        let settings = Settings {
            prompt: "private vocabulary".into(),
            ..Settings::default()
        };
        assert_eq!(
            send(
                &url,
                audio.path(),
                "test-token",
                Some("account-123"),
                &settings
            )
            .unwrap(),
            "Front center."
        );
        let request = server.join().unwrap();
        assert!(request.contains("chatgpt-account-id: account-123"));
        assert!(request.contains("user-agent: Codex Desktop/"));
        assert!(!request.contains("name=\"model\""));
        assert!(!request.contains("private vocabulary"));
    }

    #[test]
    fn api_error_keeps_recording_and_redacts_credentials() {
        let mut audio = tempfile::NamedTempFile::new().unwrap();
        audio.write_all(&[0; 100]).unwrap();
        let (url, server) = mock(
            "401 Unauthorized",
            r#"{"error":{"message":"Bad key: test-key"}}"#,
        );
        let error = send(
            &url,
            audio.path(),
            "test-key",
            None,
            &Settings {
                backend: Backend::ApiKey,
                ..Settings::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("401"));
        assert!(!error.contains("test-key"));
        assert!(audio.path().exists());
        server.join().unwrap();
    }

    #[test]
    fn codex_unauthorized_requests_token_refresh() {
        let mut audio = tempfile::NamedTempFile::new().unwrap();
        audio.write_all(&[0; 100]).unwrap();
        let (url, server) = mock("401 Unauthorized", "{}");
        let error = send(&url, audio.path(), "test-token", None, &Settings::default()).unwrap_err();
        assert!(error.is::<ExpiredLogin>());
        assert!(audio.path().exists());
        server.join().unwrap();
    }

    #[test]
    #[ignore = "Contacts ChatGPT using the current Codex login and a system speech sample"]
    fn live_codex_transcription() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config: dir.path().to_owned(),
            state: dir.path().to_owned(),
        };
        let text = transcribe(
            Path::new("/usr/share/sounds/alsa/Front_Center.wav"),
            &paths,
            &Settings::default(),
        )
        .unwrap();
        assert!(
            text.to_lowercase().contains("front"),
            "Expected system sample transcription"
        );
        assert!(
            text.to_lowercase().contains("center"),
            "Expected system sample transcription"
        );
    }

    #[test]
    fn oversize_audio_is_rejected_before_upload() {
        let audio = tempfile::NamedTempFile::new().unwrap();
        audio.as_file().set_len(MAX_BYTES + 1).unwrap();
        let error = send(
            "http://127.0.0.1:1",
            audio.path(),
            "test-key",
            None,
            &Settings::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("25 MB"));
    }
}
