# Deployment

Alyosha runs inside a NixOS container on your machine. The container holds the whole agent: the TUI, the Agent Loop, GPROXY, and the MCP servers. Inside it, each shell command still gets its own Bubblewrap sandbox. The container limits what the agent as a whole can reach, and Bubblewrap limits what the model's shell can reach. [Architecture overview](../README.md) · [Environment Interface](EnvironmentInterface.md)

```text
host (NixOS)
└─ containers.alyosha               declared in your host configuration
   │  sees: ~/work, ~/.config/alyosha, ~/.local/share/alyosha
   │  network: private, internet through the host
   │
   └─ your user (not root)
      ├─ alyosha (TUI + agent)
      ├─ alyosha-telegram service ─► alyosha acp (agent)
      ├─ alyosha-qq service ───────► alyosha acp (agent)
      ├─ gproxy service ◄── every alyosha process
      ├─ MCP servers
      └─ bwrap for each run_command      sees /workspace, /skills, the store read-only, and its cache
```

## Why a container around the whole agent

Without the container, Bubblewrap protects your machine only from shell commands. MCP servers run as ordinary processes and can reach anything your user can. A bug in Alyosha's own path checks could too. Inside the container, all of them see only the folders you mount and have no route to services on your host's `localhost`. [Where MCP servers run](MCP.md#where-servers-run)

Bubblewrap stays inside the container because the two layers do different jobs. GPROXY's credentials, the memory store, and session logs live in the container, and the model's shell must still not see them. [Shell commands](EnvironmentInterface.md#shell-commands)

## What the container is

A NixOS container is a `systemd-nspawn` machine declared in the host's NixOS configuration. It runs its own systemd, users, and services on the host's kernel. `nixos-rebuild switch` builds it like the rest of the system, so there is no image to download.

## Packages from the Nix store

The container does not get its own copy of the Nix store. It shares the host's `/nix/store` read-only, which is why it costs almost no disk space. Packages reach each layer as follows:

```text
host /nix/store                  every package the host has built or downloaded
└─ container                     sees all of it, read-only; only declared packages are on PATH
   └─ bwrap (run_command)        sees all of it, read-only; PATH from the project's dev shell or the default tools
```

- **The container sees the whole store but installs only what you declare.** Its configuration puts Alyosha, GPROXY, and the commands of your MCP servers on `PATH`. Other packages are on disk, and a program can still run one by its full store path.
- **Programs in the container can add packages.** NixOS gives the container the host's Nix daemon socket, so `nix shell` or `nix build` inside the container asks the host daemon. New packages land in the shared store, where the host sees them too. Nothing in the container can change a path that already exists. Removing this access is deferred.
- **The model's shell cannot add packages.** Bubblewrap mounts the store read-only, without the daemon socket. Its tools come from the project's Nix dev shell, which Alyosha loads outside the sandbox at session start, or from a small default list. To give the shell a new tool, add it to the project's `flake.nix` and start a new session. For projects without a flake, add it to Alyosha's default tool list and rebuild with `nixos-rebuild switch`. [Bubblewrap launch contract](../develop/EnvironmentInterface.md#bubblewrap-launch-contract)
- **Secrets never go in the store.** Everything in `/nix/store` is readable by every user and by the container. Keep tokens and keys in `~/.config/alyosha` and never put them directly in a Nix expression.

## What it can see

The container mounts three host folders writable, each at the same path it has on the host:

- The workspace folders, `~/work` by default, which hold every project. Alyosha accepts any folder as a workspace, so this mount is the real limit: nothing else of your home exists in the container.
- `~/.config/alyosha`, with the configuration file, skills, and GPROXY's secrets.
- `~/.local/share/alyosha`, with memory, GPROXY's database, the shell's download cache, and Alyosha's log files. [Logging](../reference/Logging.md)

Using the same paths keeps file names identical on both sides, so a path in a memory, a log line, or an error means the same thing on the host. You edit skills and memories with your usual tools on the host, and they survive a rebuild of the container. Everything else on the host, such as your home directory, `~/.ssh`, and other projects, does not exist inside.

The container has its own network with a route to the internet through the host. GPROXY and MCP servers can reach their services. Services listening on the host's `localhost` are out of reach.

## Limits on memory and CPU

The container has a memory cap, set in the host configuration. When everything inside together uses more, the kernel stops processes inside the container instead of freezing your machine. A bridge whose agent is stopped this way restarts with a fresh session. Shell commands and background jobs also run at a lower CPU priority, so a long build leaves the host responsive. [Container module](../develop/Deployment.md#host-module)

## Using it

The host starts the container at boot. To use Alyosha, run `alyosha` in any terminal on the host, in a folder under `~/work`: the host's `alyosha` is a small launcher that runs the container's `alyosha` in the same folder through `machinectl shell`, so it feels like Codex or Claude Code. A polkit rule lets you, and only you, open that shell without a password. The Telegram and QQ bridges run in the container as always-on services, so you can also message Alyosha from your phone. [Chat channels](Channels.md#the-telegram-bridge) Alyosha runs as your user, never as root, because root inside a NixOS container is close to root on the host. Your user has the same user ID inside, so files Alyosha writes are owned by you on the host.

## How we get there

The component code does not depend on the container. The same build runs on the host and inside the container, because paths such as the configuration folder and the workspace come from the environment and are not fixed in code. Build and debug the core, the TUI, and the first plugins on the host, which are steps 1 to 4 of the implementation order. Then add the container as a NixOS module at step 5, before any real sign-ins or tokens are used. The first check in the container is whether Bubblewrap starts inside it. If it does not, adjust the container's settings and keep Bubblewrap. [Implementation order](../README.md#where-to-start-implementing)

## Deferred

- Running the TUI as a client of a shared service. The TUI starts from a shell, and only the bridges run as services.
- Mounting individual projects instead of all of `~/work`.
- Removing the container's access to the host's Nix daemon, which lets programs in the container build or download packages into the shared store.
- An ephemeral container that resets its own files on restart. Everything worth keeping already lives in the mounted folders.
- Running each MCP server in its own sandbox.

[Deployment implementation guide](../develop/Deployment.md)
