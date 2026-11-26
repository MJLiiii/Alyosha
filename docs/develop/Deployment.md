# Run Alyosha in a NixOS container

Add the container at step 5 of the build order, after the core, the TUI, search, and the browser work on the host, and before email and the bridges. Nothing in the components changes for it. Alyosha must find its folders through `$HOME` and the XDG variables and take the workspace from where it starts, as the component guides already describe. [Deployment design](../design/Deployment.md) · [Architecture overview](../README.md)

## Host module

Declare the container in the host configuration. This sketch assumes the user `mjli` with user ID 1000. `alyosha` and `gproxy` stand for the packages that Alyosha's flake will provide. On the host for v1, install the same two packages. [GPROXY package](LLMClientGateway.md#install-and-update-gproxy-with-nix)

```nix
{ config, pkgs, ... }:
let
  user = "mjli";
  home = "/home/${user}";
  shared = path: { hostPath = path; isReadOnly = false; };
in {
  containers.alyosha = {
    autoStart = true;
    privateNetwork = true;
    hostAddress = "10.233.1.1";
    localAddress = "10.233.1.2";

    # Each mount keeps its host path.
    bindMounts = {
      "${home}/work" = shared "${home}/work";
      "${home}/.config/alyosha" = shared "${home}/.config/alyosha";
      "${home}/.local/share/alyosha" = shared "${home}/.local/share/alyosha";
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
| `bindMounts` with `isReadOnly = false` | NixOS mounts read-only by default. Workspaces, memory, and GPROXY's database need writes. Mounting at the host path keeps paths identical on both sides. |
| `uid = 1000` | The container does not remap user IDs, so the same ID makes files written inside belong to you on the host. Use the output of `id -u`. |
| `privateNetwork` and `networking.nat` | The container gets its own network and loopback. NAT gives GPROXY and MCP servers the internet while services on the host's `localhost` stay unreachable. |
| `autoStart` | The container is ready when you open a shell. |
| `MemoryMax` on `container@alyosha` | Caps the memory of everything in the container: the agent, the Chromium of each session, builds, and jobs. When the cap is reached, the kernel's out-of-memory killer chooses processes inside the container, and the host keeps running. |

GPROXY keeps listening on `127.0.0.1` inside the container. That is the container's own loopback, so the gateway setup is unchanged. When moving in, disable the host's `gproxy` service before starting the container's: both use the same mounted data folder. [Run GPROXY as a service](LLMClientGateway.md#run-gproxy-as-a-service)

## Packages and the Nix store

The container module mounts the host's `/nix/store`, the Nix database, and the Nix daemon socket read-only. It also gives the container its own profile and garbage-collector roots under `/nix/var/nix/*/per-container/alyosha`. A read-only socket mount still lets programs connect, so `nix` commands inside the container work through the host daemon. [NixOS container module](https://github.com/NixOS/nixpkgs/blob/master/nixos/modules/virtualisation/nixos-containers.nix)

Bubblewrap and `pasta` come from Alyosha's package, which refers to `pkgs.bubblewrap`, `pkgs.passt`, and the default shell tools. The sandbox mounts the whole store read-only and never the daemon socket. Alyosha loads a project's dev shell outside the sandbox through the daemon, so the container needs Nix with flakes allowed. The launch contract is unchanged inside the container. [Project dev shell](EnvironmentInterface.md#project-dev-shell) Install the commands MCP servers need through `environment.systemPackages` in the container's configuration. [Bubblewrap launch contract](EnvironmentInterface.md#bubblewrap-launch-contract)

Keep secrets out of Nix expressions, including the container module and MCP `env` values. Anything written into a Nix expression ends up in the world-readable store. Alyosha's configuration file stays an ordinary file in `~/.config/alyosha`. [MCP configuration](MCP.md#v1-configuration)

## Start a session

```bash
machinectl shell mjli@alyosha
```

Then `cd` to a workspace and run `alyosha`. Never run it as root inside the container.

The flake's module, `nixosModules.container`, sets `services.alyosha.workspaces` (default `[ "~/work" ]`) as the shared folders and, with `hostCommand` (on by default), installs a host `alyosha`. It checks that the current folder is under a shared folder, then runs `machinectl shell -q --uid=<user> alyosha /run/current-system/sw/bin/env -C <folder> /run/current-system/sw/bin/alyosha <args>`, passing `TERM` on. A polkit rule allows `org.freedesktop.machine1.shell` for the owner into this machine as themselves. Remove a host install of `alyosha` and the host's `gproxy` service when switching to the container.

## Bubblewrap inside the container

Bubblewrap needs unprivileged user namespaces, a private procfs for the sandbox, and a TUN device for pasta. The shell is still blocked in the container on ms02, so every `run_command` fails with "The sandbox could not start". The search and Chromium servers run outside Bubblewrap and can be tested independently. [Shell launch contract](EnvironmentInterface.md#bubblewrap-launch-contract)

Probed on ms02 on 2026-11-26 (kernel 6.18.60, systemd 260, Bubblewrap 0.13.1, passt 2026_07_16), as the owner inside the container:

- **TUN.** `/dev/net/tun` does not exist in the container. pasta stops with `Failed to open() /dev/net/tun: No such file or directory` and `Failed to set up tap device in namespace`. It also warns `Couldn't mount /proc`.
- **Namespaces.** Bubblewrap starts with `--unshare-user`, `--unshare-pid`, `--unshare-ipc`, `--unshare-net`, and `--dev /dev`.
- **procfs.** Only `--proc /proc` fails, with `Can't mount proc on /proc: Operation not permitted`. The kernel lets a user namespace mount a new procfs only when its mount namespace already has a fully visible one. nspawn covers the container's `/proc` with read-only mounts over `sys`, `acpi`, `asound`, `bus`, `fs`, and `irq`, and with empty files over `kallsyms`, `kcore`, `keys`, `kmsg`, `sysrq-trigger`, `timer_list`, and `sys/kernel/random/boot_id`, so it is not fully visible.
- **Host.** On the host, the same pasta and Bubblewrap launch, `--proc /proc` included, works.

Two shortcuts are ruled out. `enableTun` would also give the whole container `CAP_NET_ADMIN`, which pasta does not need. Binding the container's `/proc` into the sandbox (`--ro-bind /proc /proc`) would show the sandbox every container process, and a process running as the owner can read the environment of the owner's other processes, such as GPROXY's keys.

### Experiment: unblock the sandbox in the container

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

A read-only mount may not be enough, because the kernel treats it as locked read-only inside the sandbox's user namespace, while Bubblewrap mounts its procfs writable. The writable mount gives the container's root a procfs whose `/proc/sys` is not read-only, and the container shares the host's user IDs, so root there can change the host's kernel settings. The design already treats root in the container as close to root on the host. [Using it](../design/Deployment.md#using-it) The writable mount removes one of the things that still limits it.

**4. Check the full launch.** When the probe passes, check as the owner that `ls /run/alyosha` is refused and that the sandbox sees only its own processes:

```bash
net bwrap --unshare-user --unshare-pid --unshare-ipc --ro-bind /nix/store /nix/store --dev /dev --proc /proc -- "$sh" -c 'echo /proc/[0-9]*'
```

Expect one or two process IDs. Then run `alyosha` in a folder under `~/work` and have it run `pwd; uname -r; nproc`, and work through the shell check in [Verification](#verification). The manual mount disappears when the container restarts.

**5. Decide.**

| TUN | procfs | Next |
| --- | --- | --- |
| Works | Read-only mount works | Keep the design. Add the device and a mount unit for `/run/alyosha/proc` to `nix/container.nix`. |
| Works | Only the writable mount works | Choose between accepting the writable procfs and an inner sandbox that needs no namespaces, such as Landlock. |
| Works | Neither works | Redesign the inner sandbox: Landlock, or a separate container for commands next to the agent's. |
| Fails | Any | Record pasta's error. Bubblewrap's own network namespace works, so commands could reach the internet through a proxy instead of pasta. |

The container's tmpfiles rule gives the owner write access to `~/.config`. On a fresh container, nspawn creates this directory as root when setting up the nested Alyosha config mount. Without write access Chromium exits on its first navigation with `chrome_crashpad_handler: --database is required`. Check the browser with a real navigation and snapshot as the owner. [Chromium inside the container](Browser.md#chromium-inside-the-container)

## Verification

These checks need NixOS with the container built from the module above, and a provider account for the GPROXY case. Run the component tests with `cargo test` on the host first.

- Run the shell integration checks from the Environment Interface guide inside the container: a nonzero exit, a sandbox setup error, and a timeout with a subprocess. [Verify on the host](EnvironmentInterface.md#verify-on-the-host)
- Write a file through `write_file` in a workspace. Check on the host that it belongs to your user.
- From a shell in the container, check that `~/.ssh` and a folder outside `~/work` do not exist.
- Run `alyosha` on the host in `~/work/some-project` and in `/tmp`. Check that the first opens the TUI in the container without a password prompt, in that folder, and that the second refuses, naming `~/work`.
- Check that `systemctl show container@alyosha -p MemoryMax` on the host reports the configured value. In the container, run a job that allocates more than the cap. Check that the host stays responsive and the log of the program that lost a process says so, or the bridge restarts.
- Start a test service on the host's `127.0.0.1`. Check that the container cannot connect to it, and that HTTPS to the provider works, name resolution included.
- Run one tool exchange through GPROXY with the configured model from inside the container. [Gateway checks](LLMClientGateway.md#verify-with-the-upstream-binary-and-mock-providers)
- Configure an MCP server that tries to read a host file outside the mounts. Check that the read fails. Repeat the environment check from the MCP guide. [MCP checks](MCP.md#verification)
- From a shell in the container, run `nix shell nixpkgs#hello -c hello`. Check that it works and that the new store path is visible on the host.
- Ask the model to run `nix shell nixpkgs#hello` and `ls /nix/store` through `run_command`. Check that `nix` is missing or fails, while the store listing works. Repeat the shell network checks from the Environment Interface guide inside the container. [Verify on the host](EnvironmentInterface.md#verify-on-the-host)
- Add a skill and edit a memory file on the host. Check that a new session in the container lists both.

[Architecture overview](../README.md)
