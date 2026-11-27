//! Environment Interface: how tool handlers reach files and the shell.
//!
//! The workspace selected at session startup sets the working directory, the
//! file permissions, and the shell sandbox. Every rule is enforced at the
//! point of access: files open beneath the workspace with `openat2`'s
//! `RESOLVE_BENEATH`, so no path or symlink can lead out of it. See
//! `docs/EnvironmentInterface.md#implementation` and
//! `docs/reference/ToolPermissions.md`.

pub mod logs;
pub mod shell;

use std::{
    ffi::OsString,
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsFd, OwnedFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use rustix::{
    fs::{AtFlags, Mode, OFlags, ResolveFlags},
    io::Errno,
};

pub use logs::{LogCapture, SessionLogs};
pub use shell::{CommandOutput, SandboxConfig, ShellStatus};

use crate::tools::{Scope, ToolError};

/// The model-facing absolute path of the workspace.
pub const WORKSPACE_MOUNT: &str = "/workspace";
/// The model-facing path of the skills directory.
pub const SKILLS_MOUNT: &str = "/skills";
/// The model-facing prefix of this session's logs.
pub const LOGS_PREFIX: &str = "/session-logs";

/// How a handler opens a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileAccess {
    /// Read a workspace file, a session log, or a file under `/skills`.
    Read,
    /// Read a workspace file only, for `edit_file`.
    ReadWorkspace,
}

/// How `replace_file` treats the destination.
#[derive(Debug, Clone, Copy)]
pub enum ReplaceMode<'a> {
    CreateOrReplace,
    /// Replace only if the file still matches what was read.
    ReplaceExisting(&'a FileSnapshot),
}

/// A file's identity and contents when it was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSnapshot {
    dev: u64,
    ino: u64,
    pub content: Vec<u8>,
}

/// An authorized read handle.
#[derive(Debug)]
pub struct ScopedFile {
    pub file: File,
    pub len: u64,
    dev: u64,
    ino: u64,
}

impl ScopedFile {
    /// Reads the whole file and remembers its identity for `ReplaceExisting`.
    pub fn snapshot(mut self) -> Result<FileSnapshot, ToolError> {
        let mut content = Vec::new();
        self.file
            .read_to_end(&mut content)
            .map_err(|error| ToolError::failed(format!("Could not read the file: {error}")))?;
        Ok(FileSnapshot {
            dev: self.dev,
            ino: self.ino,
            content,
        })
    }
}

/// The session's environment: its workspace, skills, logs, and sandbox.
#[derive(Debug)]
pub struct Environment {
    /// The workspace's real path on the host.
    pub workspace: PathBuf,
    workspace_fd: OwnedFd,
    /// The session's working directory, relative to the workspace.
    pub cwd: PathBuf,
    pub skills_root: Option<PathBuf>,
    pub logs: Arc<SessionLogs>,
    pub sandbox: SandboxConfig,
}

/// Where a model-facing path points.
enum Target {
    Workspace(PathBuf),
    Skills(PathBuf),
    Log(String),
}

impl Environment {
    /// Opens the workspace, which must be an existing directory.
    pub fn new(
        workspace: &Path,
        skills_root: Option<PathBuf>,
        logs: Arc<SessionLogs>,
        sandbox: SandboxConfig,
    ) -> std::io::Result<Self> {
        let workspace = workspace.canonicalize()?;
        let workspace_fd = rustix::fs::open(
            &workspace,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        Ok(Self {
            workspace,
            workspace_fd,
            cwd: PathBuf::new(),
            skills_root: skills_root.filter(|root| root.is_dir()),
            logs,
            sandbox,
        })
    }

    fn target(&self, path: &str) -> Result<Target, ToolError> {
        if path.is_empty() {
            return Err(ToolError::failed("The path is empty."));
        }
        let path = Path::new(path);
        let relative = if let Ok(rest) = path.strip_prefix(WORKSPACE_MOUNT) {
            rest.to_owned()
        } else if let Ok(rest) = path.strip_prefix(SKILLS_MOUNT) {
            return Ok(Target::Skills(clean_relative(rest)?));
        } else if let Ok(rest) = path.strip_prefix(LOGS_PREFIX) {
            return Ok(Target::Log(rest.to_string_lossy().into_owned()));
        } else if path.is_absolute() {
            return Err(ToolError::failed(format!(
                "{} is outside the workspace. Use a relative path or one under {WORKSPACE_MOUNT}.",
                path.display()
            )));
        } else {
            self.cwd.join(path)
        };
        Ok(Target::Workspace(clean_relative(&relative)?))
    }

    /// Opens a file for reading under the caller's scope.
    pub fn open_file(
        &self,
        path: &str,
        access: FileAccess,
        scope: Scope,
    ) -> Result<ScopedFile, ToolError> {
        let target = self.target(path)?;
        let (base, relative): (&OwnedFd, PathBuf) = match target {
            Target::Workspace(relative) => (&self.workspace_fd, relative),
            Target::Skills(relative) => {
                if access != FileAccess::Read || scope != Scope::Direct {
                    return Err(ToolError::failed(format!(
                        "Files under {SKILLS_MOUNT} are read-only."
                    )));
                }
                let Some(root) = &self.skills_root else {
                    return Err(ToolError::failed(format!("{path}: no such file")));
                };
                let file = open_beneath_path(root, &relative, path)?;
                return regular(file, path);
            }
            Target::Log(name) => {
                if access != FileAccess::Read || scope != Scope::Direct {
                    return Err(ToolError::failed(format!("{path} cannot be opened here.")));
                }
                let file = self.logs.open(&name).map_err(|_| {
                    ToolError::failed(format!(
                        "{path} is gone. Saved output is removed when older conversation is summarized or Alyosha restarts."
                    ))
                })?;
                return regular(file, path);
            }
        };
        let fd = open_beneath(
            base,
            &relative,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
        )
        .map_err(|errno| open_error(errno, path))?;
        regular(File::from(fd), path)
    }

    /// Atomically creates or replaces a workspace file with `content`.
    pub fn replace_file(
        &self,
        path: &str,
        content: &[u8],
        mode: ReplaceMode<'_>,
    ) -> Result<(), ToolError> {
        let relative = match self.target(path)? {
            Target::Workspace(relative) => relative,
            Target::Skills(_) => {
                return Err(ToolError::failed(format!(
                    "Files under {SKILLS_MOUNT} are read-only."
                )));
            }
            Target::Log(_) => return Err(ToolError::failed(format!("{path} is read-only."))),
        };
        // Follow a symlink at the destination, inside the workspace only.
        let relative = match open_beneath(
            &self.workspace_fd,
            &relative,
            OFlags::PATH | OFlags::CLOEXEC,
        ) {
            Ok(fd) => self.real_relative(&fd, path)?,
            Err(Errno::NOENT) => {
                if matches!(mode, ReplaceMode::ReplaceExisting(_)) {
                    return Err(ToolError::failed(format!("{path} no longer exists.")));
                }
                relative
            }
            Err(errno) => return Err(open_error(errno, path)),
        };
        let name = relative
            .file_name()
            .ok_or_else(|| ToolError::failed(format!("{path} is not a file path.")))?
            .to_owned();
        let parent_path = relative.parent().unwrap_or(Path::new(""));
        let parent = open_beneath(
            &self.workspace_fd,
            parent_path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        )
        .map_err(|errno| match errno {
            Errno::NOENT => ToolError::failed(format!(
                "The folder for {path} does not exist. Create it first, for example with mkdir -p."
            )),
            other => open_error(other, path),
        })?;

        let existing =
            rustix::fs::statat(&parent, name.as_os_str(), AtFlags::SYMLINK_NOFOLLOW).ok();
        if let Some(stat) = &existing
            && rustix::fs::FileType::from_raw_mode(stat.st_mode)
                != rustix::fs::FileType::RegularFile
        {
            return Err(ToolError::failed(format!("{path} is not a regular file.")));
        }
        let permissions = existing
            .as_ref()
            .map_or(0o644, |stat| stat.st_mode & 0o7777);

        let mut staging_name = OsString::from(".");
        staging_name.push(&name);
        staging_name.push(format!(".alyosha-{}.tmp", random_suffix()));
        let staging = rustix::fs::openat(
            &parent,
            staging_name.as_os_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|errno| ToolError::failed(format!("Could not stage {path}: {errno}")))?;
        let discard = |message: String| {
            let _ = rustix::fs::unlinkat(&parent, staging_name.as_os_str(), AtFlags::empty());
            ToolError::failed(message)
        };
        let mut file = File::from(staging);
        if let Err(error) = file.write_all(content) {
            return Err(discard(format!("Could not write {path}: {error}")));
        }
        if let Err(errno) = rustix::fs::fchmod(&file, Mode::from_raw_mode(permissions)) {
            return Err(discard(format!(
                "Could not set the mode of {path}: {errno}"
            )));
        }
        if let Err(error) = file.sync_all() {
            return Err(discard(format!("Could not write {path}: {error}")));
        }
        drop(file);

        if let ReplaceMode::ReplaceExisting(snapshot) = mode {
            let unchanged = rustix::fs::openat(
                &parent,
                name.as_os_str(),
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
                Mode::empty(),
            )
            .ok()
            .map(File::from)
            .is_some_and(|mut current| {
                let Ok(meta) = current.metadata() else {
                    return false;
                };
                let mut content = Vec::new();
                meta.dev() == snapshot.dev
                    && meta.ino() == snapshot.ino
                    && current.read_to_end(&mut content).is_ok()
                    && content == snapshot.content
            });
            if !unchanged {
                return Err(discard(format!(
                    "{path} changed after it was read. Read it again and retry."
                )));
            }
        }
        rustix::fs::renameat(&parent, staging_name.as_os_str(), &parent, name.as_os_str())
            .map_err(|errno| discard(format!("Could not replace {path}: {errno}")))
    }

    /// Creates a folder directly in the workspace, such as `inbox`, unless
    /// it already exists there.
    pub fn create_dir(&self, name: &str) -> Result<(), ToolError> {
        if name.is_empty() || name.contains('/') || name.starts_with('.') {
            return Err(ToolError::failed(format!("{name} is not a folder name.")));
        }
        match rustix::fs::mkdirat(&self.workspace_fd, name, Mode::from_raw_mode(0o2770)) {
            Ok(()) | Err(Errno::EXIST) => {}
            Err(errno) => return Err(ToolError::failed(format!("Could not create {name}: {errno}"))),
        }
        open_beneath(
            &self.workspace_fd,
            Path::new(name),
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        )
        .map(|_| ())
        .map_err(|errno| open_error(errno, name))
    }

    /// The path of an opened workspace file relative to the workspace.
    fn real_relative(&self, fd: &OwnedFd, path: &str) -> Result<PathBuf, ToolError> {
        let link = format!("/proc/self/fd/{}", std::os::fd::AsRawFd::as_raw_fd(fd));
        let real = std::fs::read_link(link)
            .map_err(|error| ToolError::failed(format!("Could not resolve {path}: {error}")))?;
        real.strip_prefix(&self.workspace)
            .map(Path::to_owned)
            .map_err(|_| ToolError::failed(format!("{path} resolves outside the workspace.")))
    }

    /// The model-facing absolute path of a workspace-relative path.
    pub fn display_path(relative: &Path) -> String {
        Path::new(WORKSPACE_MOUNT)
            .join(relative)
            .display()
            .to_string()
    }
}

/// Rejects `..` and makes a path relative.
fn clean_relative(path: &Path) -> Result<PathBuf, ToolError> {
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => clean.push(part),
            Component::CurDir | Component::RootDir => {}
            Component::ParentDir => {
                if !clean.pop() {
                    return Err(ToolError::failed(format!(
                        "{} leads outside the workspace.",
                        path.display()
                    )));
                }
            }
            Component::Prefix(_) => return Err(ToolError::failed("Unsupported path.")),
        }
    }
    Ok(clean)
}

fn open_beneath(base: &OwnedFd, relative: &Path, flags: OFlags) -> Result<OwnedFd, Errno> {
    let relative: &Path = if relative.as_os_str().is_empty() {
        Path::new(".")
    } else {
        relative
    };
    rustix::fs::openat2(
        base.as_fd(),
        relative.as_os_str().as_bytes(),
        flags,
        Mode::empty(),
        ResolveFlags::BENEATH | ResolveFlags::NO_MAGICLINKS,
    )
}

fn open_beneath_path(root: &Path, relative: &Path, shown: &str) -> Result<File, ToolError> {
    let base = rustix::fs::open(
        root,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|errno| open_error(errno, shown))?;
    open_beneath(
        &base,
        relative,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOCTTY,
    )
    .map(File::from)
    .map_err(|errno| open_error(errno, shown))
}

fn open_error(errno: Errno, path: &str) -> ToolError {
    ToolError::failed(match errno {
        Errno::NOENT => format!("{path}: no such file"),
        Errno::XDEV => format!("{path} resolves outside the allowed folder."),
        Errno::ACCESS | Errno::PERM => format!("{path}: permission denied"),
        Errno::NOTDIR => format!("{path}: a parent is not a folder"),
        Errno::LOOP => format!("{path}: too many symlinks"),
        other => format!("{path}: {other}"),
    })
}

fn regular(file: File, path: &str) -> Result<ScopedFile, ToolError> {
    let meta = file
        .metadata()
        .map_err(|error| ToolError::failed(format!("{path}: {error}")))?;
    if !meta.is_file() {
        return Err(ToolError::failed(if meta.is_dir() {
            format!("{path} is a folder. List folders with run_command.")
        } else {
            format!("{path} is not a regular file.")
        }));
    }
    Ok(ScopedFile {
        len: meta.len(),
        dev: meta.dev(),
        ino: meta.ino(),
        file,
    })
}

pub(crate) fn random_suffix() -> String {
    let mut bytes = [0u8; 6];
    let _ = getrandom::fill(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt};

    use super::*;

    fn environment(root: &Path) -> Environment {
        let workspace = root.join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let skills = root.join("skills");
        fs::create_dir_all(skills.join("blog/reference")).unwrap();
        fs::write(skills.join("blog/reference/style.md"), "style").unwrap();
        let logs = Arc::new(SessionLogs::new(root.join("logs")));
        Environment::new(&workspace, Some(skills), logs, SandboxConfig::default()).unwrap()
    }

    fn read(env: &Environment, path: &str, scope: Scope) -> Result<String, ToolError> {
        let mut file = env.open_file(path, FileAccess::Read, scope)?.file;
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        Ok(text)
    }

    #[test]
    fn reads_stay_inside_the_workspace() {
        let root = tempfile::tempdir().unwrap();
        let env = environment(root.path());
        fs::write(root.path().join("ws/notes.md"), "inside").unwrap();
        fs::write(root.path().join("secret.txt"), "outside").unwrap();
        std::os::unix::fs::symlink(
            root.path().join("secret.txt"),
            root.path().join("ws/escape"),
        )
        .unwrap();
        std::os::unix::fs::symlink("notes.md", root.path().join("ws/alias")).unwrap();

        assert_eq!(read(&env, "notes.md", Scope::Direct).unwrap(), "inside");
        assert_eq!(
            read(&env, "/workspace/notes.md", Scope::Direct).unwrap(),
            "inside"
        );
        assert_eq!(read(&env, "alias", Scope::Direct).unwrap(), "inside");
        assert!(read(&env, "escape", Scope::Direct).is_err());
        assert!(read(&env, "../secret.txt", Scope::Direct).is_err());
        assert!(
            read(
                &env,
                &root.path().join("secret.txt").display().to_string(),
                Scope::Direct
            )
            .is_err()
        );
        assert!(
            read(&env, ".", Scope::Direct).is_err(),
            "a folder is not a file"
        );
    }

    #[test]
    fn skills_are_read_only_and_direct_only() {
        let root = tempfile::tempdir().unwrap();
        let env = environment(root.path());
        assert_eq!(
            read(&env, "/skills/blog/reference/style.md", Scope::Direct).unwrap(),
            "style"
        );
        assert!(read(&env, "/skills/blog/reference/style.md", Scope::CodeMode).is_err());
        assert!(
            env.open_file(
                "/skills/blog/reference/style.md",
                FileAccess::ReadWorkspace,
                Scope::Direct
            )
            .is_err()
        );
        assert!(
            env.replace_file("/skills/blog/x.md", b"x", ReplaceMode::CreateOrReplace)
                .is_err()
        );
        std::os::unix::fs::symlink(root.path().join("ws"), root.path().join("skills/blog/out"))
            .unwrap();
        assert!(read(&env, "/skills/blog/out/anything", Scope::Direct).is_err());
    }

    #[test]
    fn replacement_is_atomic_and_keeps_the_mode() {
        let root = tempfile::tempdir().unwrap();
        let env = environment(root.path());
        let path = root.path().join("ws/run.sh");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();

        env.replace_file("run.sh", b"new", ReplaceMode::CreateOrReplace)
            .unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o755
        );

        env.replace_file("fresh.txt", b"made", ReplaceMode::CreateOrReplace)
            .unwrap();
        assert_eq!(
            fs::read_to_string(root.path().join("ws/fresh.txt")).unwrap(),
            "made"
        );
        assert!(
            env.replace_file("missing/x.txt", b"x", ReplaceMode::CreateOrReplace)
                .is_err()
        );

        let leftovers: Vec<_> = fs::read_dir(root.path().join("ws"))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn a_symlinked_destination_replaces_its_workspace_target() {
        let root = tempfile::tempdir().unwrap();
        let env = environment(root.path());
        fs::write(root.path().join("ws/real.txt"), "old").unwrap();
        std::os::unix::fs::symlink("real.txt", root.path().join("ws/link.txt")).unwrap();
        env.replace_file("link.txt", b"new", ReplaceMode::CreateOrReplace)
            .unwrap();
        assert_eq!(
            fs::read_to_string(root.path().join("ws/real.txt")).unwrap(),
            "new"
        );
        assert!(
            fs::symlink_metadata(root.path().join("ws/link.txt"))
                .unwrap()
                .file_type()
                .is_symlink()
        );

        fs::write(root.path().join("outside.txt"), "keep").unwrap();
        std::os::unix::fs::symlink(
            root.path().join("outside.txt"),
            root.path().join("ws/out.txt"),
        )
        .unwrap();
        assert!(
            env.replace_file("out.txt", b"x", ReplaceMode::CreateOrReplace)
                .is_err()
        );
        assert_eq!(
            fs::read_to_string(root.path().join("outside.txt")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn a_changed_file_is_not_replaced() {
        let root = tempfile::tempdir().unwrap();
        let env = environment(root.path());
        let path = root.path().join("ws/a.txt");
        fs::write(&path, "one").unwrap();
        let snapshot = env
            .open_file("a.txt", FileAccess::ReadWorkspace, Scope::Direct)
            .unwrap()
            .snapshot()
            .unwrap();
        fs::write(&path, "two").unwrap();
        let error = env
            .replace_file("a.txt", b"three", ReplaceMode::ReplaceExisting(&snapshot))
            .unwrap_err();
        assert!(error.to_string().contains("changed"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "two");
    }
}
