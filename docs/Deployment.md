# Deployment

Alyosha runs inside a NixOS container on your machine. The container holds the whole agent: the Agent Loop, GPROXY, and the MCP servers. Inside it, each shell command still gets its own Bubblewrap sandbox. The container limits what the agent as a whole can reach, and Bubblewrap limits what the model's shell can reach. [Architecture overview](README.md) · [Environment Interface](EnvironmentInterface.md)

```text
host (NixOS)
└─ containers.alyosha               declared in your host configuration
   │  sees: ~/alyosha/config (read-only), ~/alyosha/state; its own ~/work
   │  network: private, internet through the host
   │
   └─ your user (not root)
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
- **The model's shell cannot add packages.** Bubblewrap mounts the store read-only, without the daemon socket. Its tools come from the project's Nix dev shell, which Alyosha loads outside the sandbox at session start, or from a small default list. To give the shell a new tool, add it to the project's `flake.nix` and start a new session. For projects without a flake, add it to Alyosha's default tool list and rebuild with `nixos-rebuild switch`. [Bubblewrap launch contract](EnvironmentInterface.md#bubblewrap-launch-contract)
- **Secrets never go in the store.** Everything in `/nix/store` is readable by every user and by the container. Keep tokens and keys in `~/alyosha` and never put them directly in a Nix expression.

## What it can see

The container mounts one host folder, `~/alyosha`, at the same path it has on the host. It has two parts, split by who writes them:

- **`~/alyosha/config`, read-only.** What you write: the configuration file, skills, and the browser's saved sign-ins. Nothing in the container can change them, so neither the agent nor an MCP server can rewrite its own settings or instructions. [Configuration](reference/Configuration.md) · [Skills](Skills.md)
- **`~/alyosha/state`, writable.** What Alyosha and its services write and you may want to read on the host: GPROXY's secrets and database, the Google sign-in store, scheduled tasks, Alyosha's log files, and the daily dumps of the session database. [Logging](reference/Logging.md)

Everything else exists only inside the container:

- **`~/work`, Alyosha's own working folder.** Each session works in one folder under it: `~/work/owner` for your private chats in Telegram and QQ, `~/work/groups/<name>` for each trusted group, and `~/work/guest` as the working directory of guest sessions' MCP servers. The folders hold only files the agent made, such as notes, saved pages, and downloads. The conversations are in the session database. None of your own folders is shared: for Alyosha to work on a project, it clones or downloads the project into its folder. To look at the files, open `machinectl shell <you>@alyosha`. [Chat channels](Channels.md#two-bridges-at-once)
- **The session database.** PostgreSQL runs as a service in the container, reachable only through its Unix socket. It holds the conversations, your memory, and the notes on people in group chats. [Durable sessions](SessionStore.md#the-database) · [Memory](Memory.md)
- **`~/.cache/alyosha`.** The shell's download cache and each session's private folders, which Alyosha rebuilds when they are missing.

The container's own files survive a restart or rebuild of the container. They live in `/var/lib/nixos-containers/alyosha` on the host. Only the database is dumped each day; `~/work` is not backed up.

Keeping `~/alyosha` at the same path on both sides means a path in a log line or an error is also the path on the host. Everything else on the host, such as your home directory, `~/.ssh`, and your projects, does not exist inside.

The container has its own network with a route to the internet through the host. GPROXY and MCP servers can reach their services. Services listening on the host's `localhost` are out of reach.

## Limits on memory and CPU

The container has a memory cap, set in the host configuration. When everything inside together uses more, the kernel stops processes inside the container instead of freezing your machine. A bridge whose agent is stopped this way restarts and resumes its session. [Durable sessions](SessionStore.md) Shell commands and background jobs also run at a lower CPU priority, so a long build leaves the host responsive. [Container module](#host-module)

## Using it

The host starts the container at boot. The Telegram and QQ bridges run in the container as always-on services, and you talk to Alyosha by messaging them. [Chat channels](Channels.md#the-telegram-bridge) The host's `alyosha` is a small launcher that runs the container's `alyosha` through `machinectl shell`, for `alyosha gateway init` and `gateway login`. A polkit rule lets you, and only you, open that shell without a password. Alyosha runs as your user, never as root, because root inside a NixOS container is close to root on the host. Your user has the same user ID inside, so files Alyosha writes are owned by you on the host.

## How we get there

The component code does not depend on the container. The same build runs on the host and inside the container, because Alyosha finds its folders from `$HOME` and `ALYOSHA_HOME` and does not fix them in code. Build and debug the core and the first plugins on the host, which are steps 1 to 4 of the implementation order. Then add the container as a NixOS module at step 5, before any real sign-ins or tokens are used. The first check in the container is whether Bubblewrap starts inside it. If it does not, adjust the container's settings and keep Bubblewrap. [Implementation order](README.md#where-to-start-implementing)

## Deferred

- Sharing your own project folders with the container.
- Removing the container's access to the host's Nix daemon, which lets programs in the container build or download packages into the shared store.
- An ephemeral container that resets its own files on restart. It would also clear `~/work` and the database, which live in the container's own state.
- Running each MCP server in its own sandbox.

## Implementation

Add the container at step 5 of the build order, after the core, search, and the browser work on the host, and before email and the bridges. Nothing in the components changes for it. Alyosha must find its folders through `$HOME` and the XDG variables and take the workspace from the bridge, as the component pages already describe. [Architecture overview](README.md)

### Host module

Declare the container in the host configuration. This sketch assumes the user `mjli` with user ID 1000. `alyosha` and `gproxy` stand for the packages that Alyosha's flake will provide. On the host for v1, install the same two packages. [GPROXY package](LLMClientGateway.md#install-and-update-gproxy-with-nix)

```nix
{ config, pkgs, ... }:
let
  user = "mjli";
  home = "/home/${user}";
in {
  containers.alyosha = {
    autoStart = true;
    privateNetwork = true;
    hostAddress = "10.233.1.1";
    localAddress = "10.233.1.2";

    # Each mount keeps its host path. ~/work exists only inside.
    bindMounts = {
      "${home}/alyosha/config" = { hostPath = "${home}/alyosha/config"; isReadOnly = true; };
      "${home}/alyosha/state" = { hostPath = "${home}/alyosha/state"; isReadOnly = false; };
    };

    config = { pkgs, ... }: {
      users.users.${user} = { isNormalUser = true; uid = 1000; };
      # Add the commands your MCP servers use, such as the browser's
      # playwright-mcp (Browser.md), gogcli (EmailCalendar.md), and
      # open-websearch (WebSearch.md).
      environment.systemPackages = [ alyosha gproxy ];
      # Add systemd.services.gproxy from LLMClientGateway.md, PostgreSQL,
      # alyosha-db, and its backup from SessionStore.md, and the bridge
      # services from TelegramBridge.md and QQBridge.md, here. The flake's
      # nixosModules.container (nix/container.nix) does all of this.
      # Set system.stateVersion to the host's value.
    };
  };

  # Kill processes inside the container instead of freezing the host when
  # memory runs out. Pick a value for your machine, such as 75% of its RAM.
  systemd.services."container@alyosha".serviceConfig.MemoryMax = "12G";

  networking.nat = {
    enable = true;
    internalInterfaces = [ "ve-+" ];
    externalInterface = "wlan0";  # the host's uplink
  };
}
```

| Setting | Why |
| --- | --- |
| `bindMounts` | `config` stays read-only, the NixOS default, so nothing in the container can change your settings or skills. `state` needs writes for GPROXY, the logs, and the database dumps. Mounting at the host path keeps paths identical on both sides. |
| `uid = 1000` | The container does not remap user IDs, so the same ID makes files written inside belong to you on the host. Use the output of `id -u`. |
| `privateNetwork` and `networking.nat` | The container gets its own network and loopback. NAT gives GPROXY and MCP servers the internet while services on the host's `localhost` stay unreachable. |
| `autoStart` | The container is ready when you open a shell. |
| `MemoryMax` on `container@alyosha` | Caps the memory of everything in the container: the agent, the Chromium of each session, builds, and jobs. When the cap is reached, the kernel's out-of-memory killer chooses processes inside the container, and the host keeps running. |

GPROXY keeps listening on `127.0.0.1` inside the container. That is the container's own loopback, so the gateway setup is unchanged. When moving in, disable the host's `gproxy` service before starting the container's. [Run GPROXY as a service](LLMClientGateway.md#run-gproxy-as-a-service)

### Packages and the Nix store

The container module mounts the host's `/nix/store`, the Nix database, and the Nix daemon socket read-only. It also gives the container its own profile and garbage-collector roots under `/nix/var/nix/*/per-container/alyosha`. A read-only socket mount still lets programs connect, so `nix` commands inside the container work through the host daemon. [NixOS container module](https://github.com/NixOS/nixpkgs/blob/master/nixos/modules/virtualisation/nixos-containers.nix)

Bubblewrap and `pasta` come from Alyosha's package, which refers to `pkgs.bubblewrap`, `pkgs.passt`, and the default shell tools. The sandbox mounts the whole store read-only and never the daemon socket. Alyosha loads a project's dev shell outside the sandbox through the daemon, so the container needs Nix with flakes allowed. The launch contract is unchanged inside the container. [Project dev shell](EnvironmentInterface.md#project-dev-shell) Install the commands MCP servers need through `environment.systemPackages` in the container's configuration. [Bubblewrap launch contract](EnvironmentInterface.md#bubblewrap-launch-contract)

Keep secrets out of Nix expressions, including the container module and MCP `env` values. Anything written into a Nix expression ends up in the world-readable store. Alyosha's configuration file stays an ordinary file in `~/alyosha/config`. [MCP configuration](MCP.md#v1-configuration)

### Start a session

```bash
machinectl shell mjli@alyosha
```

Then run `alyosha gateway …` or probe the sandbox. Never run Alyosha as root inside the container.

The flake's module, `nixosModules.container`, mounts `~/alyosha/config` and `~/alyosha/state` and, with `hostCommand` (on by default), installs a host `alyosha`, which runs `machinectl shell -q --uid=<user> alyosha /run/current-system/sw/bin/alyosha <args>`, passing `TERM` on, for the gateway commands. A polkit rule allows `org.freedesktop.machine1.shell` for the owner into this machine as themselves. Remove a host install of `alyosha` and the host's `gproxy` service when switching to the container.

### Bubblewrap inside the container

Bubblewrap needs unprivileged user namespaces, a private procfs for the sandbox, and a TUN device for pasta. The shell is still blocked in the container on ms02, so every `run_command` fails with "The sandbox could not start". The search and Chromium servers run outside Bubblewrap and can be tested independently. [Shell launch contract](EnvironmentInterface.md#bubblewrap-launch-contract)

Probed on ms02 on 2026-11-26 (kernel 6.18.60, systemd 260, Bubblewrap 0.13.1, passt 2026_07_16), as the owner inside the container:

- **TUN.** `/dev/net/tun` does not exist in the container. pasta stops with `Failed to open() /dev/net/tun: No such file or directory` and `Failed to set up tap device in namespace`. It also warns `Couldn't mount /proc`.
- **Namespaces.** Bubblewrap starts with `--unshare-user`, `--unshare-pid`, `--unshare-ipc`, `--unshare-net`, and `--dev /dev`.
- **procfs.** Only `--proc /proc` fails, with `Can't mount proc on /proc: Operation not permitted`. The kernel lets a user namespace mount a new procfs only when its mount namespace already has a fully visible one. nspawn covers the container's `/proc` with read-only mounts over `sys`, `acpi`, `asound`, `bus`, `fs`, and `irq`, and with empty files over `kallsyms`, `kcore`, `keys`, `kmsg`, `sysrq-trigger`, `timer_list`, and `sys/kernel/random/boot_id`, so it is not fully visible.
- **Host.** On the host, the same pasta and Bubblewrap launch, `--proc /proc` included, works.

Two shortcuts are ruled out. `enableTun` would also give the whole container `CAP_NET_ADMIN`, which pasta does not need. Binding the container's `/proc` into the sandbox (`--ro-bind /proc /proc`) would show the sandbox every container process, and a process running as the owner can read the environment of the owner's other processes, such as GPROXY's keys.

#### Experiment: unblock the sandbox in the container

This experiment decides whether the container can be fixed or the inner sandbox needs a different design. Run it on ms02. It restarts the container, and with it the bridges. Record each result here.

**1. Give the container the TUN device.** Add a temporary override next to `services.alyosha` in the host flake, then rebuild and restart the container:

```nix
containers.alyosha = {
  bindMounts."/dev/net/tun" = { hostPath = "/dev/net/tun"; isReadOnly = false; };
  allowedDevices = [ { node = "/dev/net/tun"; modifier = "rw"; } ];
};
```

```bash
sudo nixos-rebuild switch --flake /etc/nixos#ms02
```

```bash
sudo systemctl restart container@alyosha
```

**2. Probe as the owner.** Open `machinectl shell mjli@alyosha`, put the sandbox's own programs on `PATH` from the `alyosha` wrapper, and define the two launches with Alyosha's flags:

```bash
eval "$(grep -E '^(PATH=|export PATH)' "$(readlink -f "$(command -v alyosha)")")"
sh=$(command -v bash)
net() { pasta --config-net --quiet --no-map-gw -t none -u none -T none -U none -- "$@"; }
box() { bwrap --unshare-user --unshare-pid --unshare-ipc --ro-bind /nix/store /nix/store --dev /dev "$@"; }
```

Then check pasta, its network, and Bubblewrap's procfs:

```bash
net "$sh" -c 'echo pasta ok'
```

```bash
net "$sh" -c 'exec 3<>/dev/tcp/1.1.1.1/443 && echo internet ok'
```

```bash
net "$sh" -c 'exec 3<>/dev/tcp/127.0.0.1/18787 && echo GPROXY REACHABLE || echo loopback blocked'
```

```bash
box --proc /proc -- "$sh" -c 'echo bwrap ok'
```

Expect `pasta ok`, `internet ok`, and `loopback blocked`. Bubblewrap's procfs should still fail at this step.

**3. Give the container a fully visible procfs.** As the container's root (`sudo nixos-container root-login alyosha` on the host), mount a fresh procfs under a folder only root can open. The folder must be the parent of the mount point, because a mounted procfs shows its own permissions instead of the folder's. Try read-only first:

```bash
install -d -m 0700 /run/alyosha && install -d /run/alyosha/proc
```

```bash
mount -t proc -o ro,nosuid,nodev,noexec proc /run/alyosha/proc
```

Run the Bubblewrap probe from step 2 again as the owner. If it still fails, remount the procfs writable and probe once more:

```bash
umount /run/alyosha/proc && mount -t proc -o nosuid,nodev,noexec proc /run/alyosha/proc
```

A read-only mount may not be enough, because the kernel treats it as locked read-only inside the sandbox's user namespace, while Bubblewrap mounts its procfs writable. The writable mount gives the container's root a procfs whose `/proc/sys` is not read-only, and the container shares the host's user IDs, so root there can change the host's kernel settings. The design already treats root in the container as close to root on the host. [Using it](#using-it) The writable mount removes one of the things that still limits it.

**4. Check the full launch.** When the probe passes, check as the owner that `ls /run/alyosha` is refused and that the sandbox sees only its own processes:

```bash
net bwrap --unshare-user --unshare-pid --unshare-ipc --ro-bind /nix/store /nix/store --dev /dev --proc /proc -- "$sh" -c 'echo /proc/[0-9]*'
```

Expect one or two process IDs. Then ask Alyosha from Telegram to run `pwd; uname -r; nproc`, and work through the shell check in [Verification](#verification). The manual mount disappears when the container restarts.

**5. Decide.**

| TUN | procfs | Next |
| --- | --- | --- |
| Works | Read-only mount works | Keep the design. Add the device and a mount unit for `/run/alyosha/proc` to `nix/container.nix`. |
| Works | Only the writable mount works | Choose between accepting the writable procfs and an inner sandbox that needs no namespaces, such as Landlock. |
| Works | Neither works | Redesign the inner sandbox: Landlock, or a separate container for commands next to the agent's. |
| Fails | Any | Record pasta's error. Bubblewrap's own network namespace works, so commands could reach the internet through a proxy instead of pasta. |

The container's tmpfiles rules create `~/work` and give the owner write access to `~/.config`, which Chromium needs for its crash database. Without write access Chromium exits on its first navigation with `chrome_crashpad_handler: --database is required`. Check the browser with a real navigation and snapshot as the owner. [Chromium inside the container](Browser.md#chromium-inside-the-container)

### Move to `~/alyosha`

Earlier versions of the module mounted `~/work`, `~/.config/alyosha`, and `~/.local/share/alyosha`. Stop the container, move the files once, then start it with the new module:

```bash
sudo systemctl stop container@alyosha
```

```bash
mkdir -p ~/alyosha/config ~/alyosha/state/secrets
mv ~/.config/alyosha/config.toml ~/.config/alyosha/skills ~/alyosha/config/
mv ~/.config/alyosha/gproxy.env ~/alyosha/state/secrets/
mv ~/.local/share/alyosha/gproxy ~/.local/share/alyosha/logs ~/alyosha/state/
```

Move `~/.config/alyosha/gog` to `~/alyosha/state/gog`, `~/.local/share/alyosha/browser` to `~/alyosha/config/browser`, and `schedule.toml` to `~/alyosha/state/` if they exist, and update the paths in `config.toml`. Then remove the `workspace` keys and turn `trusted_groups` into tables. [Configuration](reference/Configuration.md#telegram) Copy each memory file into the new store by asking Alyosha to remember it again. The old `~/work` folders held only the bridges' scratch files. Delete them, and delete what is left of `~/.local/share/alyosha`: the shell cache, the session folders, and the empty guest folder.

### Verification

These checks need NixOS with the container built from the module above, and a provider account for the GPROXY case. Run the component tests with `cargo test` on the host first.

- Run the shell integration checks from the Environment Interface page inside the container: a nonzero exit, a sandbox setup error, and a timeout with a subprocess. [Verify on the host](EnvironmentInterface.md#verify-on-the-host)
- From a shell in the container, check that `~/work` exists, that `~/.ssh` and `~/Projects` do not, and that writing a file in `~/alyosha/config` fails while one in `~/alyosha/state` succeeds and belongs to your user on the host.
- Run `alyosha --help` on the host. Check that it prints the container's usage without a password prompt.
- Check that `systemctl show container@alyosha -p MemoryMax` on the host reports the configured value. In the container, run a job that allocates more than the cap. Check that the host stays responsive and the log of the program that lost a process says so, or the bridge restarts.
- Start a test service on the host's `127.0.0.1`. Check that the container cannot connect to it, and that HTTPS to the provider works, name resolution included.
- Run one tool exchange through GPROXY with the configured model from inside the container. [Gateway checks](LLMClientGateway.md#verify-with-the-upstream-binary-and-mock-providers)
- Configure an MCP server that tries to read a host file outside the mounts. Check that the read fails. Repeat the environment check from the MCP page. [MCP checks](MCP.md#verification)
- From a shell in the container, run `nix shell nixpkgs#hello -c hello`. Check that it works and that the new store path is visible on the host.
- Ask the model to run `nix shell nixpkgs#hello` and `ls /nix/store` through `run_command`. Check that `nix` is missing or fails, while the store listing works. Repeat the shell network checks from the Environment Interface page inside the container. [Verify on the host](EnvironmentInterface.md#verify-on-the-host)
- Add a skill on the host. Check that a new session in the container lists it.

[Architecture overview](README.md)
