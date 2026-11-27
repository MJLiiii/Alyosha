# Deployment

Alyosha runs inside a NixOS container on your machine. Everything it keeps lives inside the container: its configuration, secrets, conversations, memory, and working folders. Inside, each service runs as its own Unix user, so ordinary file permissions keep secrets with the one service that needs them. Each shell command runs as a separate user again, in a systemd unit of its own that sees only its session's working folder. The container keeps the agent away from your machine, and the users and units keep the model's commands away from the agent's secrets and from other sessions. [Architecture overview](README.md) · [Environment Interface](EnvironmentInterface.md)

```text
host (NixOS)
├─ ~/.agents/skills/              your skills, read-only in the container
├─ /var/backup/alyosha/           daily dumps of the session database
├─ alyosha                        the host command: status, logs, secrets, backups
├─ firewall                       the container reaches the internet, never the host or the LAN
└─ containers.alyosha             declared in your host configuration
   ├─ gproxy        user gproxy   GPROXY, its database, and its master key
   ├─ postgres      user postgres the session database: conversations, memory, people's notes
   ├─ alyosha       user alyosha  the Telegram and QQ bridges → alyosha acp → MCP servers
   └─ alyosha-run   user alyosha-run
                    one unit per run_command or job, seeing only its session's folder
```

## Why a container around the whole agent

Without the container, MCP servers run as ordinary processes and can reach anything your user can, and so could a bug in Alyosha's own path checks. Inside the container, nothing of your home exists except your skills, read-only, and nothing on your host or your local network can be reached. [Where MCP servers run](MCP.md#where-servers-run)

The container alone does not protect Alyosha from its own commands. The model's shell runs whatever a prompt, an email, or a web page talks the model into, and on 2026-11-26 a process running as the agent's user could read every key, the whole session database, and other processes' environments, and reach the host's SSH port and the router. So inside the container, the services and the commands run as different users, and each command runs in a unit that sees only its own folder. [Command units](EnvironmentInterface.md#shell-commands)

## What the container is

A NixOS container is a `systemd-nspawn` machine declared in the host's NixOS configuration. It runs its own systemd, users, and services on the host's kernel. `nixos-rebuild switch` builds it like the rest of the system, so there is no image to download. Its files live in `/var/lib/nixos-containers/alyosha` on the host and survive restarts and rebuilds.

## Packages from the Nix store

The container does not get its own copy of the Nix store. It shares the host's `/nix/store` read-only, which is why it costs almost no disk space. Packages reach each layer as follows:

```text
host /nix/store                  every package the host has built or downloaded
└─ container                     sees all of it, read-only; only declared packages are on PATH
   └─ a command's unit           sees all of it, read-only; PATH from the project's dev shell or the default tools
```

- **The container sees the whole store but installs only what you declare.** Its configuration puts Alyosha, GPROXY, and the commands of your MCP servers on `PATH`. Other packages are on disk, and a program can still run one by its full store path.
- **The agent can add packages.** NixOS gives the container the host's Nix daemon socket, so Alyosha can load a project's dev shell through it. New packages land in the shared store, where the host sees them too. Nothing in the container can change a path that already exists. Removing this access is deferred.
- **The model's shell cannot add packages.** A command's unit hides the daemon socket. Its tools come from the project's Nix dev shell, which Alyosha loads outside the unit at session start, or from a small default list. To give the shell a new tool, add it to the project's `flake.nix` and start a new session. [Project dev shell](EnvironmentInterface.md#project-dev-shell)
- **Secrets never go in the store.** Everything in `/nix/store` is readable by every user and by the container. Secrets live in the container's secrets folder, never in a Nix expression. [Where things live](#where-things-live)

## Where things live

| What | Where in the container | Owner and mode | How you change it |
| --- | --- | --- | --- |
| Settings | `/etc/alyosha/config.toml`, generated from `services.alyosha.settings` | root, read-only | Edit the host configuration, then `sudo nixos-rebuild switch` [Configuration](reference/Configuration.md) |
| Secrets | `/var/lib/alyosha-secrets/<name>`, one file each | root, readable by the `alyosha` group; `gproxy.env` root only | `alyosha secret set <name>` [Secrets](reference/Configuration.md#secrets) |
| GPROXY's database and provider sign-in | `/var/lib/gproxy` | `gproxy` | `alyosha gateway login` [LLM Client / Gateway](LLMClientGateway.md) |
| Conversations with the images you sent, memory, people's notes | PostgreSQL | `postgres`; only the `alyosha` role connects | Ask Alyosha; look with `alyosha memory list` [Memory](Memory.md) |
| Working folders | `/var/lib/alyosha/work/owner`, `work/groups/<name>`, `work/guest` | `alyosha-run`, group `alyosha-work` | Look with `alyosha shell` |
| Google sign-in store, `schedule.toml` | `/var/lib/alyosha` | `alyosha` | `alyosha shell` [Email and calendar](EmailCalendar.md) · [Scheduled tasks](Schedule.md) |
| Skills | host `~/.agents/skills`, mounted read-only at `/var/lib/alyosha/.agents/skills` | you | Your editor [Skills](Skills.md) |
| Shell cache and session folders | `/var/cache/alyosha` | `alyosha-run` and `alyosha` | Nothing; Alyosha rebuilds them |
| Logs | the container's journal | — | `alyosha logs` [Logging](reference/Logging.md) |
| Database dumps | host `/var/backup/alyosha`, mounted writable | root on the host | `alyosha backup` |

- **Each session has one working folder.** Your private chats in Telegram and QQ share `work/owner`, each trusted group has `work/groups/<name>`, and guest sessions' MCP servers start in `work/guest`. The folders hold only what the agent made, such as notes, saved pages, clones, and downloads, and what you sent it, under `inbox/`. [Where things are kept](Media.md#where-things-are-kept) None of your own folders is shared: for Alyosha to work on a project, it clones the project into its folder. [Chat channels](Channels.md#two-bridges-at-once)
- **What each user can read.** A command, as `alyosha-run`, sees its own folder, the Nix store, the shell cache, and the internet. The agent and its MCP servers, as `alyosha`, read the secrets they need, the database, and every working folder. Only `gproxy` and root read GPROXY's master key and provider sign-in.
- **Backups.** Only the database is dumped each day. The working folders are not backed up. The container's own files survive restarts and rebuilds, but `nixos-container destroy` removes them.

## The host command

On the host, `alyosha` is a small program of the flake's module that manages the container, so you never need the container's paths. Commands that change something ask for `sudo` themselves.

| Command | What it does |
| --- | --- |
| `alyosha status` | The container's services, the loopback rule, the secrets that are set, and the last backup |
| `alyosha logs [telegram\|qq\|gproxy\|db] [-f]` | The service's journal, as `journalctl -M alyosha -u …` |
| `alyosha secret set <name>` | Reads a secret from the terminal without echo, or from standard input, writes it with the right owner and mode, and restarts the services that use it |
| `alyosha secret list` | The names of the secrets that are set, never their values |
| `alyosha secret remove <name>` | Removes one and restarts its services |
| `alyosha gateway init` · `alyosha gateway login` · `alyosha gateway login transcription` | GPROXY's first setup, the model provider's sign-in, and the OpenAI API key for transcription, run as root in the container [Run GPROXY as a service](LLMClientGateway.md#run-gproxy-as-a-service) · [Transcription](Media.md#transcription) |
| `alyosha backup now` · `alyosha backup list` · `alyosha backup restore <file>` | Start a dump, list dumps, restore one into an emptied database |
| `alyosha memory list` | Your memories' names, dates, and descriptions, then how many memories and people's notes the database holds, read-only. It runs `alyosha memory-list` in the container as `alyosha`. [The host's list](Memory.md#the-hosts-list) |
| `alyosha shell` | A shell as `alyosha` in its state folder, to look at the working folders |
| `alyosha check` | Runs `alyosha check-units` in the container as `alyosha`: the command-unit checks from [Verification](#verification) that need no model account |
| `alyosha commands --json` | Every command with its arguments, for scripts and coding agents |

## Network

The container has its own network with a route to the internet through the host. GPROXY and MCP servers reach their services, and commands download packages.

- **Not the host.** The host's firewall drops everything the container sends to the host itself, so services listening on all the host's addresses, such as SSH, are out of reach too.
- **Not your local network.** The host refuses to forward the container's traffic to private addresses: `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `169.254.0.0/16` (which includes cloud metadata addresses), `100.64.0.0/10`, and the private and link-local IPv6 ranges. This holds for the browser too, so a web page cannot steer it to your router.
- **Not the container's own loopback, for commands.** GPROXY listens on the container's `127.0.0.1`. A rule in the container refuses loopback connections from commands' units. The agent and MCP servers still reach GPROXY. [Command network](EnvironmentInterface.md#network)

Name resolution uses the public nameservers in the container's configuration. A nameserver on the LAN, such as the router, would be unreachable.

## Limits on memory and CPU

The container has a memory cap, set in the host configuration. When everything inside together uses more, the kernel stops processes inside the container instead of freezing your machine. A bridge whose agent is stopped this way restarts and resumes its session. [Durable sessions](SessionStore.md) Each command's unit has its own cap as well, so a runaway build is stopped before it touches the bridges. Commands and background jobs run at a lower CPU priority, so a long build leaves the host responsive. [Container module](#host-module)

## Using it

The host starts the container at boot. The Telegram and QQ bridges run in the container as always-on services, and you talk to Alyosha by messaging them. [Chat channels](Channels.md#the-telegram-bridge) You manage it with the host command. Nothing runs as root in the container except the services that must, because root inside a NixOS container is close to root on the host.

## How we get there

The component code does not depend on the container. Alyosha finds its folders at fixed paths, which environment variables replace for tests and development on the host. [Folders](reference/Configuration.md#folders) Build and debug the core and the first plugins on the host, which are steps 1 to 4 of the implementation order. Then add the container as a NixOS module at step 5, before any real sign-ins or tokens are used. [Implementation order](README.md#where-to-start-implementing)

## Deferred

- Sharing your own project folders with the container.
- Removing the container's access to the host's Nix daemon, which lets programs in the container build or download packages into the shared store.
- An ephemeral container that resets its own files on restart. It would also clear the working folders and the database, which live in the container's own state.
- Running each MCP server in its own sandbox. They run as `alyosha` and can read what the agent reads.
- Running commands under gVisor inside their units, for a user-space kernel between them and the host's. It ran rootless in the container on 2026-11-26, but has no memory limit or loopback rule of its own and slows builds. [Why units](#why-units-and-not-bubblewrap)

## Implementation

Add the container at step 5 of the build order, after the core, search, and the browser work on the host, and before email and the bridges. [Architecture overview](README.md)

### Host module

The flake's module, `nixosModules.container` in `nix/container.nix`, declares everything below. In the host configuration:

```nix
imports = [ inputs.alyosha.nixosModules.container ];
services.alyosha = {
  enable = true;
  owner = "mjli";                 # whose ~/.agents/skills is mounted
  externalInterface = "enp4s0";   # the host's uplink, for NAT
  memoryMax = "12G";              # everything in the container
  commandMemoryMax = "4G";        # each command or job
  telegram.enable = true;
  qq.enable = true;
  nameservers = [ "62.179.104.196" "213.46.228.196" ];
  settings = {
    timezone = "Europe/Amsterdam";
    model = { provider = "chatgpt"; api = "codex"; name = "gpt-6.2-sol"; };
    telegram.owner_id = 123456789;
    qq = { app_id = "102000000"; owner_openid = "…"; trusted_groups.friends = "<group_openid>"; };
  };
};
```

`settings` is the configuration file as Nix values. The module writes it with `pkgs.formats.toml` to `/etc/alyosha/config.toml` and checks it with `alyosha config check` at build time, so a wrong key fails the rebuild instead of the start. It holds no secrets: the module refuses a `settings` value under a key the reference lists as a secret. [Configuration](reference/Configuration.md)

| Host setting | Why |
| --- | --- |
| `privateNetwork` and `networking.nat` | The container gets its own network and loopback. NAT gives it the internet. |
| `bindMounts` | Only two: `~<owner>/.agents/skills` read-only at `/var/lib/alyosha/.agents/skills`, and `/var/backup/alyosha` writable at the same path. |
| Firewall rules on `ve-alyosha` | Input from the container to the host is dropped. Forwarded traffic from it to the private ranges under [Network](#network) is rejected. |
| `MemoryMax` on `container@alyosha` | Caps everything in the container. When it is reached, the kernel's out-of-memory killer chooses processes inside the container, and the host keeps running. |
| The host command | `alyosha`, described under [The host command](#the-host-command). It reaches the container with `machinectl shell`, `systemctl -M`, and `journalctl -M`, and writes secrets through the container's root folder with `sudo`. |

### Inside the container

| Piece | Setting |
| --- | --- |
| Users | `alyosha` (home `/var/lib/alyosha`), `alyosha-run`, and `gproxy`, all system users without a login shell. The group `alyosha-work` holds `alyosha` and `alyosha-run`. |
| Folders | tmpfiles rules create `/var/lib/alyosha-secrets` (root:alyosha, `0750`), `/var/lib/alyosha/work/owner` and `work/groups/<name>` for each trusted group (alyosha-run:alyosha-work, `2770`; the bridges and the command units run with `UMask=0007`, so new files stay group-writable), `work/guest` (alyosha, `0700`), `/var/cache/alyosha/shell` (alyosha-run, `0700`), `/var/cache/alyosha/sessions` (alyosha, `0700`), and `/run/alyosha-run` (alyosha:alyosha-work, `0750`). |
| `gproxy.service` | `User=gproxy`, `StateDirectory=gproxy`, `EnvironmentFile=/var/lib/alyosha-secrets/gproxy.env`, which systemd reads as root. Skipped until the file exists. [Run GPROXY as a service](LLMClientGateway.md#run-gproxy-as-a-service) |
| PostgreSQL | As in [Durable sessions](SessionStore.md#container), with the role and database owner `alyosha`. |
| The bridges | `alyosha-telegram.service` and `alyosha-qq.service`, `User=alyosha`, standard output and error to the journal. They require the database and start after GPROXY. |
| Command units | One template per working folder, the media unit's template, the slice, the polkit rule, and the loopback rule, under [Command units](#command-units). |
| Chromium | A tmpfiles rule gives `alyosha` a writable `~/.config`, which Chromium needs for its crash database. Without it Chromium exits on its first navigation with `chrome_crashpad_handler: --database is required`. [Chromium inside the container](Browser.md#chromium-inside-the-container) |

### Command units

Each working folder that runs commands gets a template unit: `alyosha-run-owner@.service`, and `alyosha-run-group-<name>@.service` for each trusted group. Trusted groups are a fixed list in the settings, so the templates are too, and no unit is ever told which folder to mount at run time. Guest sessions run no commands and get none. The instance name is `<process ID>-<counter>` of the `alyosha acp` that started it. [Command launch](EnvironmentInterface.md#command-launch)

```ini
[Service]
Type=exec
User=alyosha-run
Group=alyosha-work
Slice=alyosha-run.slice
ExecStart=/run/current-system/sw/bin/alyosha run-exec %i
StandardOutput=truncate:/run/alyosha-run/%i/output
StandardError=inherit
# Keep a finished unit, so its result can be read.
RemainAfterExit=yes
WorkingDirectory=/workspace
BindPaths=/var/lib/alyosha/work/owner:/workspace
BindPaths=/var/cache/alyosha/shell:/home/alyosha/.cache
BindReadOnlyPaths=/var/lib/alyosha/.agents/skills:/skills
BindReadOnlyPaths=/run/alyosha-run/%i:/run/alyosha-run/%i
TemporaryFileSystem=/var/lib /var/cache /home /run/alyosha-run
InaccessiblePaths=/run/postgresql /nix/var/nix/daemon-socket /var/backup
PrivatePIDs=yes
PrivateTmp=yes
PrivateIPC=yes
ProtectSystem=strict
NoNewPrivileges=yes
RestrictNamespaces=yes
RestrictSUIDSGID=yes
LockPersonality=yes
MemoryMax=4G
MemorySwapMax=0
TasksMax=4096
RuntimeMaxSec=24h
Nice=10
UMask=0007
KillMode=control-group
```

- **Who may start them.** A polkit rule lets the user `alyosha` start and stop units whose name matches `alyosha-run-[a-z0-9-]+@[0-9]+-[0-9]+\.service`, and nothing else. A template fixes its user, mounts, and limits, so starting one cannot gain anything beyond what the unit allows.
- **The slice.** `alyosha-run.slice` is started at boot. A oneshot after it, `alyosha-run-firewall.service`, adds `iptables -I OUTPUT -o lo -m cgroup --path alyosha.slice/alyosha-run.slice -j REJECT` and the same for `ip6tables`. The dash in the slice's name puts it under `alyosha.slice`, so that is the cgroup path. The rule can only be added once the slice's cgroup exists. systemd's own `IPAddressDeny=` would be simpler, but it needs a BPF firewall, and the container lacks `CAP_BPF`.
- **Leftovers.** When `alyosha acp` starts, it stops every `alyosha-run-*@<pid>-*` unit whose process ID no longer runs: commands and jobs of a process that crashed.
- **The media unit.** `alyosha-run-media@.service` runs FFmpeg on videos and sound people send, so a flaw in a decoder reaches nothing of value. Its name matches the polkit rule, and it uses the same slice, user, and protections as the command units, with these differences: no working folder, `BindPaths=/run/alyosha-run/%i` for its input and output, `PrivateNetwork=yes`, `MemoryMax=1G`, `RuntimeMaxSec=5min`, and `ExecStart=/run/current-system/sw/bin/alyosha media-exec %i`. The `alyosha` package refers to `ffmpeg-headless` by its store path, so FFmpeg needs no place on `PATH`. [Video and sound in a media unit](Media.md#video-and-sound-in-a-media-unit)

### Why units and not Bubblewrap

The first design ran each command in Bubblewrap under `pasta`, as Codex and Claude Code do. In the container it cannot start, and fixing that would weaken the container. Probed on ms02 on 2026-11-26 (kernel 6.18.60, systemd 260, Bubblewrap 0.13.1, passt 2026_07_16):

- `/dev/net/tun` does not exist in the container, so pasta stops with `Failed to set up tap device in namespace`. Giving it the device is possible; `enableTun` would also give the whole container `CAP_NET_ADMIN`.
- Bubblewrap creates its user, PID, IPC, and network namespaces, but `--proc /proc` fails with `Operation not permitted`. The kernel lets a user namespace mount a new procfs only when its mount namespace already has a fully visible one, and nspawn covers the container's `/proc` with read-only mounts and empty files.
- A fresh procfs mounted read-only by the container's root does not help: the kernel locks it read-only inside the sandbox's user namespace. A writable one works, but it gives the container's root a writable view of the host's kernel settings.
- Binding the container's `/proc` into the sandbox, Claude Code's weaker nested mode, would show every container process and let a command read their environments, such as GPROXY's keys.

The container's systemd runs as root without a user namespace, so none of this refuses it. The same day, `systemd-run --wait --pipe` as root in the container, with the properties above, gave a command only its own processes, hid the other folders and the database socket, wrote files with the run user, kept the internet and name resolution, returned exit status 7 for `exit 7`, stopped a command and its background child at `RuntimeMaxSec=3`, and killed a 200 MB allocation at `MemoryMax=64M`. The cgroup-matched `iptables` rule refused loopback to those units only.

### Move into the container

Earlier versions of the module ran Alyosha as the owner and mounted `~/work`, `~/.config/alyosha`, and `~/.local/share/alyosha`. Move once:

1. Rebuild with the new module and stop the bridges: `sudo systemctl -M alyosha stop alyosha-telegram alyosha-qq`.
2. Set each secret from the old files with `alyosha secret set`: `telegram-token` and `qq-secret` from `config.toml`, `gproxy.env` without its `ALYOSHA_GATEWAY_KEY` line, `gateway-key` from that line, `browser-state.json` and `gog-keyring-password` if you use them. [Secrets](reference/Configuration.md#secrets)
3. Move the rest of `config.toml` into `services.alyosha.settings`.
4. Copy GPROXY's data folder to `/var/lib/gproxy` in the container and give it to `gproxy`, the Google sign-in store to `/var/lib/alyosha/gog`, and `schedule.toml` to `/var/lib/alyosha`, all with `sudo` through `/var/lib/nixos-containers/alyosha`.
5. Move skills to `~/.agents/skills`.
6. Start the bridges, and ask Alyosha to remember the one memory again.
7. Delete `~/work`, `~/.config/alyosha`, and `~/.local/share/alyosha` on the host, and the owner's old folders in the container.

### Move memory into the database

Builds before the database version kept memories and people's notes as files in `/var/lib/alyosha/memory` in the container. The first start of the new build imports them into the database and checks the copy. [Import the old files](Memory.md#import-the-old-files) Move once:

1. Before rebuilding, save what the files hold:

   ```bash
   alyosha memory list > ~/memory-before.txt
   ```

   ```bash
   sudo ls /var/lib/nixos-containers/alyosha/var/lib/alyosha/memory/users | wc -l
   ```

   The first gives one line per memory, the second the number of people with notes.
2. Rebuild with `sudo nixos-rebuild switch`. The first bridge to start imports the files.
3. If `alyosha status` shows the bridges failing, `alyosha logs telegram` names each file that could not be imported and why. Fix or move each one with `alyosha shell`, then `sudo systemctl -M alyosha restart alyosha-telegram alyosha-qq`. Nothing was imported in the failed attempt, so the next start imports everything.
4. Run `alyosha memory list`. Check that its last line counts as many memories as `~/memory-before.txt` has lines and as many people as step 1 counted, and that the same names appear.
5. From your private chat, ask "What do you remember about me?" and check that the answer uses your memories. In a group chat that is not trusted, ask the same and check that it knows nothing of them.
6. Delete the old folder and the saved list:

   ```bash
   sudo rm -r /var/lib/nixos-containers/alyosha/var/lib/alyosha/memory
   ```

   ```bash
   rm ~/memory-before.txt
   ```

   Until then an earlier build can still be started on the files, but it misses anything remembered since the move.

### Verification

These checks need NixOS with the container built from the module, and a provider account for the GPROXY case. Run the component tests with `cargo test` on the host first. `alyosha check` runs the ones that need no model account.

- In a command's unit: the working folder is writable and its files belong to `alyosha-run` with group `alyosha-work`; `/var/lib/alyosha-secrets`, `/var/lib/gproxy`, the other working folders, `/run/postgresql`, and the Nix daemon socket are absent; `/proc` shows only the unit's processes; `psql` cannot connect.
- From a command: HTTPS to a public address and name resolution work; `127.0.0.1:18787`, the host's `10.233.1.1:22`, and the router fail.
- As `alyosha`: `read_file` and `edit_file` work on a file a command wrote, and a command can change a file the agent wrote.
- Exit status, a timeout, Stop, a job, and `MemoryMax` behave as in [Command results](EnvironmentInterface.md#command-results). Kill an `alyosha acp` with a job running and start it again. Check that the leftover unit is stopped.
- Start a unit of another name as `alyosha` with `systemctl start`. Check that polkit refuses it.
- In a media unit: no network interface but loopback, `/var/lib/alyosha-secrets` and the working folders absent, and FFmpeg writes frames from a sample video. [Media verification](Media.md#verification)
- The browser cannot open the router's address; MCP servers still reach GPROXY; `sudo alyosha gateway login` works.
- `alyosha secret set telegram-token` restarts only the Telegram bridge; `alyosha secret list` prints no values; `alyosha logs qq -f` follows the bridge.
- `systemctl show container@alyosha -p MemoryMax` reports the configured value.
- Restart the container and the host. Check that the slice, the loopback rule, and the host's firewall rules are back.
- From a shell in the container, run `nix shell nixpkgs#hello -c hello`. Check that it works. Ask the model to run it through `run_command`. Check that it fails while `ls /nix/store` works.
- Add a skill in `~/.agents/skills` on the host. Check that a new session lists it.

[Architecture overview](README.md)
