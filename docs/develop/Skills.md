# Implement Skills

Scan the skills directory once at session startup. Add the skills index to the normal system instruction and register the `skill` tool for direct model calls. Expose the directory read-only to direct `read_file` calls and to the shell sandbox at `/skills`. [Skills design](../design/Skills.md) · [Architecture overview](../README.md)

## V1 store contract

```text
SkillStore
    open(root: &Path) -> SkillStore
    index(&self) -> SkillIndex
    list(&self) -> Vec<SkillEntry>
    load(&self, name: &str) -> Result<LoadedSkill, SkillError>

SkillEntry  { name: String, description: String }
SkillIndex  { text: String, omitted: usize, invalid: Vec<String> }
LoadedSkill { body: String, files: Vec<String>, more_files: usize }
```

- **Location.** The root is `${XDG_CONFIG_HOME:-$HOME/.config}/alyosha/skills`. A missing root is an empty store. Alyosha never creates or writes it.
- **Discovery.** Each immediate subdirectory containing a regular file `SKILL.md` is a candidate. Skip symlinked subdirectories and ignore other entries.
- **Names.** A name matches `[a-z0-9]+(-[a-z0-9]+)*` and is at most 64 bytes. The `name` field must equal the directory name.
- **Header.** `SKILL.md` starts with a front matter block holding `name` and a nonempty `description` of at most 1,024 bytes. Ignore other fields, including `allowed-tools`, `license`, and `metadata`. A candidate whose header is missing or invalid goes into `invalid` and stays out of the index and `list`.
- **Reading at call time.** `index` reflects the scan at startup. `load` and `list` read the disk when called, so edits to an existing skill take effect on the next load.

## Build the index

Sort entries by name. Emit one line per entry, `- <name>: <description>`, while the text fits a 4 KiB budget. Count the rest in `omitted`. If anything was omitted, end with `N more skills are not shown. Use skill with no name to list them.`

Append this section after the memory section of the normal system instruction. [Memory index](Memory.md#build-the-index)

```text
## Skills
Skills are procedures the owner wrote. When a task matches a skill's description,
load it with the skill tool before starting, then follow it. A skill's other files are
under /skills/<name>/. Read them with read_file and run its scripts with run_command.

<index lines, or "No skills installed.">
```

If `invalid` is nonempty, show **N skills could not be loaded** in the status area with their directory names. The Context Manager counts the section as system instructions. [Context Manager budgets](ContextManager.md#v1-token-budgets)

## `skill` tool

| Input | Successful output |
| --- | --- |
| no `name` | One line per valid skill, `- <name>: <description>`, without the index budget |
| `name` | A header naming `/skills/<name>/SKILL.md` and the skill's other files, followed by the body of `SKILL.md` without its front matter |

For a loaded skill, list the other regular files under the skill directory as `/skills/<name>/<relative path>`, sorted, up to 100 entries. Skip symlinks and count the rest in `more_files`. Put the header before the body, so the file list survives if the result preview is shortened. If the body is cut, the model continues with `read_file` on the `SKILL.md` path. [Tool result limits](ContextManager.md#v1-tool-result-limits)

```text
Skill: blog-release (/skills/blog-release/SKILL.md)
Files:
/skills/blog-release/reference/style.md
/skills/blog-release/scripts/check-links.sh

1. Run /skills/blog-release/scripts/check-links.sh on the draft.
...
```

Return a tool error for an invalid name, a missing skill, or an invalid header. Open files relative to the skills root without following symlinks out of it.

**Scope.** The session `ToolContext` carries the store handle. The tool is available to direct model calls only. The Code Mode host never exposes it. [Tool permissions](../reference/ToolPermissions.md#skills-directory)

## `/skills` paths

- **`read_file`.** In `open_file` with `Read`, resolve an absolute path under `/skills/` against the skills root. Permit regular files only and reject a path that resolves outside the root, including through a symlink. `ReadWorkspace` and `replace_file` never resolve `/skills`, so `edit_file` and `write_file` return a tool error for those paths. The Code Mode scope rejects them. [Handler interface](EnvironmentInterface.md#handler-interface)
- **Shell.** If the root exists, add `--ro-bind <root> /skills` to the Bubblewrap arguments. Otherwise leave `/skills` absent. [Bubblewrap launch contract](EnvironmentInterface.md#bubblewrap-launch-contract)

## Verification

Run these against a temporary skills root.

- Start with no root. The index reads **No skills installed.** and `skill` with no name returns an empty list.
- Install `blog-release` with a script and a reference file. Check its index line, then load it and check the header, file list, and body without front matter.
- Add skills whose header is missing, whose `name` differs from the directory, and whose name is `Bad_Name`. Check that each is left out of the index and counted in the startup warning.
- Overflow the 4 KiB index. Check the omitted count, and that `skill` with no name still lists every valid skill.
- Load a skill whose body exceeds the result limit. Check that the file list survives the cut and that `read_file` on the `SKILL.md` path returns the rest.
- Read `/skills/blog-release/reference/style.md` with a direct `read_file`. Then try `write_file` and `edit_file` on it, a `/skills` symlink pointing outside the root, and the same read from an `execute_code` script. Check that only the first succeeds.
- Run `/skills/blog-release/scripts/check-links.sh` with `run_command`. Check that it runs, and that `touch /skills/x` fails inside the sandbox.
- Edit a skill's body during a session. Check that the next load returns the new body and that the system instruction stays byte-identical.

[Architecture overview](../README.md)
