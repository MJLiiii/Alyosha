//! The `send_file` tool: sends a workspace file to the chat. The file is
//! copied into the session's own folder first, so the bridge sends exactly
//! what was checked. See `docs/Media.md#sending-files-out`.

use std::{
    fs::OpenOptions,
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
};

use serde_json::{Value, json};

use super::{clean_name, mime_for_name};
use crate::{
    environment::{FileAccess, LOGS_PREFIX, SKILLS_MOUNT},
    tools::{RegisteredTool, ToolContext, ToolDefinition, ToolError, ToolOutput, handler},
};

/// The largest file `send_file` sends: Telegram's limit for bots.
pub const MAX_BYTES: u64 = 50 * 1024 * 1024;
/// The longest caption.
pub const MAX_CAPTION: usize = 1024;

/// A file on its way to the chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingFile {
    /// The copy in the session's folder.
    pub path: PathBuf,
    /// The original name.
    pub name: String,
    pub mime: String,
    pub size: u64,
    pub caption: Option<String>,
}

/// Tells the front end about a file to send.
pub type SendFile = Arc<dyn Fn(OutgoingFile) + Send + Sync>;

/// Where a session puts files for the bridge, and how it tells the bridge.
pub struct Outbox {
    pub dir: PathBuf,
    counter: AtomicU64,
    send: Mutex<Option<SendFile>>,
}

impl Outbox {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            counter: AtomicU64::new(0),
            send: Mutex::new(None),
        }
    }

    /// How files reach the front end; the session driver sets it.
    pub fn connect(&self, send: SendFile) {
        *self.send.lock().unwrap_or_else(PoisonError::into_inner) = Some(send);
    }

    /// Removes the folder and what is left in it.
    pub fn remove_all(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub fn send_file_tool() -> RegisteredTool {
    RegisteredTool {
        definition: ToolDefinition {
            name: "send_file".into(),
            description: "Send a file from the workspace to the chat, such as a photo, a chart you drew, a PDF, or a video, with an optional caption. Photos show as photos, MP4 videos play in the chat, Opus .ogg recordings play as voice messages, and anything else arrives as a file. Files go out after your reply, in the order you send them. At most 50 MB.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "caption": {"type": "string", "maxLength": MAX_CAPTION}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            requires_approval: false,
        },
        handler: handler(|arguments: Value, context: ToolContext| async move {
            let path = arguments["path"].as_str().unwrap_or_default();
            let caption = arguments["caption"]
                .as_str()
                .map(str::trim)
                .filter(|caption| !caption.is_empty())
                .map(str::to_owned);
            let outbox = context
                .session
                .outbox
                .as_ref()
                .ok_or_else(|| ToolError::failed("This session cannot send files."))?;
            let file = send(&context, outbox, path, caption)?;
            Ok(ToolOutput::text(
                json!({"status": "queued", "name": file.name, "bytes": file.size}).to_string(),
            ))
        }),
    }
}

fn send(
    context: &ToolContext,
    outbox: &Outbox,
    path: &str,
    caption: Option<String>,
) -> Result<OutgoingFile, ToolError> {
    if Path::new(path).starts_with(SKILLS_MOUNT) || Path::new(path).starts_with(LOGS_PREFIX) {
        return Err(ToolError::failed(format!(
            "{path} is not in the workspace. Only workspace files can be sent."
        )));
    }
    // The usual checks: inside the workspace, no symlink out of it, a
    // regular file.
    let mut file = context
        .environment()?
        .open_file(path, FileAccess::ReadWorkspace, context.scope)?;
    if file.len > MAX_BYTES {
        return Err(ToolError::failed(format!(
            "{path} is {} MB; files up to 50 MB can be sent.",
            file.len.div_ceil(1024 * 1024)
        )));
    }
    let name = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(clean_name)
        .unwrap_or_else(|| "file".into());
    std::fs::create_dir_all(&outbox.dir)
        .map_err(|error| ToolError::failed(format!("Could not prepare the file: {error}")))?;
    let number = outbox.counter.fetch_add(1, Ordering::Relaxed) + 1;
    let copy = outbox.dir.join(format!("{number}-{name}"));
    let copied = (|| -> std::io::Result<u64> {
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&copy)?;
        // Through the checked handle, at most the limit and one byte more.
        let mut buffer = Vec::new();
        (&mut file.file).take(MAX_BYTES + 1).read_to_end(&mut buffer)?;
        out.write_all(&buffer)?;
        out.sync_all()?;
        Ok(buffer.len() as u64)
    })();
    let size = match copied {
        Ok(size) if size <= MAX_BYTES => size,
        Ok(_) => {
            let _ = std::fs::remove_file(&copy);
            return Err(ToolError::failed(format!(
                "{path} grew past 50 MB while it was copied."
            )));
        }
        Err(error) => {
            let _ = std::fs::remove_file(&copy);
            return Err(ToolError::failed(format!("Could not copy {path}: {error}")));
        }
    };
    let outgoing = OutgoingFile {
        path: copy,
        mime: mime_for_name(&name).to_owned(),
        name,
        size,
        caption: caption.map(|caption| caption.chars().take(MAX_CAPTION).collect()),
    };
    let send = outbox
        .send
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    match send {
        Some(send) => send(outgoing.clone()),
        None => {
            let _ = std::fs::remove_file(&outgoing.path);
            return Err(ToolError::failed("This session cannot send files."));
        }
    }
    Ok(outgoing)
}
