# The NixOS container that runs Alyosha, GPROXY, the session database, the
# MCP servers, and the chat bridges as the owner. It sees only the workspace folders (~/work by
# default) and Alyosha's own folders, and the host's localhost services are
# unreachable. On the host, `alyosha` runs the container's alyosha in the
# current folder, so it starts from any terminal like Codex or Claude Code.
# See docs/design/Deployment.md and docs/develop/Deployment.md.
#
# In the host configuration:
#
#   imports = [ inputs.alyosha.nixosModules.container ];
#   services.alyosha = {
#     enable = true;
#     user = "mjli";
#     workspaces = [ "/home/mjli/work" ];
#     externalInterface = "wlan0";
#     memoryMax = "12G";
#     telegram.enable = true;
#   };
self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.alyosha;
  packages = self.packages.${pkgs.stdenv.hostPlatform.system};
  home = "/home/${cfg.user}";
  shared = path: {
    hostPath = path;
    isReadOnly = false;
  };
  # The host's `alyosha`: checks that the folder is shared with the
  # container, then runs alyosha there in the same folder.
  hostCommand = pkgs.writeShellApplication {
    name = "alyosha";
    runtimeInputs = [ pkgs.systemd ];
    text = ''
      here=$(pwd -P)
      shared=0
      roots=(${lib.escapeShellArgs cfg.workspaces})
      for root in "''${roots[@]}"; do
        case "$here/" in "$root"/*) shared=1 ;; esac
      done
      if [ "$shared" = 0 ]; then
        echo "alyosha: $here is not shared with the Alyosha container. Work in a folder under: ${lib.concatStringsSep " " cfg.workspaces}" >&2
        exit 2
      fi
      exec machinectl shell -q --uid=${cfg.user} \
        --setenv=TERM="''${TERM:-xterm-256color}" --setenv=COLORTERM="''${COLORTERM:-}" \
        alyosha /run/current-system/sw/bin/env -C "$here" /run/current-system/sw/bin/alyosha "$@"
    '';
  };
  # The session database's major version. Moving to another needs a dump
  # and restore (docs/develop/SessionStore.md#container).
  postgresql = pkgs.postgresql_17;
  bridge = name: {
    wantedBy = [ "multi-user.target" ];
    wants = [
      "network-online.target"
      "gproxy.service"
    ];
    requires = [ "alyosha-db.service" ];
    after = [
      "network-online.target"
      "gproxy.service"
      "alyosha-db.service"
    ];
    # Reach the commands in environment.systemPackages, such as MCP servers.
    path = [ "/run/current-system/sw" ];
    serviceConfig = {
      User = cfg.user;
      ExecStart = "${packages.alyosha}/bin/${name}";
      Restart = "on-failure";
      RestartSec = 5;
    };
  };
in
{
  options.services.alyosha = {
    enable = lib.mkEnableOption "the Alyosha container";
    user = lib.mkOption {
      type = lib.types.str;
      description = "The owner, who runs everything in the container.";
    };
    uid = lib.mkOption {
      type = lib.types.int;
      default = 1000;
      description = "The owner's user ID on the host (`id -u`), so files written inside belong to them.";
    };
    workspaces = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ "${home}/work" ];
      defaultText = lib.literalExpression ''[ "/home/<user>/work" ]'';
      description = "Folders shared with the container at the same path. Alyosha works in any folder inside them.";
    };
    hostCommand = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Install `alyosha` on the host, which runs the container's alyosha in the current folder, and let the owner open shells in the container without a password.";
    };
    externalInterface = lib.mkOption {
      type = lib.types.str;
      description = "The host's uplink, for NAT.";
    };
    hostAddress = lib.mkOption {
      type = lib.types.str;
      default = "10.233.1.1";
    };
    localAddress = lib.mkOption {
      type = lib.types.str;
      default = "10.233.1.2";
    };
    memoryMax = lib.mkOption {
      type = lib.types.str;
      default = "12G";
      description = "The memory cap for everything in the container, such as 75% of the machine's RAM.";
    };
    gproxyPort = lib.mkOption {
      type = lib.types.port;
      default = 18787;
      description = "GPROXY's loopback port inside the container; match [gateway] url in config.toml.";
    };
    extraPackages = lib.mkOption {
      type = lib.types.listOf lib.types.package;
      default = [ ];
      description = "More commands for MCP servers.";
    };
    telegram.enable = lib.mkEnableOption "the Telegram bridge";
    qq.enable = lib.mkEnableOption "the QQ bridge";
    stateVersion = lib.mkOption {
      type = lib.types.str;
      default = config.system.stateVersion;
    };
  };

  config = lib.mkIf cfg.enable {
    containers.alyosha = {
      autoStart = true;
      privateNetwork = true;
      inherit (cfg) hostAddress localAddress;

      # Each mount keeps its host path.
      bindMounts = lib.genAttrs cfg.workspaces shared // {
        "${home}/.config/alyosha" = shared "${home}/.config/alyosha";
        "${home}/.local/share/alyosha" = shared "${home}/.local/share/alyosha";
      };

      config =
        { ... }:
        {
          system.stateVersion = cfg.stateVersion;
          users.users.${cfg.user} = {
            isNormalUser = true;
            inherit (cfg) uid;
          };
          # nspawn creates the parent of the Alyosha config mount as root.
          # Chromium needs to create its crash database under ~/.config.
          systemd.tmpfiles.rules = [ "d ${home}/.config 0700 ${cfg.user} - -" ];
          # Dev shells load through the host's Nix daemon.
          nix.settings.experimental-features = [
            "nix-command"
            "flakes"
          ];
          environment.systemPackages = [
            packages.alyosha
            packages.gproxy
            packages.gogcli
            packages.open-websearch
            packages.playwright-mcp
          ]
          ++ cfg.extraPackages;

          # GPROXY, always on; skipped until `alyosha gateway init` wrote its
          # secrets (docs/develop/LLMClientGateway.md).
          systemd.services.gproxy =
            let
              data = "${home}/.local/share/alyosha/gproxy";
              envFile = "${home}/.config/alyosha/gproxy.env";
            in
            {
              wantedBy = [ "multi-user.target" ];
              wants = [ "network-online.target" ];
              after = [ "network-online.target" ];
              unitConfig.ConditionPathExists = envFile;
              serviceConfig = {
                User = cfg.user;
                EnvironmentFile = envFile;
                WorkingDirectory = data;
                ExecStart =
                  "${packages.gproxy}/bin/gproxy serve --host 127.0.0.1 --port ${toString cfg.gproxyPort} --data-dir ${data}"
                  + " --log-format json --log-filter info --update-automatic=false";
                Restart = "on-failure";
                RestartSec = 5;
              };
            };

          # The session database: a Unix socket only, and the owner's role
          # by peer authentication (docs/develop/SessionStore.md#container).
          services.postgresql = {
            enable = true;
            package = postgresql;
            enableTCPIP = false;
            settings.listen_addresses = lib.mkForce "";
          };
          # Creates the owner's role and the `alyosha` database it owns, once.
          systemd.services.alyosha-db = {
            wantedBy = [ "multi-user.target" ];
            requires = [ "postgresql.service" ];
            after = [
              "postgresql.service"
              "postgresql-setup.service"
            ];
            path = [ postgresql ];
            serviceConfig = {
              Type = "oneshot";
              RemainAfterExit = true;
              User = "postgres";
            };
            script = ''
              psql -tAc "select 1 from pg_roles where rolname = '${cfg.user}'" | grep -q 1 \
                || psql -c 'create role "${cfg.user}" login'
              psql -tAc "select 1 from pg_database where datname = 'alyosha'" | grep -q 1 \
                || psql -c 'create database alyosha owner "${cfg.user}"'
            '';
          };
          # A daily dump into the mounted data folder; the last 7 are kept.
          systemd.services.alyosha-db-backup = {
            requires = [ "alyosha-db.service" ];
            after = [ "alyosha-db.service" ];
            startAt = "daily";
            path = [
              postgresql
              pkgs.gzip
              pkgs.findutils
            ];
            serviceConfig = {
              Type = "oneshot";
              User = cfg.user;
            };
            script = ''
              dir=${home}/.local/share/alyosha/backups
              mkdir -p "$dir"
              file="$dir/alyosha-$(date +%F).sql.gz"
              pg_dump alyosha | gzip > "$file.tmp"
              mv "$file.tmp" "$file"
              find "$dir" -name 'alyosha-*.sql.gz' -mtime +7 -delete
            '';
          };

          systemd.services.alyosha-telegram = lib.mkIf cfg.telegram.enable (bridge "alyosha-telegram");
          systemd.services.alyosha-qq = lib.mkIf cfg.qq.enable (bridge "alyosha-qq");
        };
    };

    # Kill processes inside the container instead of freezing the host when
    # memory runs out.
    systemd.services."container@alyosha".serviceConfig.MemoryMax = cfg.memoryMax;

    environment.systemPackages = lib.mkIf cfg.hostCommand [ hostCommand ];
    # machinectl shell asks polkit; allow the owner into this one container
    # as themselves.
    security.polkit.extraConfig = lib.mkIf cfg.hostCommand ''
      polkit.addRule(function(action, subject) {
        if (action.id == "org.freedesktop.machine1.shell" &&
            subject.user == "${cfg.user}" &&
            action.lookup("machine") == "alyosha" &&
            action.lookup("user") == "${cfg.user}") {
          return polkit.Result.YES;
        }
      });
    '';

    networking.nat = {
      enable = true;
      internalInterfaces = [ "ve-+" ];
      inherit (cfg) externalInterface;
    };
  };
}
