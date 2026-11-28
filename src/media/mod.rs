//! Photos, voice, video, and files: what people send becomes images and
//! text for the model, and files are saved in the session's `inbox/`.
//!
//! Design: `docs/Media.md`. Implementation guide:
//! `docs/Media.md#implementation`.

pub mod image;
pub mod ogg;
pub mod send_file;
pub mod store;
pub mod transcribe;
pub mod video;

use std::sync::Arc;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use tokio_util::sync::CancellationToken;

pub use store::{MediaStore, MemoryMedia, sha256_hex};
pub use transcribe::{GatewayTranscriber, Transcriber};
pub use video::{Extracted, Job, VideoRunner};

use crate::{
    agent_state::{Attachment, MediaRef, UserPart},
    environment::{Environment, FileAccess, ReplaceMode},
    tools::Scope,
};

/// The most attachments one message may carry.
pub const MAX_ATTACHMENTS: usize = 10;
/// The most bytes one attachment may have.
pub const MAX_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;
/// The folder in the workspace where attachments are saved.
pub const INBOX: &str = "inbox";
/// What a model that cannot see images gets instead of one.
pub const NOT_SHOWN: &str = "[image not shown: this model cannot see images]";
/// The status line while attachments are prepared.
pub const READING: &str = "Reading attachments…";
/// The most images one tool result shows.
pub const MAX_RESULT_IMAGES: usize = 4;
/// The largest image file `read_file` opens.
pub const MAX_IMAGE_FILE: u64 = 20 * 1024 * 1024;
/// What a session without a media store shows instead of a tool's image.
pub const NOT_KEPT: &str = "[image not shown: this session cannot keep images]";

/// An image a tool returned, before it is prepared.
/// See `docs/Media.md#images-in-tool-results`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawImage {
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// What a session without media services records for a tool's images.
pub fn unkept_lines(images: &[RawImage]) -> Vec<String> {
    let shown = images.len().min(MAX_RESULT_IMAGES);
    let mut lines = vec![NOT_KEPT.to_owned(); shown];
    if images.len() > shown {
        lines.push(format!("[{} more images not shown]", images.len() - shown));
    }
    lines
}

/// What a session needs to prepare attachments.
pub struct MediaServices {
    /// Where images are kept.
    pub store: Arc<dyn MediaStore>,
    /// Speech to text, if `[media] transcribe_provider` is set.
    pub transcriber: Option<Arc<dyn Transcriber>>,
    /// FFmpeg, in the media unit.
    pub video: Arc<dyn VideoRunner>,
    /// `[model] images`: whether the model sees images.
    pub images: bool,
    /// For the date and time in saved files' names.
    pub timezone: Tz,
}

/// Makes the transcriber for a session from its configuration.
pub type TranscriberFactory =
    Arc<dyn Fn(&alyosha_config::Config) -> Option<Arc<dyn Transcriber>> + Send + Sync>;

/// What a front end gives sessions to read attachments with.
#[derive(Clone)]
pub struct MediaSetup {
    pub store: Arc<dyn MediaStore>,
    pub transcriber: TranscriberFactory,
    pub video: Arc<dyn VideoRunner>,
}

impl MediaSetup {
    /// The services for a session with this configuration.
    pub fn services(&self, config: &alyosha_config::Config) -> Arc<MediaServices> {
        Arc::new(MediaServices {
            store: self.store.clone(),
            transcriber: (self.transcriber)(config),
            video: self.video.clone(),
            images: config.model.images,
            timezone: config.timezone,
        })
    }

    /// The real setup: images in `store`, OpenAI through the gateway with
    /// the secret `gateway-key`, and FFmpeg in the media unit.
    pub fn real(store: Arc<dyn MediaStore>, paths: alyosha_config::Paths) -> Self {
        Self {
            store,
            transcriber: Arc::new(move |config| transcriber(config, &paths)),
            video: video::runner(),
        }
    }
}

/// The transcriber `[media]` asks for, if any.
pub fn transcriber(
    config: &alyosha_config::Config,
    paths: &alyosha_config::Paths,
) -> Option<Arc<dyn Transcriber>> {
    let provider = config.media.transcribe_provider.as_deref()?;
    let key = paths.secrets().read(alyosha_config::GATEWAY_KEY).ok()?;
    Some(Arc::new(GatewayTranscriber::new(
        &config.gateway.url,
        key.expose(),
        provider,
        &config.media.transcribe_model,
    )))
}

/// What an attachment is, by its type or its bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Image,
    Audio,
    Video,
    File,
}

/// The attachment's kind and MIME type, sniffing the bytes when the type is
/// missing or vague.
pub fn kind(attachment: &Attachment) -> (Kind, String) {
    if let Some(format) = image::format(&attachment.bytes) {
        return (Kind::Image, format.to_mime_type().to_owned());
    }
    let mime = attachment.mime.trim().to_ascii_lowercase();
    if mime.starts_with("audio/") {
        return (Kind::Audio, mime);
    }
    if mime.starts_with("video/") {
        return (Kind::Video, mime);
    }
    let vague = mime.is_empty() || mime == "application/octet-stream";
    let bytes = &attachment.bytes;
    if vague {
        if bytes.starts_with(b"OggS") {
            return (Kind::Audio, "audio/ogg".into());
        }
        if bytes.get(4..8) == Some(b"ftyp") {
            return (Kind::Video, "video/mp4".into());
        }
        if bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
            return (Kind::Video, "video/webm".into());
        }
        if bytes.starts_with(b"ID3") || bytes.starts_with(&[0xff, 0xfb]) {
            return (Kind::Audio, "audio/mpeg".into());
        }
    }
    let mime = if vague {
        attachment
            .name
            .as_deref()
            .map(mime_for_name)
            .unwrap_or("application/octet-stream")
            .to_owned()
    } else {
        mime
    };
    (Kind::File, mime)
}

/// The MIME type for a file name's extension.
pub fn mime_for_name(name: &str) -> &'static str {
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "pdf" => "application/pdf",
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "csv" => "text/csv",
        "json" => "application/json",
        "html" | "htm" => "text/html",
        "zip" => "application/zip",
        "svg" => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

fn extension_for(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/quicktime" => "mov",
        "audio/ogg" | "audio/opus" => "ogg",
        "audio/mpeg" => "mp3",
        "audio/mp4" => "m4a",
        "audio/wav" | "audio/x-wav" => "wav",
        "application/pdf" => "pdf",
        _ => "bin",
    }
}

/// A file name safe to save: no slashes or control characters, not hidden,
/// at most 100 characters.
pub fn clean_name(name: &str) -> Option<String> {
    let cleaned: String = name
        .chars()
        .filter(|c| *c != '/' && *c != '\\' && !c.is_control())
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').trim();
    if cleaned.is_empty() {
        return None;
    }
    let mut kept: String = cleaned.chars().take(100).collect();
    // Keep the extension when a long name is cut.
    if cleaned.chars().count() > 100
        && let Some((_, extension)) = cleaned.rsplit_once('.')
        && extension.len() <= 10
    {
        kept = format!(
            "{}.{extension}",
            cleaned.chars().take(99 - extension.len()).collect::<String>()
        );
    }
    Some(kept)
}

/// `0:42`, `12:05`, or `1:02:03`.
pub fn duration_text(seconds: f64) -> String {
    let total = seconds.round() as u64;
    let (hours, minutes, seconds) = (total / 3600, total / 60 % 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// `182 KB` or `3.4 MB`.
pub fn size_text(bytes: usize) -> String {
    if bytes < 1024 * 1024 {
        format!("{} KB", bytes.div_ceil(1024))
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// One attachment's line: `[<label>: <path>, <detail>, <problem>]`, with
/// the parts it has.
struct Line {
    label: String,
    path: Option<String>,
    detail: Option<String>,
    problem: Option<String>,
}

impl Line {
    fn text(&self) -> String {
        let mut text = format!("[{}", self.label);
        let mut rest = Vec::new();
        rest.extend(self.path.clone());
        rest.extend(self.detail.clone());
        rest.extend(self.problem.clone());
        if !rest.is_empty() {
            text.push_str(": ");
            text.push_str(&rest.join(", "));
        }
        text.push(']');
        text
    }
}

impl MediaServices {
    /// Turns a message's attachments into parts, in order: for each, a line
    /// naming it, then its images or transcript. Files are saved in the
    /// workspace's `inbox/` when there is a workspace. `None` means Stop
    /// came first.
    pub async fn prepare(
        &self,
        environment: Option<&Environment>,
        attachments: Vec<Attachment>,
        submitted_at: DateTime<Utc>,
        cancel: &CancellationToken,
    ) -> Option<Vec<UserPart>> {
        let mut parts = Vec::new();
        for attachment in attachments {
            if cancel.is_cancelled() {
                return None;
            }
            parts.extend(
                self.prepare_one(environment, attachment, submitted_at, cancel)
                    .await?,
            );
        }
        Some(parts)
    }

    async fn prepare_one(
        &self,
        environment: Option<&Environment>,
        attachment: Attachment,
        submitted_at: DateTime<Utc>,
        cancel: &CancellationToken,
    ) -> Option<Vec<UserPart>> {
        let (kind, mime) = kind(&attachment);
        let mut line = Line {
            label: match kind {
                Kind::Image => "photo".into(),
                Kind::Audio if attachment.name.is_none() => "voice".into(),
                Kind::Audio => "audio".into(),
                Kind::Video => "video".into(),
                Kind::File => "file".into(),
            },
            path: None,
            detail: None,
            problem: None,
        };
        let Some(environment) = environment else {
            if kind == Kind::File {
                let name = attachment
                    .name
                    .as_deref()
                    .and_then(clean_name)
                    .unwrap_or_else(|| "file".into());
                return Some(vec![UserPart::Text(format!(
                    "[file {name}: files are not read in this chat]"
                ))]);
            }
            return self.understand(kind, &mime, attachment, line, cancel).await;
        };
        let name = attachment
            .name
            .as_deref()
            .and_then(clean_name)
            .unwrap_or_else(|| {
                let stem = match kind {
                    Kind::Image => "photo",
                    Kind::Audio if attachment.name.is_none() => "voice",
                    Kind::Audio => "audio",
                    Kind::Video => "video",
                    Kind::File => "file",
                };
                format!("{stem}.{}", extension_for(&mime))
            });
        let stamp = submitted_at.with_timezone(&self.timezone).format("%Y-%m-%d-%H%M");
        match save(environment, &format!("{stamp}-{name}"), &attachment.bytes) {
            Ok(path) => line.path = Some(path),
            Err(error) => {
                tracing::warn!(error = %error, "could not save an attachment");
                line.problem = Some(format!("could not save it: {error}"));
            }
        }
        if kind == Kind::File {
            line.detail = Some(size_text(attachment.bytes.len()));
            return Some(vec![UserPart::Text(line.text())]);
        }
        self.understand(kind, &mime, attachment, line, cancel).await
    }

    /// The parts for an image, sound, or video.
    async fn understand(
        &self,
        kind: Kind,
        mime: &str,
        attachment: Attachment,
        mut line: Line,
        cancel: &CancellationToken,
    ) -> Option<Vec<UserPart>> {
        let mut parts = Vec::new();
        match kind {
            Kind::Image => {
                if !self.images {
                    return Some(vec![
                        UserPart::Text(line.text()),
                        UserPart::Text(NOT_SHOWN.into()),
                    ]);
                }
                let bytes = attachment.bytes;
                let decoded = tokio::task::spawn_blocking(move || -> Result<Vec<image::Encoded>, String> {
                    match image::animation_frames(&bytes)? {
                        Some(frames) => frames
                            .into_iter()
                            .map(|frame| image::encode(frame, image::FRAME_SIDE))
                            .collect(),
                        None => Ok(vec![image::normalize(&bytes, image::MAX_SIDE)?]),
                    }
                })
                .await
                .unwrap_or_else(|error| Err(error.to_string()));
                match decoded {
                    Ok(images) => {
                        if images.len() > 1 {
                            line.label = "GIF".into();
                        }
                        for encoded in images {
                            match self.store_image(encoded).await {
                                Ok(reference) => parts.push(UserPart::Image(reference)),
                                Err(error) => {
                                    line.problem = Some(format!("could not keep it: {error}"));
                                    parts.clear();
                                    break;
                                }
                            }
                        }
                    }
                    Err(error) => line.problem = Some(error),
                }
            }
            Kind::Audio => {
                let mut sound = Some((attachment.bytes, mime.to_owned()));
                let opus = sound.as_ref().is_some_and(|(bytes, _)| ogg::is_ogg_opus(bytes));
                if opus {
                    line.label = format_label(&line.label, sound.as_ref().and_then(|(b, _)| ogg::opus_duration(b)));
                } else {
                    match self.video.extract(sound.take().unwrap().0, Job { frames: false }, cancel.clone()).await {
                        _ if cancel.is_cancelled() => return None,
                        Ok(extracted) => {
                            line.label = format_label(&line.label, extracted.duration);
                            sound = extracted.audio.map(|audio| (audio, "audio/ogg".to_owned()));
                            if sound.is_none() {
                                line.problem = Some("it has no sound".into());
                            }
                        }
                        Err(error) => line.problem = Some(format!("could not read it: {error}")),
                    }
                }
                if let Some((audio, mime)) = sound {
                    match self.transcribe(audio, &mime, cancel).await {
                        _ if cancel.is_cancelled() => return None,
                        Ok(text) => parts.push(UserPart::Text(text)),
                        Err(error) => line.problem = Some(format!("could not transcribe: {error}")),
                    }
                }
            }
            Kind::Video => {
                let extracted = self
                    .video
                    .extract(attachment.bytes, Job { frames: true }, cancel.clone())
                    .await;
                if cancel.is_cancelled() {
                    return None;
                }
                match extracted {
                    Ok(extracted) => {
                        line.label = format_label(&line.label, extracted.duration);
                        if self.images {
                            for frame in extracted.frames {
                                let encoded = tokio::task::spawn_blocking(move || {
                                    image::normalize(&frame, image::FRAME_SIDE)
                                })
                                .await
                                .unwrap_or_else(|error| Err(error.to_string()));
                                match encoded {
                                    Ok(encoded) => match self.store_image(encoded).await {
                                        Ok(reference) => parts.push(UserPart::Image(reference)),
                                        Err(error) => {
                                            line.problem = Some(format!("could not keep a frame: {error}"));
                                        }
                                    },
                                    Err(error) => line.problem = Some(error),
                                }
                            }
                        } else {
                            parts.push(UserPart::Text(NOT_SHOWN.into()));
                        }
                        if let Some(audio) = extracted.audio {
                            match self.transcribe(audio, "audio/ogg", cancel).await {
                                _ if cancel.is_cancelled() => return None,
                                Ok(text) if !text.is_empty() => parts.push(UserPart::Text(text)),
                                Ok(_) => {}
                                Err(error) => {
                                    line.problem = Some(format!("could not transcribe: {error}"));
                                }
                            }
                        }
                    }
                    Err(error) => line.problem = Some(format!("could not read it: {error}")),
                }
            }
            Kind::File => {}
        }
        let mut all = vec![UserPart::Text(line.text())];
        all.extend(parts);
        Some(all)
    }

    /// Prepares the images a tool returned: the first four are decoded,
    /// shrunk, and stored like an attachment's. Returns their references and
    /// the lines to add to the result's text for those that are not shown.
    pub async fn prepare_result_images(
        &self,
        images: Vec<RawImage>,
    ) -> (Vec<MediaRef>, Vec<String>) {
        let total = images.len();
        let mut references = Vec::new();
        let mut lines = Vec::new();
        for raw in images.into_iter().take(MAX_RESULT_IMAGES) {
            if !self.images {
                lines.push(NOT_SHOWN.to_owned());
                continue;
            }
            let bytes = raw.bytes;
            let encoded =
                tokio::task::spawn_blocking(move || image::normalize(&bytes, image::MAX_SIDE))
                    .await
                    .unwrap_or_else(|error| Err(error.to_string()));
            match encoded {
                Ok(encoded) => match self.store_image(encoded).await {
                    Ok(reference) => references.push(reference),
                    Err(error) => lines.push(format!("[image could not be read: {error}]")),
                },
                Err(error) => lines.push(format!("[image could not be read: {error}]")),
            }
        }
        if total > MAX_RESULT_IMAGES {
            lines.push(format!(
                "[{} more images not shown]",
                total - MAX_RESULT_IMAGES
            ));
        }
        (references, lines)
    }

    async fn store_image(&self, encoded: image::Encoded) -> Result<MediaRef, String> {
        let sha256 = self.store.put_media(encoded.mime, &encoded.bytes).await?;
        Ok(MediaRef {
            sha256,
            mime: encoded.mime.to_owned(),
            width: encoded.width,
            height: encoded.height,
        })
    }

    async fn transcribe(
        &self,
        audio: Vec<u8>,
        mime: &str,
        cancel: &CancellationToken,
    ) -> Result<String, String> {
        match &self.transcriber {
            Some(transcriber) => transcriber.transcribe(audio, mime, cancel.clone()).await,
            None => Err("transcription is not set up".into()),
        }
    }
}

/// `voice 0:42`, or the label alone when the length is unknown.
fn format_label(label: &str, duration: Option<f64>) -> String {
    match duration {
        Some(seconds) => format!("{label} {}", duration_text(seconds)),
        None => label.to_owned(),
    }
}

/// Saves bytes as `inbox/<name>`, adding `-2`, `-3`, … before the extension
/// when the name is taken, and returns the path relative to the workspace.
pub fn save(environment: &Environment, name: &str, bytes: &[u8]) -> Result<String, String> {
    environment
        .create_dir(INBOX)
        .map_err(|error| error.to_string())?;
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, format!(".{extension}")),
        _ => (name, String::new()),
    };
    for attempt in 1..1000 {
        let candidate = if attempt == 1 {
            format!("{INBOX}/{name}")
        } else {
            format!("{INBOX}/{stem}-{attempt}{extension}")
        };
        if environment
            .open_file(&candidate, FileAccess::ReadWorkspace, Scope::Direct)
            .is_ok()
        {
            continue;
        }
        environment
            .replace_file(&candidate, bytes, ReplaceMode::CreateOrReplace)
            .map_err(|error| error.to_string())?;
        return Ok(candidate);
    }
    Err("too many files with that name".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attachment(mime: &str, name: Option<&str>, bytes: &[u8]) -> Attachment {
        Attachment {
            mime: mime.into(),
            name: name.map(str::to_owned),
            bytes: bytes.to_vec(),
        }
    }

    #[test]
    fn kinds_come_from_bytes_then_type() {
        let ogg = ogg::tests::sample(48_000);
        assert_eq!(kind(&attachment("", None, &ogg)).0, Kind::Audio);
        assert_eq!(
            kind(&attachment("video/mp4", Some("a.mp4"), b"xx")).0,
            Kind::Video
        );
        assert_eq!(
            kind(&attachment("", None, b"\0\0\0\x18ftypisom")),
            (Kind::Video, "video/mp4".into())
        );
        assert_eq!(
            kind(&attachment("", Some("report.PDF"), b"%PDF-1.7")),
            (Kind::File, "application/pdf".into())
        );
        // An image type whose bytes are no image Alyosha decodes is a file.
        assert_eq!(
            kind(&attachment("image/heic", Some("a.heic"), b"nothing")).0,
            Kind::File
        );
    }

    #[test]
    fn names_are_cleaned() {
        assert_eq!(clean_name("../../etc/passwd").as_deref(), Some("etcpasswd"));
        assert_eq!(clean_name(".hidden").as_deref(), Some("hidden"));
        assert_eq!(clean_name(" \u{7} "), None);
        let long = format!("{}.pdf", "a".repeat(200));
        let cleaned = clean_name(&long).unwrap();
        assert_eq!(cleaned.chars().count(), 100);
        assert!(cleaned.ends_with(".pdf"));
    }

    #[test]
    fn durations_and_sizes_read_like_a_chat() {
        assert_eq!(duration_text(42.4), "0:42");
        assert_eq!(duration_text(725.0), "12:05");
        assert_eq!(duration_text(3723.0), "1:02:03");
        assert_eq!(size_text(186_000), "182 KB");
        assert_eq!(size_text(3_565_158), "3.4 MB");
    }
}
