//! Photos, voice, video, stickers, and files: what a Telegram message
//! carries, downloaded and turned into ACP blocks, and files from
//! `send_file`, uploaded after the reply. See
//! `docs/TelegramBridge.md#attachments` and
//! `docs/TelegramBridge.md#sending-files`.

use std::time::Duration;

use alyosha_bridge::{OutFile, attachments};
use serde_json::{Value, json};

use crate::Api;

/// The Bot API downloads nothing larger.
pub const DOWNLOAD_LIMIT: u64 = 20 * 1024 * 1024;
/// `sendPhoto` takes photos up to this size.
const PHOTO_LIMIT: u64 = 10 * 1024 * 1024;
/// The longest caption Telegram shows.
const CAPTION_LIMIT: usize = 1024;

/// One thing a message carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    /// A file to download with `getFile`.
    File {
        file_id: String,
        size: Option<u64>,
        mime: String,
        name: Option<String>,
        /// What to call it when it has no name: photo, voice, video…
        kind: &'static str,
    },
    /// A line of text, such as a location.
    Line(String),
}

fn file(value: &Value, mime: &str, kind: &'static str) -> Option<Item> {
    Some(Item::File {
        file_id: value["file_id"].as_str()?.to_owned(),
        size: value["file_size"].as_u64(),
        mime: value["mime_type"].as_str().unwrap_or(mime).to_owned(),
        name: value["file_name"].as_str().map(str::to_owned),
        kind,
    })
}

/// What a message carries besides its text or caption, in the order the
/// docs list them. Empty for a plain text message.
pub fn items(message: &Value) -> Vec<Item> {
    let mut items = Vec::new();
    if let Some(sizes) = message["photo"].as_array()
        && let Some(largest) = sizes.last()
    {
        items.extend(file(largest, "image/jpeg", "photo"));
    }
    if message.get("animation").is_some() {
        // Telegram sends a `document` with it too; use only the animation.
        items.extend(file(&message["animation"], "video/mp4", "GIF"));
    } else if let Some(document) = message.get("document") {
        items.extend(file(document, "application/octet-stream", "file"));
    }
    if let Some(voice) = message.get("voice") {
        items.extend(file(voice, "audio/ogg", "voice"));
    }
    if let Some(audio) = message.get("audio") {
        items.extend(file(audio, "audio/mpeg", "audio"));
    }
    if let Some(video) = message.get("video") {
        items.extend(file(video, "video/mp4", "video"));
    }
    if let Some(note) = message.get("video_note") {
        items.extend(file(note, "video/mp4", "video"));
    }
    if let Some(sticker) = message.get("sticker") {
        let emoji = sticker["emoji"].as_str().unwrap_or_default();
        items.push(Item::Line(format!("[sticker {emoji}]").replace(" ]", "]")));
        let moving = sticker["is_animated"] == true || sticker["is_video"] == true;
        if !moving {
            items.extend(file(sticker, "image/webp", "sticker"));
        } else if let Some(thumbnail) = sticker.get("thumbnail").or(sticker.get("thumb")) {
            // Its still thumbnail; Alyosha tells the format from the bytes.
            items.extend(file(thumbnail, "", "sticker"));
        }
    }
    let location = message.get("venue").map_or(&message["location"], |venue| &venue["location"]);
    if let (Some(latitude), Some(longitude)) =
        (location["latitude"].as_f64(), location["longitude"].as_f64())
    {
        let mut line = format!("[location: {latitude}, {longitude}");
        if let Some(venue) = message.get("venue") {
            for field in ["title", "address"] {
                if let Some(text) = venue[field].as_str() {
                    line.push_str(", ");
                    line.push_str(text);
                }
            }
        }
        line.push(']');
        items.push(Item::Line(line));
    }
    if let Some(contact) = message.get("contact") {
        let name = [contact["first_name"].as_str(), contact["last_name"].as_str()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        let phone = contact["phone_number"].as_str().unwrap_or_default();
        items.push(Item::Line(format!("[contact: {name}, {phone}]")));
    }
    if let Some(poll) = message.get("poll") {
        let options: Vec<&str> = poll["options"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|option| option["text"].as_str())
            .collect();
        items.push(Item::Line(format!(
            "[poll: {} — {}]",
            poll["question"].as_str().unwrap_or_default(),
            options.join(" / ")
        )));
    }
    if let Some(dice) = message.get("dice") {
        items.push(Item::Line(format!(
            "[dice {}: {}]",
            dice["emoji"].as_str().unwrap_or("🎲"),
            dice["value"].as_i64().unwrap_or_default()
        )));
    }
    items
}

/// The message's text, or its caption.
pub fn text_of(message: &Value) -> &str {
    message["text"]
        .as_str()
        .or(message["caption"].as_str())
        .unwrap_or_default()
}

impl Api {
    /// Downloads each file and returns the prompt's blocks, in order. A file
    /// over 20 MB, or one that cannot be downloaded, becomes a line.
    pub async fn blocks(&self, items: Vec<Item>) -> Vec<Value> {
        let mut blocks = Vec::new();
        for item in items {
            match item {
                Item::Line(line) => blocks.push(attachments::text(&line)),
                Item::File {
                    file_id,
                    size,
                    mime,
                    name,
                    kind,
                } => {
                    if size.is_some_and(|size| size > DOWNLOAD_LIMIT) {
                        blocks.push(attachments::text(&attachments::too_large(
                            kind,
                            name.as_deref(),
                            size.unwrap_or_default(),
                        )));
                        continue;
                    }
                    match self.download(&file_id).await {
                        Ok(bytes) => blocks.push(attachments::block_for(&mime, name.as_deref(), kind, &bytes)),
                        Err(error) => {
                            tracing::warn!(error = %error, kind, "could not download an attachment");
                            let name = name.map(|name| format!(" {name}")).unwrap_or_default();
                            blocks.push(attachments::text(&format!(
                                "[{kind}{name} could not be downloaded: {error}]"
                            )));
                        }
                    }
                }
            }
        }
        blocks
    }

    /// `getFile`, then the file itself. The URL holds the token, so it is
    /// never logged.
    async fn download(&self, file_id: &str) -> Result<Vec<u8>, String> {
        let file = self.call("getFile", json!({"file_id": file_id})).await?;
        let path = file["file_path"]
            .as_str()
            .ok_or("Telegram gave no file path")?;
        let mut response = self
            .http
            .get(format!("{}/{path}", self.files))
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

    /// Sends a file `send_file` gave a prompt, by its type, falling back to
    /// `sendDocument` and then to a line saying it could not be sent. The
    /// copy is deleted either way.
    pub async fn send_file(&self, chat: i64, file: &OutFile, reply_to: Option<i64>) {
        let bytes = match tokio::fs::read(&file.path).await {
            Ok(bytes) => bytes,
            Err(error) => {
                let _ = self
                    .send(chat, &format!("Could not send {}: {error}", file.name), None)
                    .await;
                return;
            }
        };
        let (method, field) = method_for(&file.mime, bytes.len() as u64);
        let mut result = self
            .upload(method, field, chat, file, &bytes, reply_to)
            .await;
        if result.is_err() && method != "sendDocument" {
            tracing::warn!(method, "Telegram refused the file; sending it as a document");
            result = self
                .upload("sendDocument", "document", chat, file, &bytes, reply_to)
                .await;
        }
        if let Err(error) = result {
            let _ = self
                .send(chat, &format!("Could not send {}: {error}", file.name), None)
                .await;
        }
        file.remove();
    }

    /// One multipart upload, with the retries `call` makes.
    async fn upload(
        &self,
        method: &str,
        field: &str,
        chat: i64,
        file: &OutFile,
        bytes: &[u8],
        reply_to: Option<i64>,
    ) -> Result<Value, String> {
        let mut backoff = Duration::from_secs(1);
        loop {
            let part = reqwest::multipart::Part::bytes(bytes.to_vec())
                .file_name(file.name.clone())
                .mime_str(&file.mime)
                .unwrap_or_else(|_| reqwest::multipart::Part::bytes(bytes.to_vec()).file_name(file.name.clone()));
            let mut form = reqwest::multipart::Form::new()
                .text("chat_id", chat.to_string())
                .part(field.to_owned(), part);
            if let Some(caption) = &file.caption {
                form = form.text("caption", caption.chars().take(CAPTION_LIMIT).collect::<String>());
            }
            if let Some(message) = reply_to {
                form = form.text(
                    "reply_parameters",
                    json!({"message_id": message, "allow_sending_without_reply": true}).to_string(),
                );
            }
            let response = self
                .http
                .post(format!("{}/{method}", self.base))
                .timeout(Duration::from_secs(300))
                .multipart(form)
                .send()
                .await;
            match response {
                Ok(response) => {
                    let status = response.status();
                    let reply: Value = response.json().await.unwrap_or(Value::Null);
                    if reply["ok"] == true {
                        return Ok(reply["result"].clone());
                    }
                    if status.as_u16() == 429 {
                        let wait = reply["parameters"]["retry_after"].as_u64().unwrap_or(5);
                        tokio::time::sleep(Duration::from_secs(wait)).await;
                        continue;
                    }
                    if !status.is_server_error() {
                        return Err(reply["description"]
                            .as_str()
                            .unwrap_or("unknown error")
                            .to_owned());
                    }
                }
                Err(error) => {
                    tracing::warn!(method, error = %error.without_url(), "network error; retrying");
                }
            }
            if backoff > Duration::from_secs(60) {
                return Err("Telegram could not be reached".into());
            }
            tokio::time::sleep(backoff).await;
            backoff *= 2;
        }
    }
}

/// The method and form field for a file, by the docs' table.
pub fn method_for(mime: &str, size: u64) -> (&'static str, &'static str) {
    match mime {
        "image/jpeg" | "image/png" | "image/webp" if size <= PHOTO_LIMIT => ("sendPhoto", "photo"),
        "image/gif" => ("sendAnimation", "animation"),
        "video/mp4" => ("sendVideo", "video"),
        "audio/ogg" => ("sendVoice", "voice"),
        "audio/mpeg" | "audio/mp4" => ("sendAudio", "audio"),
        _ => ("sendDocument", "document"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_give_their_items() {
        let message = json!({
            "photo": [{"file_id": "small"}, {"file_id": "big", "file_size": 900}],
            "caption": "look",
        });
        assert_eq!(
            items(&message),
            vec![Item::File {
                file_id: "big".into(),
                size: Some(900),
                mime: "image/jpeg".into(),
                name: None,
                kind: "photo"
            }]
        );
        let gif = json!({"animation": {"file_id": "a", "mime_type": "video/mp4"}, "document": {"file_id": "a"}});
        assert_eq!(items(&gif).len(), 1);
        let sticker = json!({"sticker": {"file_id": "s", "emoji": "😂", "is_animated": true, "thumbnail": {"file_id": "t"}}});
        let found = items(&sticker);
        assert_eq!(found[0], Item::Line("[sticker 😂]".into()));
        assert!(matches!(&found[1], Item::File { file_id, .. } if file_id == "t"));
        let venue = json!({"venue": {"location": {"latitude": 52.3731, "longitude": 4.8922}, "title": "Dam Square", "address": "Dam"}, "location": {"latitude": 52.3731, "longitude": 4.8922}});
        assert_eq!(items(&venue), vec![Item::Line("[location: 52.3731, 4.8922, Dam Square, Dam]".into())]);
        let poll = json!({"poll": {"question": "Lunch?", "options": [{"text": "Yes"}, {"text": "No"}]}});
        assert_eq!(items(&poll), vec![Item::Line("[poll: Lunch? — Yes / No]".into())]);
        let contact = json!({"contact": {"first_name": "Anna", "last_name": "K", "phone_number": "+31 6"}});
        assert_eq!(items(&contact), vec![Item::Line("[contact: Anna K, +31 6]".into())]);
        assert!(items(&json!({"text": "hi"})).is_empty());
    }

    #[test]
    fn files_go_out_by_type() {
        assert_eq!(method_for("image/png", 100), ("sendPhoto", "photo"));
        assert_eq!(method_for("image/png", 11 * 1024 * 1024).0, "sendDocument");
        assert_eq!(method_for("video/mp4", 1).0, "sendVideo");
        assert_eq!(method_for("audio/ogg", 1).0, "sendVoice");
        assert_eq!(method_for("application/zip", 1).0, "sendDocument");
    }
}
