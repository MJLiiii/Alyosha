# Skills

A skill is a procedure you write down for Alyosha, such as how you release your blog or how to triage a failing NixOS build. Memory keeps facts. Skills keep the steps for doing something. In v1 you write every skill yourself, and the model cannot create or change one. [Memory](Memory.md) · [Architecture overview](../README.md)

## Where skills live

Each skill is a directory in `${XDG_CONFIG_HOME:-$HOME/.config}/alyosha/skills/`. The directory holds a `SKILL.md` file and, optionally, scripts and reference files the skill uses.

```text
~/.config/alyosha/skills/
  blog-release/
    SKILL.md
    scripts/check-links.sh
    reference/style.md
```

`SKILL.md` starts with a short header giving the skill's name and a description of when to use it. The body holds the instructions.

```markdown
---
name: blog-release
description: Publish a draft post on the blog. Use when the owner asks to release, publish, or ship a post.
---
1. Run /skills/blog-release/scripts/check-links.sh on the draft.
2. Follow reference/style.md for the front matter.
...
```

This is the Agent Skills layout, so a skill written for another agent usually works in Alyosha unchanged. The store is global. Every workspace sees the same skills. Per-workspace skills are deferred because a repository's files are not necessarily trusted. [Agent Skills specification](https://agentskills.io/specification)

## What the model sees

When a session starts, Alyosha reads the skills directory and builds an **index** with one line per skill: its name and description. The index is appended to the system instruction after the memory index, along with a short note on using skills. Like the memory index, it stays fixed for the session. A skill added during the session appears in the next one, which for a chat means after its nightly fresh start. [Default instruction](LLMClientGateway.md#default-instruction-for-normal-requests) · [Memory index](Memory.md#what-the-model-sees)

When a task matches a description, the model loads the skill with the `skill` tool. The tool returns the instructions and a list of the skill's other files. For example, you ask “publish the post about Nix flakes.” The model sees `blog-release` in the index, loads it, and follows its steps. [Built-in tool contracts](../reference/BuiltInTools.md#skill)

A skill with a missing or invalid header is left out of the index, and the TUI shows a warning at startup.

## Bundled files

The skills directory is visible read-only at `/skills` in two places:

- Direct `read_file` calls accept `/skills/<name>/...` paths, so the model can read a skill's reference files.
- The shell sandbox mounts the directory read-only at `/skills`, so `run_command` can run a skill's scripts.

Nothing can write under `/skills`. A script runs under the usual shell rules: it can write the workspace, it can reach the internet, and it can use only the shell's tools, which come from the workspace's dev shell or the default set. A script that needs another interpreter, such as Python, needs it in that dev shell or in the default set. Code Mode scripts cannot read `/skills`. [Tool permissions](../reference/ToolPermissions.md#skills-directory) · [Shell sandbox](EnvironmentInterface.md#shell-commands)

## Trust

Skills are instructions you wrote, so the model treats them like your own requests. They are not untrusted content. Keep anything you would not run yourself out of the skills directory.

## Deferred

- Skills the model writes or edits.
- Per-workspace skills.
- Invoking a skill directly from the TUI, such as `/blog-release`.
- Honoring the optional `allowed-tools` header field.
- Access from Code Mode.

[Skills implementation guide](../develop/Skills.md)
