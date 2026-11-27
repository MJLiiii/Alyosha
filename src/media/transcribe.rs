//! Speech to text: OpenAI's transcription API through GPROXY.
//! See `docs/Media.md#transcription`.

use std::time::Duration;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{BoxFuture, llm_client::error_message};

/// The most sound one transcription may send: OpenAI's limit.
pub const MAX_AUDIO_BYTES: usize = 25 * 1024 * 1024;

/// Turns recorded speech into text.
pub trait Transcriber: Send + Sync {
    /// `audio` is a whole file of type `mime`, such as `audio/ogg`.
    fn transcribe<'a>(
        &'a self,
        audio: Vec<u8>,
        mime: &'a str,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<String, String>>;
}

/// Sends speech to `/<provider>/v1/audio/transcriptions` on the gateway.
pub struct GatewayTranscriber {
    http: reqwest::Client,
    base_url: String,
    gateway_key: String,
    provider: String,
    model: String,
    idle: Duration,
}

impl GatewayTranscriber {
    /// With the same limits as model requests: 10 seconds to connect and 5
    /// minutes for the reply.
    pub fn new(base_url: &str, gateway_key: &str, provider: &str, model: &str) -> Self {
        Self::with_limits(
            base_url,
            gateway_key,
            provider,
            model,
            Duration::from_secs(10),
            Duration::from_secs(5 * 60),
        )
    }

    pub fn with_limits(
        base_url: &str,
        gateway_key: &str,
        provider: &str,
        model: &str,
        connect: Duration,
        idle: Duration,
    ) -> Self {
        Self {
            http: reqwest::Client::builder()
                .connect_timeout(connect)
                .build()
                .expect("the HTTP client settings are valid"),
            base_url: base_url.trim_end_matches('/').to_owned(),
            gateway_key: gateway_key.to_owned(),
            provider: provider.to_owned(),
            model: model.to_owned(),
            idle,
        }
    }

    async fn send(&self, audio: Vec<u8>, mime: &str) -> Result<String, String> {
        if audio.len() > MAX_AUDIO_BYTES {
            return Err(format!(
                "the sound is {} MB, more than the 25 MB OpenAI takes",
                audio.len().div_ceil(1024 * 1024)
            ));
        }
        let extension = match mime {
            "audio/ogg" | "audio/opus" => "ogg",
            "audio/mpeg" => "mp3",
            "audio/wav" | "audio/x-wav" => "wav",
            "audio/mp4" | "audio/m4a" => "m4a",
            "audio/webm" => "webm",
            "audio/flac" => "flac",
            _ => "ogg",
        };
        let part = reqwest::multipart::Part::bytes(audio)
            .file_name(format!("audio.{extension}"))
            .mime_str(mime)
            .map_err(|error| format!("bad MIME type {mime}: {error}"))?;
        let form = reqwest::multipart::Form::new()
            .part("file", part)
            .text("model", self.model.clone());
        let request = self
            .http
            .post(format!(
                "{}/{}/v1/audio/transcriptions",
                self.base_url, self.provider
            ))
            .bearer_auth(&self.gateway_key)
            .multipart(form)
            .send();
        let response = tokio::time::timeout(self.idle, request)
            .await
            .map_err(|_| "no reply within 5 minutes".to_owned())?
            .map_err(|error| {
                if error.is_connect() {
                    format!("could not reach the gateway at {}", self.base_url)
                } else {
                    format!("the request failed: {error}")
                }
            })?;
        let status = response.status();
        let body = tokio::time::timeout(self.idle, response.text())
            .await
            .map_err(|_| "no reply within 5 minutes".to_owned())?
            .map_err(|error| format!("the reply broke off: {error}"))?;
        if !status.is_success() {
            let message = error_message(&body).unwrap_or_default();
            return Err(match status.as_u16() {
                401 | 403 | 404 => format!(
                    "the transcription provider is not signed in; run `alyosha gateway login` ({status} {message})"
                ),
                429 => format!("rate limit ({message})"),
                _ => format!("{status} {message}").trim().to_owned(),
            });
        }
        let json: Value = serde_json::from_str(&body)
            .map_err(|error| format!("the reply is not JSON: {error}"))?;
        json["text"]
            .as_str()
            .map(|text| text.trim().to_owned())
            .ok_or_else(|| "the reply has no text".to_owned())
    }
}

impl Transcriber for GatewayTranscriber {
    fn transcribe<'a>(
        &'a self,
        audio: Vec<u8>,
        mime: &'a str,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            tokio::select! {
                () = cancel.cancelled() => Err("stopped".to_owned()),
                result = self.send(audio, mime) => result,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::*;

    /// Serves one request with `status` and `body`, and returns what it got.
    async fn serve_once(status: u16, body: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut seen = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let read = socket.read(&mut buffer).await.unwrap();
                seen.extend_from_slice(&buffer[..read]);
                let text = String::from_utf8_lossy(&seen);
                if let Some(end) = text.find("\r\n\r\n") {
                    let length = text[..end]
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|value| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if seen.len() >= end + 4 + length {
                        break;
                    }
                }
                if read == 0 {
                    break;
                }
            }
            let reply = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(reply.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&seen).into_owned()
        });
        (url, handle)
    }

    #[tokio::test]
    async fn speech_goes_to_the_provider_route_as_multipart() {
        let (url, server) = serve_once(200, r#"{"text": " Hello there. "}"#).await;
        let transcriber = GatewayTranscriber::new(&url, "gw-key", "openai", "gpt-transcribe");
        let text = transcriber
            .transcribe(b"OggS-sound".to_vec(), "audio/ogg", CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(text, "Hello there.");
        let request = server.await.unwrap();
        assert!(request.starts_with("POST /openai/v1/audio/transcriptions "), "{request}");
        assert!(request.to_lowercase().contains("authorization: bearer gw-key"));
        assert!(request.contains("multipart/form-data"));
        assert!(request.contains("filename=\"audio.ogg\""));
        assert!(request.contains("gpt-transcribe"));
        assert!(request.contains("OggS-sound"));
    }

    #[tokio::test]
    async fn errors_say_what_went_wrong() {
        let (url, _server) = serve_once(429, r#"{"error": {"message": "slow down"}}"#).await;
        let transcriber = GatewayTranscriber::new(&url, "k", "openai", "gpt-transcribe");
        let error = transcriber
            .transcribe(vec![1], "audio/ogg", CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error, "rate limit (slow down)");
        let (url, _server) = serve_once(404, "{}").await;
        let transcriber = GatewayTranscriber::new(&url, "k", "missing", "gpt-transcribe");
        let error = transcriber
            .transcribe(vec![1], "audio/ogg", CancellationToken::new())
            .await
            .unwrap_err();
        assert!(error.contains("alyosha gateway login"), "{error}");
        let too_big = vec![0; MAX_AUDIO_BYTES + 1];
        let error = transcriber
            .transcribe(too_big, "audio/ogg", CancellationToken::new())
            .await
            .unwrap_err();
        assert!(error.contains("25 MB"), "{error}");
    }
}
