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
      # Add systemd.services.gproxy from LLMClientGateway.md, and the
      # bridge services from TelegramBridge.md and QQBridge.md, here.
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

Bubblewrap needs unprivileged user namespaces, a private procfs for the sandbox, and a TUN device for pasta. On ms02, the container could create user namespaces, but pasta failed to open `/dev/net/tun` and Bubblewrap failed to mount its own `/proc`. The shell integration checks are still blocked in that deployment. The search and Chromium servers run outside Bubblewrap and can be tested independently. [Shell launch contract](EnvironmentInterface.md#bubblewrap-launch-contract)

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
