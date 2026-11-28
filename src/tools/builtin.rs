//! The built-in file and shell tools: `read_file`, `write_file`, `edit_file`,
//! and `run_command`. See `docs/reference/BuiltInTools.md`.

use std::{
    io::{Read, Seek, SeekFrom},
    path::Path,
    time::Duration,
};

use serde_json::{Value, json};

use super::{
    OutputKind, RegisteredTool, Scope, ToolContext, ToolDefinition, ToolError, ToolOutput, handler,
};
use crate::{
    environment::{
        FileAccess, LogCapture, ReplaceMode,
        shell::{ShellLaunch, run_shell},
    },
    jobs::{self, JobOrigin},
};

/// The per-result byte limit, which also caps `max_bytes`.
pub const READ_LIMIT: usize = 8 * 1024;

pub fn read_file_tool() -> RegisteredTool {
    RegisteredTool {
        definition: ToolDefinition {
            name: "read_file".into(),
            description: format!(
                "Read a chunk of UTF-8 text from a file: a workspace file (relative to the working directory, or under /workspace), a session log_path, or a skill file under /skills. A PNG, JPEG, GIF, or WebP file returns the image itself for you to look at. Returns at most max_bytes (default and maximum {READ_LIMIT}) from the byte offset (default 0). When more text remains, the result ends with next_offset; pass it as offset to continue. List folders with run_command."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "offset": {"type": "integer", "minimum": 0},
                    "max_bytes": {"type": "integer", "minimum": 1}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            requires_approval: false,
        },
        handler: handler(|arguments: Value, context: ToolContext| async move {
            let path = arguments["path"].as_str().unwrap_or_default();
            let offset = arguments["offset"].as_u64().unwrap_or(0);
            let max_bytes = arguments["max_bytes"]
                .as_u64()
                .map_or(READ_LIMIT, |max| (max as usize).min(READ_LIMIT));
            let mut file =
                context
                    .environment()?
                    .open_file(path, FileAccess::Read, context.scope)?;
            if let Some(mime) = image_mime(&mut file.file, path)? {
                return read_image(file.file, file.len, path, mime, context.scope);
            }
            read_chunk(file.file, file.len, path, offset, max_bytes)
        }),
    }
}

/// The MIME type of an image file Alyosha decodes, from its first bytes.
/// Leaves the file at its start.
fn image_mime(file: &mut std::fs::File, path: &str) -> Result<Option<&'static str>, ToolError> {
    let mut head = [0u8; 16];
    let read = file
        .read(&mut head)
        .map_err(|error| ToolError::failed(format!("{path}: {error}")))?;
    file.seek(SeekFrom::Start(0))
        .map_err(|error| ToolError::failed(format!("{path}: {error}")))?;
    Ok(
        crate::media::image::format(&head[..read]).map(|format| match format {
            image::ImageFormat::Png => "image/png",
            image::ImageFormat::Gif => "image/gif",
            image::ImageFormat::WebP => "image/webp",
            _ => "image/jpeg",
        }),
    )
}

/// An image file as a line describing it and the image for the model.
/// See `docs/Media.md#images-in-tool-results`.
fn read_image(
    mut file: std::fs::File,
    len: u64,
    path: &str,
    mime: &'static str,
    scope: Scope,
) -> Result<ToolOutput, ToolError> {
    if scope == Scope::CodeMode {
        return Err(ToolError::failed(format!(
            "{path} is an image. Code Mode reads text only."
        )));
    }
    if len > crate::media::MAX_IMAGE_FILE {
        return Err(ToolError::failed(format!("{path} is an image over 20 MB.")));
    }
    let mut bytes = Vec::with_capacity(len as usize);
    file.read_to_end(&mut bytes)
        .map_err(|error| ToolError::failed(format!("{path}: {error}")))?;
    let size = image::ImageReader::new(std::io::Cursor::new(&bytes))
        .with_guessed_format()
        .ok()
        .and_then(|reader| reader.into_dimensions().ok())
        .map(|(width, height)| format!(", {width}×{height}"))
        .unwrap_or_default();
    Ok(ToolOutput {
        text: format!(
            "{path}: {mime}{size}, {}",
            crate::media::size_text(bytes.len())
        ),
        notice: None,
        kind: OutputKind::Plain,
        images: vec![crate::media::RawImage {
            mime: mime.to_owned(),
            bytes,
        }],
    })
}

/// Reads whole characters starting at `offset`.
pub fn read_chunk(
    mut file: std::fs::File,
    len: u64,
    path: &str,
    offset: u64,
    max_bytes: usize,
) -> Result<ToolOutput, ToolError> {
    if offset > len {
        return Err(ToolError::failed(format!(
            "{path} has only {len} bytes; offset {offset} is past the end."
        )));
    }
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| ToolError::failed(format!("{path}: {error}")))?;
    let mut buffer = vec![0u8; max_bytes.min((len - offset) as usize)];
    file.read_exact(&mut buffer)
        .map_err(|error| ToolError::failed(format!("{path}: {error}")))?;
    if buffer
        .first()
        .is_some_and(|byte| byte & 0b1100_0000 == 0b1000_0000)
    {
        return Err(ToolError::failed(format!(
            "Offset {offset} is inside a character of {path}. Use the next_offset a read returned."
        )));
    }
    let text = match std::str::from_utf8(&buffer) {
        Ok(text) => text.to_owned(),
        Err(error) if error.error_len().is_none() => {
            // The chunk ends inside a character: return only whole ones.
            std::str::from_utf8(&buffer[..error.valid_up_to()])
                .expect("valid up to here")
                .to_owned()
        }
        Err(_) => return Err(ToolError::failed(format!("{path} is not UTF-8 text."))),
    };
    let more = offset + (text.len() as u64) < len;
    Ok(ToolOutput {
        text,
        notice: None,
        kind: OutputKind::FileChunk { offset, more },
        images: Vec::new(),
    })
}

pub fn write_file_tool() -> RegisteredTool {
    RegisteredTool {
        definition: ToolDefinition {
            name: "write_file".into(),
            description: "Create a workspace file or replace its entire contents. The folder must already exist. The file changes all at once when the write completes.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}, "content": {"type": "string"}},
                "required": ["path", "content"],
                "additionalProperties": false
            }),
            requires_approval: false,
        },
        handler: handler(|arguments: Value, context: ToolContext| async move {
            direct_only(&context, "write_file")?;
            let path = arguments["path"].as_str().unwrap_or_default();
            let content = arguments["content"].as_str().unwrap_or_default();
            context
                .environment()?
                .replace_file(path, content.as_bytes(), ReplaceMode::CreateOrReplace)?;
            Ok(ToolOutput::text(format!("bytes_written: {}", content.len())))
        }),
    }
}

pub fn edit_file_tool() -> RegisteredTool {
    RegisteredTool {
        definition: ToolDefinition {
            name: "edit_file".into(),
            description: "Replace one exact occurrence of old_text with new_text in a workspace file. old_text must occur exactly once; include enough surrounding text to make it unique. An empty new_text deletes the match.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_text": {"type": "string", "minLength": 1},
                    "new_text": {"type": "string"}
                },
                "required": ["path", "old_text", "new_text"],
                "additionalProperties": false
            }),
            requires_approval: false,
        },
        handler: handler(|arguments: Value, context: ToolContext| async move {
            direct_only(&context, "edit_file")?;
            let path = arguments["path"].as_str().unwrap_or_default();
            let old_text = arguments["old_text"].as_str().unwrap_or_default();
            let new_text = arguments["new_text"].as_str().unwrap_or_default();
            let environment = context.environment()?;
            let snapshot = environment
                .open_file(path, FileAccess::ReadWorkspace, context.scope)?
                .snapshot()?;
            let text = std::str::from_utf8(&snapshot.content)
                .map_err(|_| ToolError::failed(format!("{path} is not UTF-8 text.")))?;
            match text.matches(old_text).count() {
                0 => return Err(ToolError::failed(format!("old_text does not occur in {path}."))),
                1 => {}
                count => {
                    return Err(ToolError::failed(format!(
                        "old_text occurs {count} times in {path}. Include more surrounding text so it occurs once."
                    )));
                }
            }
            let edited = text.replacen(old_text, new_text, 1);
            environment.replace_file(path, edited.as_bytes(), ReplaceMode::ReplaceExisting(&snapshot))?;
            Ok(ToolOutput::text(format!("Edited {path}.")))
        }),
    }
}

fn direct_only(context: &ToolContext, tool: &str) -> Result<(), ToolError> {
    if context.scope == Scope::Direct {
        Ok(())
    } else {
        Err(ToolError::failed(format!("{tool} is not available here.")))
    }
}

pub fn run_command_tool() -> RegisteredTool {
    RegisteredTool {
        definition: ToolDefinition {
            name: "run_command".into(),
            description: "Run a Bash command in a sandbox of the workspace: directory listings, search, Git, builds, and test runners. It starts in the working directory, with the project's dev shell tools when the workspace has a flake.nix, and has internet access. timeout_seconds defaults to 120 and may be up to 1800; raise it for long builds and test suites. Long output is shortened and its full text kept at log_path for read_file. With background: true the command runs as a background job (default limit 4 hours, up to 86400 seconds) and the call returns its job_id at once; when the job ends, its result arrives as a new message.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "minLength": 1},
                    "timeout_seconds": {"type": "integer", "minimum": 1, "maximum": 86400},
                    "background": {"type": "boolean"}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
            requires_approval: false,
        },
        handler: handler(|arguments: Value, context: ToolContext| async move {
            direct_only(&context, "run_command")?;
            let command = arguments["command"].as_str().unwrap_or_default();
            let background = arguments["background"].as_bool().unwrap_or(false);
            let timeout = arguments["timeout_seconds"].as_u64();
            if background {
                let jobs = context
                    .session
                    .jobs
                    .as_ref()
                    .ok_or_else(|| ToolError::failed("This session cannot run background jobs."))?;
                let timeout = timeout.map_or(jobs::DEFAULT_TIMEOUT, Duration::from_secs);
                let info = jobs.start(command, timeout, JobOrigin::Model)?;
                return Ok(ToolOutput::text(format!(
                    "status: Started\njob_id: {}\nlog_path: {}",
                    info.id, info.log_path
                ))
                .with_notice(format!("Job {} started: {}", info.id, jobs::short_command(command))));
            }
            let timeout = timeout.unwrap_or(120);
            if timeout > 1800 {
                return Err(ToolError::failed(
                    "timeout_seconds may be at most 1800 without background: true.",
                ));
            }
            let environment = context.environment()?;
            let launch = ShellLaunch {
                config: &environment.sandbox,
                workspace: &environment.workspace,
                cwd: Path::new(&environment.cwd),
                skills_root: environment.skills_root.as_deref(),
            };
            let output = run_shell(&launch, command, Duration::from_secs(timeout), &context.cancel).await?;
            let text = String::from_utf8_lossy(&output.output).into_owned();
            let capture = LogCapture::new(environment.logs.clone(), text.len() as u64, "cmd");
            Ok(ToolOutput {
                text,
                notice: None,
                kind: OutputKind::Logged {
                    status: Some(output.status.to_string()),
                    capture: Some(capture),
                },
                images: Vec::new(),
            })
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn chunk(text: &[u8], offset: u64, max: usize) -> Result<ToolOutput, ToolError> {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(text).unwrap();
        read_chunk(file, text.len() as u64, "t", offset, max)
    }

    #[test]
    fn reads_return_whole_characters() {
        // `café!` is six bytes; four bytes from the start end inside `é`.
        let output = chunk("café!".as_bytes(), 0, 4).unwrap();
        assert_eq!(output.text, "caf");
        assert_eq!(
            output.kind,
            OutputKind::FileChunk {
                offset: 0,
                more: true
            }
        );
        let rest = chunk("café!".as_bytes(), 3, 8).unwrap();
        assert_eq!(rest.text, "é!");
        assert_eq!(
            rest.kind,
            OutputKind::FileChunk {
                offset: 3,
                more: false
            }
        );
        assert!(
            chunk("café!".as_bytes(), 4, 8).is_err(),
            "inside a character"
        );
        assert!(chunk(&[0x66, 0xff, 0x66], 0, 8).is_err(), "not UTF-8");
        assert!(chunk(b"abc", 4, 8).is_err(), "past the end");
        assert_eq!(chunk(b"abc", 3, 8).unwrap().text, "");
    }
}
