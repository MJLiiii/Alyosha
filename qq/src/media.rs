//! Photos, voice, video, and files: QQ's attachments, downloaded and turned
//! into ACP blocks, and files from `send_file`, uploaded and sent as rich
//! media after the reply. See `docs/QQBridge.md#attachments` and
//! `docs/QQBridge.md#sending-files`.

use alyosha_bridge::{OutFile, attachments};
use base64::Engine;
use serde_json::{Value, json};

use crate::{ReplyTo, SendError, Sender, gateway::QqApi};

/// `file_data` takes at most this much.
pub const UPLOAD_LIMIT: u64 = 5 * 1024 * 1024;
/// QQ's code for a file type it does not take.
const UNSUPPORTED_FORMAT: i64 = 850019;
/// Codes that mean the passive window for a message is closed.
const PASSIVE_CLOSED: [i64; 3] = [40034128, 40034005, 304103];

/// Where an upload goes: a private chat or a group.
#[derive(Debug, Clone, Copy)]
pub enum Chat<'a> {
    User(&'a str),
    Group(&'a str),
}

impl Chat<'_> {
    fn files_path(&self) -> String {
        match self {
            Chat::User(openid) => format!("/v2/users/{openid}/files"),
            Chat::Group(group) => format!("/v2/groups/{group}/files"),
        }
    }
}

/// QQ's `file_type` for a MIME type: 1 image, 2 video, 3 voice, 4 file.
pub fn file_type(mime: &str) -> u8 {
    match mime {
        "image/png" | "image/jpeg" => 1,
        "video/mp4" => 2,
        "audio/ogg" | "audio/mpeg" | "audio/wav" | "audio/x-wav" | "audio/silk" => 3,
        _ => 4,
    }
}

impl QqApi {
    /// Downloads each attachment and returns the prompt's blocks, in order.
    pub async fn blocks(&self, entries: &[Value]) -> Vec<Value> {
        let mut blocks = Vec::new();
        for entry in entries {
            let content_type = entry["content_type"].as_str().unwrap_or_default();
            let name = entry["filename"].as_str().filter(|name| !name.is_empty());
            let (url, mime, kind) = if content_type == "voice" {
                match entry["voice_wav_url"].as_str().filter(|url| !url.is_empty()) {
                    Some(url) => (url, "audio/wav", "voice"),
                    None => {
                        let line = match entry["asr_refer_text"].as_str().filter(|text| !text.trim().is_empty()) {
                            Some(text) => format!("[voice: {}]", text.trim()),
                            None => "[voice message: QQ sent it in a format Alyosha cannot read]".to_owned(),
                        };
                        blocks.push(attachments::text(&line));
                        continue;
                    }
                }
            } else {
                let kind = if content_type.starts_with("image/") {
                    "photo"
                } else if content_type.starts_with("video/") {
                    "video"
                } else {
                    "file"
                };
                let mime = if content_type == "file" { "" } else { content_type };
                (entry["url"].as_str().unwrap_or_default(), mime, kind)
            };
            if entry["size"].as_u64().is_some_and(|size| size > attachments::MAX_BYTES) {
                blocks.push(attachments::text(&attachments::too_large(
                    kind,
                    name,
                    entry["size"].as_u64().unwrap_or_default(),
                )));
                continue;
            }
            // A voice message's WAV copy has no name; keep it unnamed, so
            // Alyosha calls it a voice message.
            let name = if kind == "voice" { None } else { name };
            match self.download(url).await {
                Ok(bytes) => blocks.push(attachments::block_for(mime, name, kind, &bytes)),
                Err(error) => {
                    tracing::warn!(error = %error, kind, "could not download an attachment");
                    let shown = name.map(|name| format!(" {name}")).unwrap_or_default();
                    blocks.push(attachments::text(&format!(
                        "[{kind}{shown} could not be downloaded: {error}]"
                    )));
                }
            }
        }
        blocks
    }

    async fn download(&self, url: &str) -> Result<Vec<u8>, String> {
        let url = if url.starts_with("//") {
            format!("https:{url}")
        } else {
            url.to_owned()
        };
        if !url.starts_with("https://") && !url.starts_with("http://") {
            return Err("QQ gave no address".into());
        }
        let mut response = self
            .http()
            .get(&url)
            .send()
            .await
            .map_err(|error| format!("the download failed: {}", error.without_url()))?;
        if !response.status().is_success() {
            return Err(format!("the download failed: {}", response.status()));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| format!("the download broke off: {}", error.without_url()))?
        {
            bytes.extend_from_slice(&chunk);
            if bytes.len() as u64 > attachments::MAX_BYTES {
                return Err("the file is larger than 25 MB".into());
            }
        }
        Ok(bytes)
    }

    /// Uploads a file for a chat and returns its `file_info`. A voice file
    /// QQ refuses goes again as a plain file.
    pub async fn upload(&self, chat: Chat<'_>, file: &OutFile, bytes: &[u8]) -> Result<String, SendError> {
        if bytes.len() as u64 > UPLOAD_LIMIT {
            return Err(SendError {
                code: -1,
                message: "QQ takes files up to 5 MB from Alyosha".into(),
            });
        }
        let data = base64::engine::general_purpose::STANDARD.encode(bytes);
        let kind = file_type(&file.mime);
        let body = |kind: u8| json!({"file_type": kind, "file_data": data, "file_name": file.name, "srv_send_msg": false});
        let path = chat.files_path();
        let reply = match self.post(&path, body(kind)).await {
            Err(error) if kind == 3 && error.code == UNSUPPORTED_FORMAT => {
                self.post(&path, body(4)).await?
            }
            other => other?,
        };
        reply["file_info"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| SendError {
                code: -1,
                message: "QQ gave no file_info".into(),
            })
    }
}

/// Sends an uploaded file as rich media: a passive reply while one is left,
/// otherwise an active message.
pub async fn deliver_media(
    sender: &dyn Sender,
    file_info: &str,
    caption: Option<&str>,
    reply: &mut ReplyTo,
) -> Result<(), SendError> {
    let mut body = json!({"msg_type": 7, "media": {"file_info": file_info}});
    if let Some(caption) = caption {
        body["content"] = json!(caption);
    }
    if let Some(msg_id) = reply.msg_id.clone().filter(|_| !reply.passive_closed) {
        let mut passive = body.clone();
        passive["msg_id"] = json!(msg_id);
        passive["msg_seq"] = json!(reply.seq);
        reply.seq += 1;
        match sender.send(passive).await {
            Ok(()) => return Ok(()),
            Err(error) if PASSIVE_CLOSED.contains(&error.code) => reply.passive_closed = true,
            Err(error) => return Err(error),
        }
    }
    sender.send(body).await
}

/// Uploads and sends each file after a reply. A file that cannot be sent
/// becomes a line saying so, which is returned to be delivered like text.
/// Every copy is deleted.
pub async fn send_files(
    api: &QqApi,
    sender: &dyn Sender,
    chat: Chat<'_>,
    files: &[OutFile],
    reply: &mut ReplyTo,
) -> Vec<String> {
    let mut problems = Vec::new();
    for file in files {
        let result = async {
            let bytes = tokio::fs::read(&file.path).await.map_err(|error| SendError {
                code: -1,
                message: error.to_string(),
            })?;
            let info = api.upload(chat, file, &bytes).await?;
            deliver_media(sender, &info, file.caption.as_deref(), reply).await
        }
        .await;
        if let Err(error) = result {
            tracing::warn!(code = error.code, "a file could not be sent");
            problems.push(format!("Could not send {}: {}", file.name, error.message));
        }
        file.remove();
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    /// Records each body and answers from a script.
    struct Script {
        sent: Mutex<Vec<Value>>,
        results: Mutex<Vec<Result<(), i64>>>,
    }

    impl Sender for Script {
        fn send(&self, body: Value) -> futures_util::future::BoxFuture<'_, Result<(), SendError>> {
            self.sent.lock().unwrap().push(body);
            let result = self.results.lock().unwrap().remove(0);
            Box::pin(async move {
                result.map_err(|code| SendError {
                    code,
                    message: "no".into(),
                })
            })
        }
    }

    #[tokio::test]
    async fn rich_media_is_a_passive_reply_then_active() {
        let script = Script {
            sent: Mutex::default(),
            results: Mutex::new(vec![Ok(()), Err(40034128), Ok(())]),
        };
        let mut reply = ReplyTo::message("m1");
        reply.seq = 3;
        deliver_media(&script, "INFO", Some("Sales"), &mut reply).await.unwrap();
        deliver_media(&script, "INFO", None, &mut reply).await.unwrap();
        let sent = script.sent.lock().unwrap();
        assert_eq!(
            sent[0],
            json!({"msg_type": 7, "media": {"file_info": "INFO"}, "content": "Sales", "msg_id": "m1", "msg_seq": 3})
        );
        assert_eq!(sent[1]["msg_seq"], 4);
        // The window closed: the same message goes as an active one.
        assert_eq!(sent[2], json!({"msg_type": 7, "media": {"file_info": "INFO"}}));
        assert!(reply.passive_closed);
    }

    #[tokio::test]
    async fn voice_without_a_wav_copy_uses_qq_transcript_and_large_files_wait() {
        let api = QqApi::new("http://127.0.0.1:9", "http://127.0.0.1:9", "1", "s");
        let blocks = api
            .blocks(&[
                json!({"content_type": "voice", "url": "x.silk", "asr_refer_text": " 明天三点 "}),
                json!({"content_type": "voice", "url": "x.silk"}),
                json!({"content_type": "file", "filename": "big.zip", "size": 30 * 1024 * 1024, "url": "https://x"}),
            ])
            .await;
        assert_eq!(
            blocks,
            vec![
                json!({"type": "text", "text": "[voice: 明天三点]"}),
                json!({"type": "text", "text": "[voice message: QQ sent it in a format Alyosha cannot read]"}),
                json!({"type": "text", "text": "[file big.zip too large to download: 30 MB]"}),
            ]
        );
        let file = OutFile {
            path: "/nonexistent".into(),
            name: "big.bin".into(),
            mime: "application/octet-stream".into(),
            size: 0,
            caption: None,
        };
        let error = api
            .upload(Chat::User("u"), &file, &vec![0; 6 * 1024 * 1024])
            .await
            .unwrap_err();
        assert!(error.message.contains("5 MB"));
    }

    #[test]
    fn file_types_follow_qq() {
        assert_eq!(file_type("image/png"), 1);
        assert_eq!(file_type("image/gif"), 4);
        assert_eq!(file_type("video/mp4"), 2);
        assert_eq!(file_type("audio/ogg"), 3);
        assert_eq!(file_type("application/pdf"), 4);
    }
}
