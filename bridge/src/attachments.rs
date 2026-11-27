//! The ACP blocks a bridge sends for what came with a message, and the text
//! lines for what is not a file. Bridges never decode or convert media;
//! `alyosha acp` does. See `docs/Media.md#from-the-bridge-to-alyosha-acp`.

use base64::Engine;
use serde_json::{Value, json};

/// The most attachments one message may carry.
pub const MAX_ATTACHMENTS: usize = 10;
/// The most bytes a bridge downloads for one attachment.
pub const MAX_BYTES: u64 = 25 * 1024 * 1024;

fn encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// `{"type": "image", ...}`, with the file name in `_meta` when there is one.
pub fn image(mime: &str, name: Option<&str>, bytes: &[u8]) -> Value {
    with_name(
        json!({"type": "image", "mimeType": mime, "data": encode(bytes)}),
        name,
    )
}

/// `{"type": "audio", ...}`, with the file name in `_meta` when there is one.
pub fn audio(mime: &str, name: Option<&str>, bytes: &[u8]) -> Value {
    with_name(
        json!({"type": "audio", "mimeType": mime, "data": encode(bytes)}),
        name,
    )
}

/// `{"type": "resource", ...}` for a video, GIF, or any other file.
pub fn resource(mime: &str, name: &str, bytes: &[u8]) -> Value {
    let name: String = name.chars().filter(|c| *c != '/' && !c.is_control()).collect();
    json!({
        "type": "resource",
        "resource": {"uri": format!("attachment:///{name}"), "mimeType": mime, "blob": encode(bytes)},
    })
}

fn with_name(mut block: Value, name: Option<&str>) -> Value {
    if let Some(name) = name {
        block["_meta"] = json!({"alyosha": {"name": name}});
    }
    block
}

/// A `text` block.
pub fn text(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

/// `[<kind> <name> too large to download: <size> MB]`.
pub fn too_large(kind: &str, name: Option<&str>, bytes: u64) -> String {
    let name = name.map(|name| format!(" {name}")).unwrap_or_default();
    format!(
        "[{kind}{name} too large to download: {} MB]",
        bytes.div_ceil(1024 * 1024)
    )
}

/// `[<n> more attachments left out]`.
pub fn left_out(count: usize) -> String {
    format!("[{count} more attachments left out]")
}

/// Whether a block is an attachment, as opposed to text.
pub fn is_attachment(block: &Value) -> bool {
    matches!(block["type"].as_str(), Some("image" | "audio" | "resource"))
}

/// Keeps at most `MAX_ATTACHMENTS` attachment blocks, replacing the rest
/// with one line saying how many were left out.
pub fn limit(blocks: Vec<Value>) -> Vec<Value> {
    let mut kept = Vec::new();
    let mut attachments = 0;
    let mut dropped = 0;
    for block in blocks {
        if is_attachment(&block) {
            if attachments == MAX_ATTACHMENTS {
                dropped += 1;
                continue;
            }
            attachments += 1;
        }
        kept.push(block);
    }
    if dropped > 0 {
        kept.push(text(&left_out(dropped)));
    }
    kept
}

/// The block for a downloaded file, by its MIME type: images and sound as
/// their own blocks, anything else as a resource. `kind` names it when it
/// has no file name.
pub fn block_for(mime: &str, name: Option<&str>, kind: &str, bytes: &[u8]) -> Value {
    let mime = mime.trim().to_ascii_lowercase();
    if matches!(
        mime.as_str(),
        "image/jpeg" | "image/png" | "image/webp" | "image/gif"
    ) {
        image(&mime, name, bytes)
    } else if mime.starts_with("audio/") {
        audio(&mime, name, bytes)
    } else {
        let fallback = match mime.as_str() {
            "video/mp4" => format!("{kind}.mp4"),
            "video/webm" => format!("{kind}.webm"),
            _ => kind.to_owned(),
        };
        resource(
            if mime.is_empty() { "application/octet-stream" } else { &mime },
            name.unwrap_or(&fallback),
            bytes,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_follow_the_type() {
        assert_eq!(block_for("image/jpeg", None, "photo", b"x")["type"], "image");
        let voice = block_for("audio/ogg", None, "voice", b"x");
        assert_eq!(voice["type"], "audio");
        assert!(voice.get("_meta").is_none());
        let video = block_for("video/mp4", None, "video", b"x");
        assert_eq!(video["resource"]["uri"], "attachment:///video.mp4");
        let pdf = block_for("application/pdf", Some("a/b.pdf"), "file", b"%PDF");
        assert_eq!(pdf["resource"]["uri"], "attachment:///ab.pdf");
        assert_eq!(pdf["resource"]["blob"], "JVBERg==");
        let named = audio("audio/mpeg", Some("song.mp3"), b"x");
        assert_eq!(named["_meta"]["alyosha"]["name"], "song.mp3");
    }

    #[test]
    fn at_most_ten_attachments_are_kept() {
        let mut blocks = vec![text("hi")];
        blocks.extend((0..12).map(|_| image("image/png", None, b"x")));
        let kept = limit(blocks);
        assert_eq!(kept.iter().filter(|block| is_attachment(block)).count(), 10);
        assert_eq!(kept.last().unwrap()["text"], "[2 more attachments left out]");
        assert_eq!(too_large("file", Some("big.zip"), 30 * 1024 * 1024), "[file big.zip too large to download: 30 MB]");
    }
}
