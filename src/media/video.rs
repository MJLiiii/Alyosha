//! Video and sound: FFmpeg reads the length, takes frames, and writes the
//! sound as Opus, in a unit of its own that cannot reach the network or the
//! secrets. See `docs/Media.md#video-and-sound-in-a-media-unit`.

use std::{
    fs,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::image::MAX_FRAMES;
use crate::{
    BoxFuture,
    environment::shell::{self, DIRECT_VARIABLE, RUNS_DIR},
};

/// The unit template that runs FFmpeg.
pub const TEMPLATE: &str = "alyosha-run-media";
/// How long one run may take.
pub const LIMIT: Duration = Duration::from_secs(5 * 60);
/// Chooses the FFmpeg binary; the package sets it to a store path.
pub const FFMPEG_VARIABLE: &str = "ALYOSHA_FFMPEG";
/// Chooses the ffprobe binary.
pub const FFPROBE_VARIABLE: &str = "ALYOSHA_FFPROBE";

/// What a run is asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    /// Take frames; false for sound only.
    pub frames: bool,
}

/// What a run found, as `result.json` holds it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Found {
    /// The length in seconds, if FFmpeg could tell.
    pub duration: Option<f64>,
    /// The frame files, in order.
    pub frames: Vec<String>,
    /// Whether `audio.ogg` was written.
    pub audio: bool,
}

/// What came out of a video or sound file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Extracted {
    pub duration: Option<f64>,
    /// JPEG frames, in order.
    pub frames: Vec<Vec<u8>>,
    /// The sound as mono Ogg Opus, if there was any.
    pub audio: Option<Vec<u8>>,
}

/// Runs FFmpeg on one file.
pub trait VideoRunner: Send + Sync {
    fn extract<'a>(
        &'a self,
        input: Vec<u8>,
        job: Job,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<Extracted, String>>;
}

/// The runner for this process: the media unit, or FFmpeg as a plain child
/// process with `ALYOSHA_COMMANDS=direct`, for development outside the
/// container.
pub fn runner() -> std::sync::Arc<dyn VideoRunner> {
    if std::env::var(DIRECT_VARIABLE).as_deref() == Ok("direct") {
        std::sync::Arc::new(DirectRunner)
    } else {
        std::sync::Arc::new(UnitRunner {
            runs_dir: PathBuf::from(RUNS_DIR),
        })
    }
}

fn binary(variable: &str, default: &str) -> PathBuf {
    std::env::var_os(variable).map_or_else(|| PathBuf::from(default), PathBuf::from)
}

/// Writes the input and the job into a fresh run folder.
fn prepare_folder(folder: &Path, input: &[u8], job: Job) -> Result<(), String> {
    fs::DirBuilder::new()
        .mode(0o770)
        .create(folder)
        .map_err(|error| format!("could not create {}: {error}", folder.display()))?;
    fs::write(folder.join("input"), input)
        .and_then(|()| {
            fs::write(
                folder.join("job.json"),
                serde_json::to_vec(&job).expect("a job serializes"),
            )
        })
        .map_err(|error| format!("could not write the run folder: {error}"))
}

/// Reads what a finished run left.
fn collect(folder: &Path) -> Result<Extracted, String> {
    let found: Found = fs::read(folder.join("result.json"))
        .map_err(|error| format!("FFmpeg left no result: {error}"))
        .and_then(|bytes| {
            serde_json::from_slice(&bytes).map_err(|error| format!("bad result: {error}"))
        })?;
    let mut frames = Vec::new();
    for name in &found.frames {
        if name.contains('/') {
            return Err(format!("bad frame name {name:?}"));
        }
        frames.push(
            fs::read(folder.join(name)).map_err(|error| format!("could not read {name}: {error}"))?,
        );
    }
    let audio = if found.audio {
        Some(
            fs::read(folder.join("audio.ogg"))
                .map_err(|error| format!("could not read the sound: {error}"))?,
        )
    } else {
        None
    };
    Ok(Extracted {
        duration: found.duration,
        frames,
        audio,
    })
}

/// Runs in the media unit, as `alyosha media-exec <instance>`: FFmpeg with
/// fixed arguments on the run folder's `input`.
pub fn media_exec(runs_dir: &Path, instance: &str) -> Result<(), String> {
    if instance.contains('/') || instance.starts_with('.') {
        return Err(format!("{instance:?} is not a run"));
    }
    run_ffmpeg(&runs_dir.join(instance))
}

/// Reads `job.json` and `input` in `folder` and writes the frames,
/// `audio.ogg`, and `result.json`.
pub fn run_ffmpeg(folder: &Path) -> Result<(), String> {
    let job: Job = fs::read(folder.join("job.json"))
        .map_err(|error| error.to_string())
        .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|error| error.to_string()))?;
    let input = folder.join("input");
    let ffmpeg = binary(FFMPEG_VARIABLE, "ffmpeg");
    let ffprobe = binary(FFPROBE_VARIABLE, "ffprobe");
    let duration = Command::new(&ffprobe)
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
        .arg(&input)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8_lossy(&output.stdout).trim().parse::<f64>().ok())
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0);
    let mut found = Found {
        duration,
        ..Found::default()
    };
    if job.frames {
        for (index, at) in frame_times(duration).into_iter().enumerate() {
            let name = format!("frame-{index}.jpg");
            let ok = Command::new(&ffmpeg)
                .args(["-v", "error", "-nostdin", "-y", "-ss"])
                .arg(format!("{at:.3}"))
                .arg("-i")
                .arg(&input)
                .args(["-frames:v", "1", "-q:v", "3"])
                .arg(folder.join(&name))
                .status()
                .is_ok_and(|status| status.success());
            if ok && folder.join(&name).is_file() {
                found.frames.push(name);
            }
        }
    }
    found.audio = Command::new(&ffmpeg)
        .args(["-v", "error", "-nostdin", "-y", "-i"])
        .arg(&input)
        .args(["-vn", "-ac", "1", "-c:a", "libopus", "-b:a", "24k", "-f", "ogg"])
        .arg(folder.join("audio.ogg"))
        .status()
        .is_ok_and(|status| status.success())
        && fs::metadata(folder.join("audio.ogg")).is_ok_and(|meta| meta.len() > 0);
    fs::write(
        folder.join("result.json"),
        serde_json::to_vec(&found).expect("a result serializes"),
    )
    .map_err(|error| error.to_string())
}

/// When to take frames: up to eight, spread evenly over the length and at
/// least a second apart, or one at the start when the length is unknown.
pub fn frame_times(duration: Option<f64>) -> Vec<f64> {
    let Some(duration) = duration.filter(|seconds| *seconds > 0.0) else {
        return vec![0.0];
    };
    let count = (duration.floor() as usize).clamp(1, MAX_FRAMES);
    (0..count)
        .map(|i| duration * (2 * i + 1) as f64 / (2 * count) as f64)
        .collect()
}

/// The media unit, started through systemd like the command units.
pub struct UnitRunner {
    pub runs_dir: PathBuf,
}

impl VideoRunner for UnitRunner {
    fn extract<'a>(
        &'a self,
        input: Vec<u8>,
        job: Job,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<Extracted, String>> {
        Box::pin(async move {
            let instance = shell::next_instance();
            let folder = self.runs_dir.join(&instance);
            prepare_folder(&folder, &input, job)?;
            let unit = format!("{TEMPLATE}@{instance}.service");
            let result = shell::run_to_end(&unit, LIMIT, &cancel).await;
            let extracted = result.and_then(|()| collect(&folder));
            let _ = fs::remove_dir_all(&folder);
            extracted
        })
    }
}

/// FFmpeg as a plain child process, without isolation. Only for tests and
/// development outside the container.
pub struct DirectRunner;

impl VideoRunner for DirectRunner {
    fn extract<'a>(
        &'a self,
        input: Vec<u8>,
        job: Job,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<Extracted, String>> {
        Box::pin(async move {
            let root = tempfile_dir()?;
            let folder = root.join("run");
            prepare_folder(&folder, &input, job)?;
            let work = folder.clone();
            let run = tokio::task::spawn_blocking(move || run_ffmpeg(&work));
            let result = tokio::select! {
                () = cancel.cancelled() => Err("stopped".to_owned()),
                () = tokio::time::sleep(LIMIT) => Err("FFmpeg took more than 5 minutes".to_owned()),
                joined = run => joined.map_err(|error| error.to_string()).and_then(|result| result),
            };
            let extracted = result.and_then(|()| collect(&folder));
            let _ = fs::remove_dir_all(&root);
            extracted
        })
    }
}

fn tempfile_dir() -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join(format!(
        "alyosha-media-{}-{}",
        std::process::id(),
        crate::environment::random_suffix()
    ));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|error| format!("could not create {}: {error}", dir.display()))?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_spread_and_a_second_apart() {
        assert_eq!(frame_times(None), vec![0.0]);
        assert_eq!(frame_times(Some(0.5)), vec![0.25]);
        assert_eq!(frame_times(Some(3.0)), vec![0.5, 1.5, 2.5]);
        let long = frame_times(Some(80.0));
        assert_eq!(long.len(), 8);
        assert_eq!(long[0], 5.0);
        assert_eq!(long[7], 75.0);
    }

    /// With FFmpeg on `PATH` (or `ALYOSHA_FFMPEG`): a three-second clip
    /// with sound gives three frames and Opus sound.
    #[test]
    fn real_ffmpeg_takes_frames_and_sound() {
        let ffmpeg = binary(FFMPEG_VARIABLE, "ffmpeg");
        if Command::new(&ffmpeg).arg("-version").output().is_err() {
            eprintln!("skipped: ffmpeg is not on PATH");
            return;
        }
        let dir = tempfile_dir().unwrap();
        let clip = dir.join("clip.mp4");
        let made = Command::new(&ffmpeg)
            .args(["-v", "error", "-f", "lavfi", "-i", "testsrc=duration=3:size=320x240:rate=10"])
            .args(["-f", "lavfi", "-i", "sine=duration=3", "-shortest", "-pix_fmt", "yuv420p"])
            .arg(&clip)
            .status()
            .unwrap();
        assert!(made.success());
        let folder = dir.join("run");
        prepare_folder(&folder, &fs::read(&clip).unwrap(), Job { frames: true }).unwrap();
        run_ffmpeg(&folder).unwrap();
        let extracted = collect(&folder).unwrap();
        assert!((extracted.duration.unwrap() - 3.0).abs() < 0.2, "{:?}", extracted.duration);
        assert_eq!(extracted.frames.len(), 3);
        assert!(super::super::image::format(&extracted.frames[0]).is_some());
        assert!(super::super::ogg::is_ogg_opus(&extracted.audio.unwrap()));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_result_is_read_back() {
        let dir = tempfile_dir().unwrap();
        fs::write(dir.join("frame-0.jpg"), b"jpg").unwrap();
        fs::write(dir.join("audio.ogg"), b"ogg").unwrap();
        fs::write(
            dir.join("result.json"),
            br#"{"duration": 2.5, "frames": ["frame-0.jpg"], "audio": true}"#,
        )
        .unwrap();
        let extracted = collect(&dir).unwrap();
        assert_eq!(extracted.duration, Some(2.5));
        assert_eq!(extracted.frames, vec![b"jpg".to_vec()]);
        assert_eq!(extracted.audio, Some(b"ogg".to_vec()));
        fs::write(dir.join("result.json"), br#"{"frames": ["../x"]}"#).unwrap();
        assert!(collect(&dir).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
