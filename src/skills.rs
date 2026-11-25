//! Skills: procedures the owner wrote, in the Agent Skills layout, in one
//! global directory that Alyosha only reads.
//!
//! The index goes into the system instruction once per session. The `skill`
//! tool loads one skill at call time. See `docs/develop/Skills.md`.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde_json::{Value, json};

use crate::tools::{
    RegisteredTool, Scope, ToolContext, ToolDefinition, ToolError, ToolOutput, handler,
};

pub const INDEX_BUDGET: usize = 4 * 1024;
pub const DESCRIPTION_LIMIT: usize = 1024;
pub const FILE_LIST_LIMIT: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillIndex {
    pub text: String,
    pub omitted: usize,
    pub invalid: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedSkill {
    pub body: String,
    pub files: Vec<String>,
    pub more_files: usize,
}

#[derive(Debug, Clone)]
pub struct SkillStore {
    root: PathBuf,
}

/// Whether a name matches `[a-z0-9]+(-[a-z0-9]+)*` within 64 bytes.
pub fn valid_name(name: &str) -> bool {
    name.len() <= 64
        && !name.is_empty()
        && name.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

/// The `name` and `description` of a `SKILL.md` header, and the body after it.
fn parse_skill(text: &str) -> Option<(String, String, String)> {
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))?;
    let end = rest.find("\n---")?;
    let header = &rest[..end];
    let after = &rest[end + 4..];
    let body = after
        .strip_prefix('\n')
        .or_else(|| after.strip_prefix("\r\n"))
        .unwrap_or(after);
    let mut name = None;
    let mut description = None;
    for line in header.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            continue;
        }
        if let Some(value) = line.strip_prefix("name:") {
            name = Some(unquote(value.trim()));
        } else if let Some(value) = line.strip_prefix("description:") {
            description = Some(unquote(value.trim()));
        }
    }
    Some((name?, description?, body.to_owned()))
}

fn unquote(value: &str) -> String {
    let stripped = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')));
    stripped.unwrap_or(value).to_owned()
}

impl SkillStore {
    /// A missing root is an empty store.
    pub fn open(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The valid skills and the directory names of invalid candidates.
    fn scan(&self) -> (Vec<SkillEntry>, Vec<String>) {
        let mut entries = Vec::new();
        let mut invalid = Vec::new();
        let Ok(listing) = fs::read_dir(&self.root) else {
            return (entries, invalid);
        };
        for item in listing.flatten() {
            let Ok(kind) = item.file_type() else { continue };
            if !kind.is_dir() {
                continue;
            }
            let dir_name = item.file_name().to_string_lossy().into_owned();
            let skill_file = item.path().join("SKILL.md");
            if !fs::symlink_metadata(&skill_file).is_ok_and(|meta| meta.is_file()) {
                continue;
            }
            match self.header(&dir_name) {
                Ok((description, _)) => entries.push(SkillEntry {
                    name: dir_name,
                    description,
                }),
                Err(_) => invalid.push(dir_name),
            }
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        invalid.sort();
        (entries, invalid)
    }

    /// The description and body of a skill, checking its header.
    fn header(&self, name: &str) -> Result<(String, String), ToolError> {
        if !valid_name(name) {
            return Err(ToolError::failed(format!(
                "{name:?} is not a valid skill name."
            )));
        }
        let dir = self.root.join(name);
        if !fs::symlink_metadata(&dir).is_ok_and(|meta| meta.is_dir()) {
            return Err(ToolError::failed(format!(
                "There is no skill named {name}."
            )));
        }
        let path = dir.join("SKILL.md");
        if !fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_file()) {
            return Err(ToolError::failed(format!(
                "There is no skill named {name}."
            )));
        }
        let text = fs::read_to_string(&path)
            .map_err(|error| ToolError::failed(format!("Could not read skill {name}: {error}")))?;
        let (header_name, description, body) = parse_skill(&text)
            .ok_or_else(|| ToolError::failed(format!("Skill {name} has no valid header.")))?;
        if header_name != name {
            return Err(ToolError::failed(format!(
                "Skill {name}'s header names {header_name:?}, not its folder."
            )));
        }
        if description.trim().is_empty() || description.len() > DESCRIPTION_LIMIT {
            return Err(ToolError::failed(format!(
                "Skill {name} needs a description of 1 to 1024 bytes."
            )));
        }
        Ok((description, body))
    }

    pub fn list(&self) -> Vec<SkillEntry> {
        self.scan().0
    }

    pub fn index(&self) -> SkillIndex {
        let (entries, invalid) = self.scan();
        let mut text = String::new();
        let mut shown = 0;
        for entry in &entries {
            let line = format!("- {}: {}\n", entry.name, entry.description);
            if text.len() + line.len() > INDEX_BUDGET {
                break;
            }
            text.push_str(&line);
            shown += 1;
        }
        let omitted = entries.len() - shown;
        if omitted > 0 {
            text.push_str(&format!(
                "{omitted} more skills are not shown. Use skill with no name to list them.\n"
            ));
        }
        SkillIndex {
            text,
            omitted,
            invalid,
        }
    }

    pub fn load(&self, name: &str) -> Result<LoadedSkill, ToolError> {
        let (_, body) = self.header(name)?;
        let mut files = Vec::new();
        collect_files(&self.root.join(name), Path::new(""), &mut files);
        files.retain(|file| file != "SKILL.md");
        files.sort();
        let more_files = files.len().saturating_sub(FILE_LIST_LIMIT);
        files.truncate(FILE_LIST_LIMIT);
        let files = files
            .into_iter()
            .map(|file| format!("/skills/{name}/{file}"))
            .collect();
        Ok(LoadedSkill {
            body,
            files,
            more_files,
        })
    }
}

/// Regular files under `dir`, skipping symlinks.
fn collect_files(dir: &Path, relative: &Path, files: &mut Vec<String>) {
    let Ok(listing) = fs::read_dir(dir) else {
        return;
    };
    for item in listing.flatten() {
        let Ok(kind) = item.file_type() else { continue };
        let path = relative.join(item.file_name());
        if kind.is_dir() {
            collect_files(&item.path(), &path, files);
        } else if kind.is_file() {
            files.push(path.display().to_string());
        }
    }
}

/// The skills section of the normal system instruction.
pub fn instruction_section(index: &SkillIndex) -> String {
    let lines = if index.text.is_empty() {
        "No skills installed.\n".to_owned()
    } else {
        index.text.clone()
    };
    format!(
        "## Skills\nSkills are procedures the owner wrote. When a task matches a skill's description,\nload it with the skill tool before starting, then follow it. A skill's other files are\nunder /skills/<name>/. Read them with read_file and run its scripts with run_command.\n\n{lines}"
    )
}

/// The built-in `skill` tool.
pub fn skill_tool() -> RegisteredTool {
    RegisteredTool {
        definition: ToolDefinition {
            name: "skill".into(),
            description: "List the owner's skills, or load one by name. A loaded skill gives its instructions and the paths of its other files under /skills/<name>/.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "additionalProperties": false
            }),
            requires_approval: false,
        },
        handler: handler(|arguments: Value, context: ToolContext| async move {
            if context.scope != Scope::Direct {
                return Err(ToolError::failed("skill is for direct calls only."));
            }
            let store = context
                .session
                .skills
                .as_ref()
                .ok_or_else(|| ToolError::failed("This session has no skills directory."))?;
            match arguments["name"].as_str() {
                None => {
                    let list: String = store
                        .list()
                        .iter()
                        .map(|entry| format!("- {}: {}\n", entry.name, entry.description))
                        .collect();
                    Ok(ToolOutput::text(if list.is_empty() { "No skills installed.".to_owned() } else { list }))
                }
                Some(name) => {
                    let skill = store.load(name)?;
                    let mut text = format!("Skill: {name} (/skills/{name}/SKILL.md)\n");
                    if !skill.files.is_empty() {
                        text.push_str("Files:\n");
                        for file in &skill.files {
                            text.push_str(file);
                            text.push('\n');
                        }
                        if skill.more_files > 0 {
                            text.push_str(&format!("…and {} more files\n", skill.more_files));
                        }
                    }
                    text.push('\n');
                    text.push_str(&skill.body);
                    Ok(ToolOutput::text(text))
                }
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install(root: &Path, dir: &str, header_name: &str, body: &str) {
        let path = root.join(dir);
        fs::create_dir_all(&path).unwrap();
        fs::write(
            path.join("SKILL.md"),
            format!("---\nname: {header_name}\ndescription: Release a blog post\nlicense: MIT\n---\n{body}"),
        )
        .unwrap();
    }

    #[test]
    fn no_root_is_an_empty_store() {
        let root = tempfile::tempdir().unwrap();
        let store = SkillStore::open(&root.path().join("missing"));
        assert!(instruction_section(&store.index()).ends_with("No skills installed.\n"));
        assert!(store.list().is_empty());
    }

    #[test]
    fn a_skill_loads_with_its_files_and_without_front_matter() {
        let root = tempfile::tempdir().unwrap();
        install(
            root.path(),
            "blog-release",
            "blog-release",
            "1. Run the checks.\n",
        );
        let dir = root.path().join("blog-release");
        fs::create_dir_all(dir.join("reference")).unwrap();
        fs::create_dir_all(dir.join("scripts")).unwrap();
        fs::write(dir.join("reference/style.md"), "s").unwrap();
        fs::write(dir.join("scripts/check-links.sh"), "x").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", dir.join("link")).unwrap();
        let store = SkillStore::open(root.path());
        assert_eq!(store.index().text, "- blog-release: Release a blog post\n");
        let loaded = store.load("blog-release").unwrap();
        assert_eq!(loaded.body, "1. Run the checks.\n");
        assert_eq!(
            loaded.files,
            [
                "/skills/blog-release/reference/style.md",
                "/skills/blog-release/scripts/check-links.sh"
            ]
        );
    }

    #[test]
    fn invalid_skills_stay_out_of_the_index() {
        let root = tempfile::tempdir().unwrap();
        install(root.path(), "good", "good", "x");
        install(root.path(), "mismatch", "other", "x");
        install(root.path(), "Bad_Name", "Bad_Name", "x");
        fs::create_dir_all(root.path().join("no-header")).unwrap();
        fs::write(root.path().join("no-header/SKILL.md"), "just text").unwrap();
        let index = SkillStore::open(root.path()).index();
        assert_eq!(index.text, "- good: Release a blog post\n");
        assert_eq!(index.invalid, ["Bad_Name", "mismatch", "no-header"]);
    }

    #[test]
    fn an_overflowing_index_counts_the_rest() {
        let root = tempfile::tempdir().unwrap();
        for n in 0..300 {
            install(
                root.path(),
                &format!("skill-{n:03}"),
                &format!("skill-{n:03}"),
                "x",
            );
        }
        let store = SkillStore::open(root.path());
        let index = store.index();
        assert!(index.omitted > 0);
        assert!(
            index
                .text
                .ends_with("more skills are not shown. Use skill with no name to list them.\n")
        );
        assert_eq!(store.list().len(), 300);
    }
}
