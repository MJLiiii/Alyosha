//! Photos, voice, video, and files in a session, with a scripted model, a
//! fake transcriber, and a fake media unit. See `docs/Media.md#verification`.

mod support;

use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use alyosha::{
    BoxFuture,
    agent_loop::{SessionDriver, SessionEvent, SessionSetup},
    agent_state::{Attachment, HistoryEntry, ToolOutcome, TurnOutcome, UserEntry, UserPart},
    clock::FixedClock,
    media::{
        Extracted, Job, MediaSetup, MemoryMedia, NOT_SHOWN, Transcriber,
        VideoRunner,
    },
    memory::InMemoryTables,
    session::{self, FrontEnd, Started},
    tools::{SessionServices, ToolRegistry},
};
use alyosha_config::{Config, Paths};
use chrono::{TimeZone, Utc};
use image::{DynamicImage, Rgb, RgbImage, codecs::jpeg::JpegEncoder};
use serde_json::json;
use support::{ScriptedModel, call, events_until_turn_ends, text};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

fn jpeg(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    DynamicImage::ImageRgb8(RgbImage::from_pixel(width, height, Rgb([20, 120, 220])))
        .write_with_encoder(JpegEncoder::new(&mut bytes))
        .unwrap();
    bytes
}

/// A two-page Ogg Opus stream of `seconds`.
fn voice(seconds: i64) -> Vec<u8> {
    let page = |granule: i64, body: &[u8]| {
        let mut page = b"OggS".to_vec();
        page.extend_from_slice(&[0, 0]);
        page.extend_from_slice(&granule.to_le_bytes());
        page.extend_from_slice(&[0; 12]);
        page.push(1);
        page.push(body.len() as u8);
        page.extend_from_slice(body);
        page
    };
    let mut head = b"OpusHead".to_vec();
    head.extend_from_slice(&[1, 1, 0, 0]);
    head.extend_from_slice(&[0; 7]);
    let mut bytes = page(0, &head);
    bytes.extend(page(seconds * 48_000, &[1, 2, 3]));
    bytes
}

fn attachment(mime: &str, name: Option<&str>, bytes: Vec<u8>) -> Attachment {
    Attachment {
        mime: mime.into(),
        name: name.map(str::to_owned),
        bytes,
    }
}

#[derive(Default)]
struct FakeTranscriber {
    calls: Mutex<Vec<(usize, String)>>,
    fail: Mutex<Option<String>>,
}

impl Transcriber for FakeTranscriber {
    fn transcribe<'a>(
        &'a self,
        audio: Vec<u8>,
        mime: &'a str,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<String, String>> {
        self.calls.lock().unwrap().push((audio.len(), mime.to_owned()));
        let fail = self.fail.lock().unwrap().clone();
        Box::pin(async move {
            match fail {
                Some(reason) => Err(reason),
                None => Ok("Move tomorrow's call to 3.".to_owned()),
            }
        })
    }
}

#[derive(Default)]
struct FakeVideo {
    jobs: Mutex<Vec<Job>>,
    /// When set, the run waits for it, ignoring Stop like a slow unit.
    gate: Mutex<Option<(Arc<Notify>, Arc<Notify>)>>,
}

impl VideoRunner for FakeVideo {
    fn extract<'a>(
        &'a self,
        _input: Vec<u8>,
        job: Job,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<Extracted, String>> {
        self.jobs.lock().unwrap().push(job);
        let gate = self.gate.lock().unwrap().clone();
        Box::pin(async move {
            if let Some((started, _release)) = gate {
                started.notify_one();
                cancel.cancelled().await;
                return Err("stopped".into());
            }
            Ok(Extracted {
                duration: Some(75.0),
                frames: if job.frames {
                    vec![jpeg(1920, 1080), jpeg(1920, 1080), jpeg(1920, 1080)]
                } else {
                    Vec::new()
                },
                audio: Some(voice(75)),
            })
        })
    }
}

struct Fakes {
    store: Arc<MemoryMedia>,
    transcriber: Arc<FakeTranscriber>,
    video: Arc<FakeVideo>,
    with_transcriber: bool,
}

impl Fakes {
    fn new() -> Self {
        Self {
            store: MemoryMedia::new(),
            transcriber: Arc::default(),
            video: Arc::default(),
            with_transcriber: true,
        }
    }

    fn setup(&self) -> MediaSetup {
        let transcriber = self.transcriber.clone();
        let with = self.with_transcriber;
        MediaSetup {
            store: self.store.clone(),
            transcriber: Arc::new(move |_: &Config| {
                with.then(|| transcriber.clone() as Arc<dyn Transcriber>)
            }),
            video: self.video.clone(),
        }
    }
}

struct Home {
    _root: tempfile::TempDir,
    paths: Paths,
    workspace: PathBuf,
}

fn home() -> Home {
    let root = tempfile::tempdir().unwrap();
    let base = root.path().canonicalize().unwrap();
    let paths = Paths::under(&base);
    let workspace = base.join("work/owner");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&paths.state_dir).unwrap();
    Home {
        _root: root,
        paths,
        workspace,
    }
}

async fn open(home: &Home, model: Arc<ScriptedModel>, fakes: &Fakes, config: &Config) -> Started {
    session::start_with_clock(
        config,
        &home.paths,
        &home.workspace,
        model,
        FrontEnd {
            approver: None,
            channel_line: None,
            log_hint: None,
            on_loading_dev_shell: None,
            persistence: None,
            memory: InMemoryTables::new(),
            media: Some(fakes.setup()),
        },
        Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 11, 25, 9, 2, 0).unwrap(),
        )),
    )
    .await
    .unwrap()
}

/// Sends one message with attachments and returns its opening entry, the
/// turn's events, and its outcome.
async fn send(
    driver: &SessionDriver,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<SessionEvent>,
    message: &str,
    attachments: Vec<Attachment>,
) -> (Option<UserEntry>, Vec<SessionEvent>, TurnOutcome) {
    let id = driver.submit_message(message, attachments, None);
    let seen = events_until_turn_ends(events, id).await;
    let state = driver.snapshot();
    let entry = state.history.iter().find_map(|entry| match entry {
        HistoryEntry::User(user) if user.message_id == id => Some(user.clone()),
        _ => None,
    });
    (entry, seen, state.outcomes[&id].clone())
}

fn texts(entry: &UserEntry) -> Vec<String> {
    entry
        .parts
        .iter()
        .map(|part| match part {
            UserPart::Text(text) => text.clone(),
            UserPart::Image(image) => format!("<image {}x{}>", image.width, image.height),
        })
        .collect()
}

#[tokio::test]
async fn a_photo_is_shrunk_kept_saved_and_sent() {
    let home = home();
    let fakes = Fakes::new();
    let model = ScriptedModel::new();
    model.reply(vec![text("A blue square.")]);
    model.reply(vec![text("Still blue.")]);
    let mut started = open(&home, model.clone(), &fakes, &Config::default()).await;
    let photo = jpeg(4000, 3000);
    let (entry, events, outcome) = send(
        &started.driver,
        &mut started.events,
        "What is this?",
        vec![attachment("image/jpeg", None, photo.clone())],
    )
    .await;
    assert_eq!(outcome, TurnOutcome::Completed);
    let entry = entry.unwrap();
    assert_eq!(
        texts(&entry),
        vec![
            "[Wed 2026-11-25 10:02, Europe/Amsterdam]\nWhat is this?".to_owned(),
            "[photo: inbox/2026-11-25-1002-photo.jpg]".to_owned(),
            "<image 2048x1536>".to_owned(),
        ]
    );
    assert!(events.contains(&SessionEvent::Notice("Reading attachments…".into())));
    // The original is saved, the shrunk copy kept once.
    assert_eq!(
        fs::read(home.workspace.join("inbox/2026-11-25-1002-photo.jpg")).unwrap(),
        photo
    );
    assert_eq!(fakes.store.len(), 1);
    let image = entry.images().next().unwrap().clone();
    let request = &model.requests()[0];
    assert_eq!(request.media[&image.sha256].mime, "image/jpeg");

    // The same photo again: one row, a second file.
    let (_, _, outcome) = send(
        &started.driver,
        &mut started.events,
        "And this?",
        vec![attachment("image/jpeg", None, photo)],
    )
    .await;
    assert_eq!(outcome, TurnOutcome::Completed);
    assert_eq!(fakes.store.len(), 1);
    assert!(home.workspace.join("inbox/2026-11-25-1002-photo-2.jpg").is_file());
}

#[tokio::test]
async fn a_missing_image_row_fails_the_turn_with_its_hash() {
    let home = home();
    let fakes = Fakes::new();
    let model = ScriptedModel::new();
    model.reply(vec![text("Blue.")]);
    let mut started = open(&home, model.clone(), &fakes, &Config::default()).await;
    let (entry, _, _) = send(
        &started.driver,
        &mut started.events,
        "",
        vec![attachment("", None, jpeg(10, 10))],
    )
    .await;
    let hash = entry.unwrap().images().next().unwrap().sha256.clone();
    fakes.store.remove(&hash);
    let (_, _, outcome) = send(&started.driver, &mut started.events, "Again?", Vec::new()).await;
    let TurnOutcome::Failed(reason) = outcome else {
        panic!("{outcome:?}");
    };
    assert!(reason.contains(&hash), "{reason}");
}

#[tokio::test]
async fn voice_is_transcribed_and_failures_are_lines() {
    let home = home();
    let mut fakes = Fakes::new();
    let model = ScriptedModel::new();
    for _ in 0..3 {
        model.reply(vec![text("Noted.")]);
    }
    let mut started = open(&home, model.clone(), &fakes, &Config::default()).await;
    let (entry, _, _) = send(
        &started.driver,
        &mut started.events,
        "",
        vec![attachment("audio/ogg", None, voice(12))],
    )
    .await;
    assert_eq!(
        texts(&entry.unwrap())[1..],
        [
            "[voice 0:12: inbox/2026-11-25-1002-voice.ogg]".to_owned(),
            "Move tomorrow's call to 3.".to_owned(),
        ]
    );
    // Opus goes straight to transcription, without the media unit.
    assert!(fakes.video.jobs.lock().unwrap().is_empty());
    assert_eq!(fakes.transcriber.calls.lock().unwrap()[0].1, "audio/ogg");

    *fakes.transcriber.fail.lock().unwrap() = Some("rate limit".into());
    let (entry, _, outcome) = send(
        &started.driver,
        &mut started.events,
        "",
        vec![attachment("audio/ogg", None, voice(12))],
    )
    .await;
    assert_eq!(outcome, TurnOutcome::Completed);
    assert_eq!(
        texts(&entry.unwrap())[1..],
        ["[voice 0:12: inbox/2026-11-25-1002-voice-2.ogg, could not transcribe: rate limit]".to_owned()]
    );
    started.driver.end().await;

    fakes.with_transcriber = false;
    let mut started = open(&home, model, &fakes, &Config::default()).await;
    let (entry, _, _) = send(
        &started.driver,
        &mut started.events,
        "",
        vec![attachment("audio/ogg", None, voice(3))],
    )
    .await;
    assert!(texts(&entry.unwrap())[1].ends_with("could not transcribe: transcription is not set up]"));
}

#[tokio::test]
async fn a_video_gives_frames_and_a_transcript() {
    let home = home();
    let fakes = Fakes::new();
    let model = ScriptedModel::new();
    model.reply(vec![text("A short clip.")]);
    let mut started = open(&home, model, &fakes, &Config::default()).await;
    let (entry, _, _) = send(
        &started.driver,
        &mut started.events,
        "What happens?",
        vec![attachment("video/mp4", Some("clip.mp4"), b"\0\0\0\x18ftypisom".to_vec())],
    )
    .await;
    assert_eq!(
        texts(&entry.unwrap())[1..],
        [
            "[video 1:15: inbox/2026-11-25-1002-clip.mp4]".to_owned(),
            "<image 1024x576>".to_owned(),
            "<image 1024x576>".to_owned(),
            "<image 1024x576>".to_owned(),
            "Move tomorrow's call to 3.".to_owned(),
        ]
    );
    assert_eq!(*fakes.video.jobs.lock().unwrap(), vec![Job { frames: true }]);
}

#[tokio::test]
async fn files_are_saved_with_their_size() {
    let home = home();
    let fakes = Fakes::new();
    let model = ScriptedModel::new();
    model.reply(vec![text("An invoice.")]);
    let mut started = open(&home, model, &fakes, &Config::default()).await;
    let (entry, _, _) = send(
        &started.driver,
        &mut started.events,
        "",
        vec![attachment(
            "application/pdf",
            Some("../Invoice.pdf"),
            vec![b'%'; 186_000],
        )],
    )
    .await;
    assert_eq!(
        texts(&entry.unwrap())[1..],
        ["[file: inbox/2026-11-25-1002-Invoice.pdf, 182 KB]".to_owned()]
    );
    assert!(home.workspace.join("inbox/2026-11-25-1002-Invoice.pdf").is_file());
}

#[tokio::test]
async fn a_model_without_images_gets_a_line() {
    let home = home();
    let fakes = Fakes::new();
    let model = ScriptedModel::new();
    model.reply(vec![text("I cannot see it.")]);
    let mut config = Config::default();
    config.model.images = false;
    let mut started = open(&home, model, &fakes, &config).await;
    let (entry, _, _) = send(
        &started.driver,
        &mut started.events,
        "",
        vec![attachment("image/jpeg", None, jpeg(8, 8))],
    )
    .await;
    assert_eq!(texts(&entry.unwrap())[2], NOT_SHOWN);
    assert!(fakes.store.is_empty());
}

/// A session without a workspace, as a guest session is.
fn guest(model: Arc<ScriptedModel>, fakes: &Fakes) -> (SessionDriver, tokio::sync::mpsc::UnboundedReceiver<SessionEvent>) {
    let mut setup = SessionSetup::minimal(
        "chatgpt",
        "test-model",
        chrono_tz::Europe::Amsterdam,
        model,
        ToolRegistry::new(),
        Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 11, 25, 9, 2, 0).unwrap(),
        )),
    );
    setup.services = SessionServices {
        media: Some(fakes.setup().services(&Config::default())),
        ..SessionServices::default()
    };
    SessionDriver::start(setup)
}

#[tokio::test]
async fn guests_see_images_but_not_files() {
    let fakes = Fakes::new();
    let model = ScriptedModel::new();
    model.reply(vec![text("A photo and a file.")]);
    let (driver, mut events) = guest(model, &fakes);
    let (entry, _, _) = send(
        &driver,
        &mut events,
        "Bob: look",
        vec![
            attachment("image/png", None, jpeg(8, 8)),
            attachment("application/pdf", Some("report.pdf"), b"%PDF".to_vec()),
        ],
    )
    .await;
    assert_eq!(
        texts(&entry.unwrap())[1..],
        [
            "[photo]".to_owned(),
            "<image 8x8>".to_owned(),
            "[file report.pdf: files are not read in this chat]".to_owned(),
        ]
    );
}

#[tokio::test]
async fn stop_during_preparation_records_nothing() {
    let home = home();
    let fakes = Fakes::new();
    let started_run = Arc::new(Notify::new());
    *fakes.video.gate.lock().unwrap() = Some((started_run.clone(), Arc::new(Notify::new())));
    let model = ScriptedModel::new();
    let mut started = open(&home, model.clone(), &fakes, &Config::default()).await;
    let id = started.driver.submit_message(
        "",
        vec![attachment("video/mp4", None, b"\0\0\0\x18ftypisom".to_vec())],
        None,
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), started_run.notified())
        .await
        .unwrap();
    started.driver.stop();
    events_until_turn_ends(&mut started.events, id).await;
    let state = started.driver.snapshot();
    assert_eq!(state.outcomes[&id], TurnOutcome::Stopped);
    assert!(state.history.is_empty());
    assert!(model.requests().is_empty());
}

#[tokio::test]
async fn send_file_copies_the_file_and_tells_the_front_end() {
    let home = home();
    let fakes = Fakes::new();
    fs::write(home.workspace.join("chart.png"), b"png bytes").unwrap();
    fs::write(home.workspace.join("big.bin"), b"").unwrap();
    fs::File::options()
        .write(true)
        .open(home.workspace.join("big.bin"))
        .unwrap()
        .set_len(51 * 1024 * 1024)
        .unwrap();
    std::os::unix::fs::symlink("/etc/hostname", home.workspace.join("outside")).unwrap();
    let model = ScriptedModel::new();
    model.reply(vec![
        call("a", "send_file", json!({"path": "chart.png", "caption": "Sales"})),
        call("b", "send_file", json!({"path": "outside"})),
        call("c", "send_file", json!({"path": "big.bin"})),
        call("d", "send_file", json!({"path": "."})),
    ]);
    model.reply(vec![text("Sent.")]);
    let mut started = open(&home, model, &fakes, &Config::default()).await;
    let (_, events, _) = send(&started.driver, &mut started.events, "Send the chart", Vec::new()).await;
    let files: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::File(file) => Some(file.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(files.len(), 1);
    let file = &files[0];
    assert_eq!(file.name, "chart.png");
    assert_eq!(file.mime, "image/png");
    assert_eq!(file.caption.as_deref(), Some("Sales"));
    assert_eq!(fs::read(&file.path).unwrap(), b"png bytes");
    assert!(file.path.parent().unwrap().ends_with("outbox"));
    let results: Vec<ToolOutcome> = started
        .driver
        .snapshot()
        .history
        .iter()
        .filter_map(|entry| match entry {
            HistoryEntry::ToolResult(result) => Some(result.outcome.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        results[0],
        ToolOutcome::Success(r#"{"status":"queued","name":"chart.png","bytes":9}"#.into())
    );
    for result in &results[1..] {
        assert!(matches!(result, ToolOutcome::Error(_)), "{result:?}");
    }
    let ToolOutcome::Error(big) = &results[2] else { unreachable!() };
    assert!(big.contains("50 MB"), "{big}");
    // The copy goes when the session ends.
    started.driver.end().await;
    assert!(!file.path.exists());
}

fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    DynamicImage::ImageRgb8(RgbImage::from_pixel(width, height, Rgb([200, 30, 30])))
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
    bytes
}

/// The recorded tool results of a session, in order.
fn results(driver: &SessionDriver) -> Vec<alyosha::agent_state::ToolResultEntry> {
    driver
        .snapshot()
        .history
        .iter()
        .filter_map(|entry| match entry {
            HistoryEntry::ToolResult(result) => Some(result.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn read_file_shows_an_image_and_code_mode_refuses_it() {
    let home = home();
    let fakes = Fakes::new();
    fs::write(home.workspace.join("chart.png"), png(4000, 3000)).unwrap();
    fs::write(home.workspace.join("notes.txt"), "hello").unwrap();
    fs::write(home.workspace.join("big.jpg"), jpeg(8, 8)).unwrap();
    fs::File::options()
        .write(true)
        .open(home.workspace.join("big.jpg"))
        .unwrap()
        .set_len(21 * 1024 * 1024)
        .unwrap();
    let script = "async (tools) => { try { await tools.read_file({ path: \"chart.png\" }); return \"read\"; } catch (e) { return String(e.message || e); } }";
    let model = ScriptedModel::new();
    model.reply(vec![
        call("a", "read_file", json!({"path": "chart.png"})),
        call("b", "read_file", json!({"path": "notes.txt"})),
        call("c", "read_file", json!({"path": "big.jpg"})),
        call("d", "execute_code", json!({"code": script})),
    ]);
    model.reply(vec![text("A red chart.")]);
    let mut started = open(&home, model.clone(), &fakes, &Config::default()).await;
    let (_, _, outcome) = send(
        &started.driver,
        &mut started.events,
        "Look at chart.png",
        Vec::new(),
    )
    .await;
    assert_eq!(outcome, TurnOutcome::Completed);

    let results = results(&started.driver);
    let ToolOutcome::Success(line) = &results[0].outcome else {
        panic!("{:?}", results[0]);
    };
    assert!(
        line.starts_with("chart.png: image/png, 4000×3000, "),
        "{line}"
    );
    assert_eq!(results[0].images.len(), 1);
    let image = &results[0].images[0];
    assert_eq!((image.width, image.height), (2048, 1536));
    assert_eq!(fakes.store.len(), 1);
    assert_eq!(results[1].outcome, ToolOutcome::Success("hello".into()));
    assert!(results[1].images.is_empty());
    let ToolOutcome::Error(big) = &results[2].outcome else {
        panic!("{:?}", results[2]);
    };
    assert!(big.contains("big.jpg is an image over 20 MB."), "{big}");
    let ToolOutcome::Success(code) = &results[3].outcome else {
        panic!("{:?}", results[3]);
    };
    assert!(
        code.contains("chart.png is an image. Code Mode reads text only."),
        "{code}"
    );
    // The next request carries the image's bytes.
    let request = &model.requests()[1];
    assert_eq!(request.media[&image.sha256].mime, image.mime);
}

/// A minimal session with one tool, `pictures`, that returns `images`.
fn picture_session(
    model: Arc<ScriptedModel>,
    fakes: &Fakes,
    config: &Config,
    images: Vec<alyosha::media::RawImage>,
) -> (
    SessionDriver,
    tokio::sync::mpsc::UnboundedReceiver<SessionEvent>,
) {
    use alyosha::tools::{RegisteredTool, ToolDefinition, ToolOutput, handler};
    let mut registry = ToolRegistry::new();
    registry
        .register(RegisteredTool {
            definition: ToolDefinition {
                name: "pictures".into(),
                description: "Return pictures.".into(),
                input_schema: json!({"type": "object"}),
                requires_approval: false,
            },
            handler: handler(move |_, _| {
                let images = images.clone();
                async move {
                    Ok(ToolOutput {
                        text: "Here they are.".into(),
                        images,
                        ..ToolOutput::default()
                    })
                }
            }),
        })
        .unwrap();
    let mut setup = SessionSetup::minimal(
        "chatgpt",
        "test-model",
        chrono_tz::Europe::Amsterdam,
        model,
        registry,
        Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 11, 25, 9, 2, 0).unwrap(),
        )),
    );
    setup.services = SessionServices {
        media: Some(fakes.setup().services(config)),
        ..SessionServices::default()
    };
    SessionDriver::start(setup)
}

fn raw(bytes: Vec<u8>) -> alyosha::media::RawImage {
    alyosha::media::RawImage {
        mime: "image/png".into(),
        bytes,
    }
}

#[tokio::test]
async fn a_result_shows_four_images_and_lines_for_the_rest() {
    let fakes = Fakes::new();
    let model = ScriptedModel::new();
    model.reply(vec![call("p", "pictures", json!({}))]);
    model.reply(vec![text("Seen.")]);
    let mut images = vec![raw(b"not an image".to_vec())];
    images.extend((1..=5).map(|side| raw(png(side * 10, 10))));
    let (driver, mut events) =
        picture_session(model.clone(), &fakes, &Config::default(), images.clone());
    let (_, _, outcome) = send(&driver, &mut events, "Show me", Vec::new()).await;
    assert_eq!(outcome, TurnOutcome::Completed);
    let result = &results(&driver)[0];
    assert_eq!(result.images.len(), 3, "the first four, one broken");
    assert_eq!(
        result
            .images
            .iter()
            .map(|image| image.width)
            .collect::<Vec<_>>(),
        [10, 20, 30]
    );
    let ToolOutcome::Success(shown) = &result.outcome else {
        panic!("{result:?}");
    };
    assert!(
        shown.starts_with("Here they are.\n[image could not be read: "),
        "{shown}"
    );
    assert!(shown.ends_with("\n[2 more images not shown]"), "{shown}");
    assert_eq!(fakes.store.len(), 3);
    assert_eq!(model.requests()[1].media.len(), 3);

    // A model without images gets a line for each and nothing is stored.
    let fakes = Fakes::new();
    let model = ScriptedModel::new();
    model.reply(vec![call("p", "pictures", json!({}))]);
    model.reply(vec![text("Blind.")]);
    let mut config = Config::default();
    config.model.images = false;
    let (driver, mut events) = picture_session(model, &fakes, &config, images[1..3].to_vec());
    send(&driver, &mut events, "Show me", Vec::new()).await;
    let result = &results(&driver)[0];
    assert!(result.images.is_empty());
    assert_eq!(
        result.outcome,
        ToolOutcome::Success(format!("Here they are.\n{NOT_SHOWN}\n{NOT_SHOWN}"))
    );
    assert!(fakes.store.is_empty());
}
