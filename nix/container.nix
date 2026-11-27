# The NixOS container that holds Alyosha and everything it keeps: GPROXY,
# the session database, the bridges, the MCP servers, its secrets, and its
# working folders. Each service runs as its own user, and each shell command
# runs in a unit of its own as `alyosha-run`. The container sees nothing of
# the host but the owner's skills, and reaches the internet but never the
# host or the LAN. On the host, `alyosha` shows status and logs and manages
# secrets. See docs/Deployment.md.
#
# In the host configuration:
#
#   imports = [ inputs.alyosha.nixosModules.container ];
#   services.alyosha = {
#     enable = true;
#     owner = "mjli";
#     externalInterface = "enp4s0";
#     telegram.enable = true;
#     settings = {
#       timezone = "Europe/Amsterdam";
#       telegram.owner_id = 123456789;
#     };
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
  alyosha = packages.alyosha;
  root = "/var/lib/nixos-containers/alyosha";
  state = "/var/lib/alyosha";
  secrets = "/var/lib/alyosha-secrets";
  work = "${state}/work";
  backups = "/var/backup/alyosha";

  # The settings as the configuration file, checked at build time so a wrong
  # key fails the rebuild instead of the start.
  settingsFile = (pkgs.formats.toml { }).generate "config.toml" cfg.settings;
  checkedSettings = pkgs.runCommand "alyosha-config.toml" { } ''
    ${alyosha}/bin/alyosha config check ${settingsFile}
    cp ${settingsFile} $out
  '';

  # Each trusted group's name is its working folder.
  groupNames =
    lib.attrNames (cfg.settings.telegram.trusted_groups or { })
    ++ lib.attrNames (cfg.settings.qq.trusted_groups or { });
  # The working folders that run commands, each with its unit template.
  folders = [
    {
      template = "alyosha-run-owner";
      path = "${work}/owner";
    }
  ]
  ++ map (name: {
    template = "alyosha-run-group-${name}";
    path = "${work}/groups/${name}";
  }) groupNames;

  bridges =
    lib.optional cfg.telegram.enable "alyosha-telegram" ++ lib.optional cfg.qq.enable "alyosha-qq";

  # The private and local ranges the container may not reach.
  privateV4 = [
    "10.0.0.0/8"
    "172.16.0.0/12"
    "192.168.0.0/16"
    "169.254.0.0/16"
    "100.64.0.0/10"
  ];
  privateV6 = [
    "fc00::/7"
    "fe80::/10"
  ];

  # The services that read a secret, restarted when it changes. Every other
  # secret is the bridges' (gateway-key, browser-state.json, MCP secrets).
  secretUsers = {
    "gproxy.env" = [ "gproxy" ];
    "telegram-token" = lib.optional cfg.telegram.enable "alyosha-telegram";
    "qq-secret" = lib.optional cfg.qq.enable "alyosha-qq";
  };

  # The host's `alyosha`: status, logs, secrets, the gateway, backups, and
  # checks. Everything it does needs root, so it asks for sudo itself.
  hostCommand = pkgs.writeShellApplication {
    name = "alyosha";
    # The folders listed hold only names Alyosha checks, so `ls` is fine.
    excludeShellChecks = [ "SC2012" ];
    runtimeInputs = [
      pkgs.systemd
      pkgs.coreutils
      pkgs.gnugrep
      pkgs.gnused
      pkgs.gzip
    ];
    text = ''
      if [ "$(id -u)" != 0 ]; then
        exec /run/wrappers/bin/sudo "$0" "$@"
      fi
      usage() {
        cat <<'EOF'
      Usage:
        alyosha status                        The container's services, secrets, and last backup
        alyosha logs [telegram|qq|gproxy|db] [-f]
                                              A service's journal, or every Alyosha program's
        alyosha secret set NAME               Read a secret (without echo, or from standard input)
        alyosha secret list                   The names of the secrets that are set
        alyosha secret remove NAME            Remove a secret
        alyosha gateway init                  Write GPROXY's secrets and start it
        alyosha gateway login                 Sign in to the model provider
        alyosha backup now|list               Dump the session database, or list the dumps
        alyosha backup restore FILE           Load a dump into an emptied database
        alyosha memory list                   Your memories, read-only
        alyosha shell                         A shell as the agent's user, in its state folder
        alyosha check                         The container checks that need no model account
        alyosha commands --json               Every command, for scripts and coding agents
      EOF
      }
      in_container() {
        systemd-run -M alyosha --quiet --wait --pipe --collect "$@"
      }
      restart_users() {
        case "$1" in
          ${lib.concatStringsSep "\n          " (
            lib.mapAttrsToList (name: units: "${name}) units=\"${lib.concatStringsSep " " units}\" ;;") secretUsers
          )}
          *) units="${lib.concatStringsSep " " bridges}" ;;
        esac
        if [ -n "$units" ]; then
          # shellcheck disable=SC2086
          systemctl -M alyosha try-restart $units
        fi
      }
      valid_secret() {
        if ! [[ "$1" =~ ^[a-z0-9][a-z0-9._-]{0,63}$ ]]; then
          echo "alyosha: $1 is not a valid secret name" >&2
          exit 2
        fi
      }
      case "''${1:-}" in
        status)
          systemctl -M alyosha --no-pager list-units --all --plain --no-legend \
            gproxy.service postgresql.service alyosha-db.service ${
              lib.concatMapStringsSep " " (b: "${b}.service") bridges
            } alyosha-run.slice alyosha-run-firewall.service
          echo "Secrets: $(ls ${root}${secrets} 2>/dev/null | tr '\n' ' ')"
          echo "Last backup: $(ls -t ${backups} 2>/dev/null | head -n 1)"
          ;;
        logs)
          shift
          case "''${1:-}" in
            telegram) shift; exec journalctl -M alyosha -u alyosha-telegram "$@" ;;
            qq) shift; exec journalctl -M alyosha -u alyosha-qq "$@" ;;
            gproxy) shift; exec journalctl -M alyosha -u gproxy "$@" ;;
            db) shift; exec journalctl -M alyosha -u postgresql -u alyosha-db -u alyosha-db-backup "$@" ;;
            *) exec journalctl -M alyosha ${lib.concatMapStringsSep " " (b: "-u ${b}") bridges} -u gproxy "$@" ;;
          esac
          ;;
        secret)
          case "''${2:-}" in
            set)
              name="''${3:?a secret name}"
              valid_secret "$name"
              if [ -t 0 ]; then
                read -rsp "$name: " value
                echo
              else
                value="$(cat)"
              fi
              if [ "$name" = gproxy.env ]; then group=root; mode=0400; else group=alyosha; mode=0440; fi
              printf '%s\n' "$value" | in_container /run/current-system/sw/bin/sh -c \
                "umask 377 && cat > ${secrets}/.$name.new && chgrp $group ${secrets}/.$name.new && chmod $mode ${secrets}/.$name.new && mv ${secrets}/.$name.new ${secrets}/$name"
              restart_users "$name"
              echo "Set $name."
              ;;
            list) ls ${root}${secrets} ;;
            remove)
              name="''${3:?a secret name}"
              valid_secret "$name"
              rm -f "${root}${secrets}/$name"
              restart_users "$name"
              ;;
            *) usage; exit 2 ;;
          esac
          ;;
        gateway)
          case "''${2:-}" in
            init)
              machinectl shell -q root@alyosha ${alyosha}/bin/alyosha gateway init
              systemctl -M alyosha start gproxy
              ;;
            login)
              machinectl shell -q --setenv=TERM="''${TERM:-xterm-256color}" root@alyosha ${alyosha}/bin/alyosha gateway login
              restart_users gateway-key
              ;;
            *) usage; exit 2 ;;
          esac
          ;;
        backup)
          case "''${2:-}" in
            now) systemctl -M alyosha start alyosha-db-backup && ls -t ${backups} | head -n 1 ;;
            list) ls -l ${backups} ;;
            restore)
              file="''${3:?a dump file}"
              systemctl -M alyosha stop ${lib.concatStringsSep " " bridges}
              gunzip -c "$file" | in_container --uid=postgres /run/current-system/sw/bin/sh -c \
                "dropdb --if-exists alyosha && createdb -O alyosha alyosha && psql -q -v ON_ERROR_STOP=1 alyosha"
              systemctl -M alyosha start ${lib.concatStringsSep " " bridges}
              ;;
            *) usage; exit 2 ;;
          esac
          ;;
        memory)
          if [ "''${2:-}" != list ]; then usage; exit 2; fi
          for file in ${root}${state}/memory/owner/*.md; do
            [ -e "$file" ] || continue
            echo "$(basename "$file" .md): $(grep -m1 '^description:' "$file" | cut -d: -f2- | sed 's/^ //')"
          done
          ;;
        shell)
          exec machinectl shell -q --setenv=TERM="''${TERM:-xterm-256color}" alyosha@alyosha \
            /run/current-system/sw/bin/bash -c "cd ${state} && exec bash"
          ;;
        check)
          in_container --uid=alyosha --setenv=PATH=/run/current-system/sw/bin ${alyosha}/bin/alyosha check-units
          ;;
        commands)
          cat <<'EOF'
      [{"command":"status"},{"command":"logs","args":["telegram|qq|gproxy|db","-f"]},
       {"command":"secret set","args":["NAME"]},{"command":"secret list"},{"command":"secret remove","args":["NAME"]},
       {"command":"gateway init"},{"command":"gateway login"},
       {"command":"backup now"},{"command":"backup list"},{"command":"backup restore","args":["FILE"]},
       {"command":"memory list"},{"command":"shell"},{"command":"check"},{"command":"commands --json"}]
      EOF
          ;;
        -h | --help) usage ;;
        *) usage; exit 2 ;;
      esac
    '';
  };

  # A command's unit. The template fixes everything; the instance name is the
  # starting process's ID and a counter (docs/Deployment.md#command-units).
  runUnit = folder: {
    description = "An Alyosha command in ${folder.path}";
    restartIfChanged = false;
    serviceConfig = {
      Type = "exec";
      User = "alyosha-run";
      Group = "alyosha-work";
      Slice = "alyosha-run.slice";
      ExecStart = "${alyosha}/bin/alyosha run-exec %i";
      StandardOutput = "truncate:/run/alyosha-run/%i/output";
      StandardError = "inherit";
      # Keep a finished unit, so its result can be read.
      RemainAfterExit = true;
      WorkingDirectory = "/workspace";
      BindPaths = [
        "${folder.path}:/workspace"
        "/var/cache/alyosha/shell:/home/alyosha/.cache"
      ];
      BindReadOnlyPaths = [
        "${state}/.agents/skills:/skills"
        "/run/alyosha-run/%i:/run/alyosha-run/%i"
      ];
      TemporaryFileSystem = [
        "/var/lib"
        "/var/cache"
        "/home"
        "/run/alyosha-run"
      ];
      InaccessiblePaths = [
        "-/run/postgresql"
        "-/nix/var/nix/daemon-socket"
        "-${backups}"
      ];
      PrivatePIDs = true;
      PrivateTmp = true;
      PrivateIPC = true;
      ProtectSystem = "strict";
      NoNewPrivileges = true;
      RestrictNamespaces = true;
      RestrictSUIDSGID = true;
      LockPersonality = true;
      MemoryMax = cfg.commandMemoryMax;
      MemorySwapMax = 0;
      TasksMax = 4096;
      RuntimeMaxSec = "24h";
      Nice = 10;
      UMask = "0007";
      KillMode = "control-group";
    };
  };
in
{
  options.services.alyosha = {
    enable = lib.mkEnableOption "the Alyosha container";
    owner = lib.mkOption {
      type = lib.types.str;
      description = "The host user whose ~/.agents/skills the container reads.";
    };
    ownerHome = lib.mkOption {
      type = lib.types.str;
      default = "/home/${cfg.owner}";
      defaultText = lib.literalExpression ''"/home/''${owner}"'';
    };
    settings = lib.mkOption {
      type = (pkgs.formats.toml { }).type;
      default = { };
      description = "Alyosha's settings, as /etc/alyosha/config.toml (docs/reference/Configuration.md). No secrets: set those with `alyosha secret set`.";
    };
    hostCommand = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Install the host's `alyosha` command.";
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
    nameservers = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = "Public nameservers for the container. Without them it uses the host's resolv.conf, which must not name a LAN or loopback server.";
    };
    memoryMax = lib.mkOption {
      type = lib.types.str;
      default = "12G";
      description = "The memory cap for everything in the container, such as 75% of the machine's RAM.";
    };
    commandMemoryMax = lib.mkOption {
      type = lib.types.str;
      default = "4G";
      description = "The memory cap for each shell command or background job.";
    };
    gproxyPort = lib.mkOption {
      type = lib.types.port;
      default = 18787;
      description = "GPROXY's loopback port inside the container; match gateway.url in the settings.";
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

      bindMounts = {
        "${state}/.agents/skills" = {
          hostPath = "${cfg.ownerHome}/.agents/skills";
          isReadOnly = true;
        };
        ${backups} = {
          hostPath = backups;
          isReadOnly = false;
        };
      };

      config =
        { ... }:
        {
          system.stateVersion = cfg.stateVersion;

          users.groups = {
            alyosha = { };
            alyosha-run = { };
            alyosha-work = { };
            gproxy = { };
          };
          users.users = {
            alyosha = {
              isSystemUser = true;
              group = "alyosha";
              extraGroups = [ "alyosha-work" ];
              home = state;
            };
            alyosha-run = {
              isSystemUser = true;
              group = "alyosha-run";
              extraGroups = [ "alyosha-work" ];
            };
            gproxy = {
              isSystemUser = true;
              group = "gproxy";
            };
          };

          environment.etc."alyosha/config.toml".source = checkedSettings;

          systemd.tmpfiles.rules = [
            "d ${secrets} 0750 root alyosha -"
            "d ${state} 0750 alyosha alyosha -"
            # Chromium writes its crash database under ~/.config.
            "d ${state}/.config 0700 alyosha alyosha -"
            "d ${state}/gog 0700 alyosha alyosha -"
            "d ${work} 0750 alyosha alyosha-work -"
            "d ${work}/groups 0750 alyosha alyosha-work -"
            "d ${work}/guest 0700 alyosha alyosha -"
            "d /var/cache/alyosha 0755 root root -"
            "d /var/cache/alyosha/shell 0700 alyosha-run alyosha-run -"
            "d /var/cache/alyosha/sessions 0700 alyosha alyosha -"
            "d /run/alyosha-run 2750 alyosha alyosha-work -"
          ]
          ++ map (folder: "d ${folder.path} 2770 alyosha-run alyosha-work -") folders;

          # Dev shells load through the host's Nix daemon.
          nix.settings.experimental-features = [
            "nix-command"
            "flakes"
          ];
          environment.systemPackages = [
            alyosha
            packages.gproxy
            packages.gogcli
            packages.open-websearch
            packages.playwright-mcp
          ]
          ++ cfg.extraPackages;

          networking.useHostResolvConf = lib.mkIf (cfg.nameservers != [ ]) false;
          networking.nameservers = cfg.nameservers;

          services.journald.extraConfig = ''
            SystemMaxUse=500M
            MaxRetentionSec=14day
          '';

          # The session database: a Unix socket only, and the role `alyosha`
          # by peer authentication (docs/SessionStore.md#container).
          services.postgresql = {
            enable = true;
            package = pkgs.postgresql_17;
            enableTCPIP = false;
            settings.listen_addresses = lib.mkForce "";
          };

          systemd.services = lib.mkMerge [
            {
              # GPROXY, always on; skipped until `alyosha gateway init` wrote
              # its secrets (docs/LLMClientGateway.md#run-gproxy-as-a-service).
              gproxy = {
                wantedBy = [ "multi-user.target" ];
                wants = [ "network-online.target" ];
                after = [ "network-online.target" ];
                unitConfig.ConditionPathExists = "${secrets}/gproxy.env";
                serviceConfig = {
                  User = "gproxy";
                  Group = "gproxy";
                  StateDirectory = "gproxy";
                  StateDirectoryMode = "0700";
                  EnvironmentFile = "${secrets}/gproxy.env";
                  WorkingDirectory = "/var/lib/gproxy";
                  ExecStart =
                    "${packages.gproxy}/bin/gproxy serve --host 127.0.0.1 --port ${toString cfg.gproxyPort} --data-dir /var/lib/gproxy"
                    + " --log-format json --log-filter info --update-automatic=false";
                  Restart = "on-failure";
                  RestartSec = 5;
                };
              };

              alyosha-db = {
                wantedBy = [ "multi-user.target" ];
                requires = [ "postgresql.service" ];
                after = [
                  "postgresql.service"
                  "postgresql-setup.service"
                ];
                path = [ pkgs.postgresql_17 ];
                serviceConfig = {
                  Type = "oneshot";
                  RemainAfterExit = true;
                  User = "postgres";
                };
                script = ''
                  psql -tAc "select 1 from pg_roles where rolname = 'alyosha'" | grep -q 1 \
                    || psql -c 'create role alyosha login'
                  psql -tAc "select 1 from pg_database where datname = 'alyosha'" | grep -q 1 \
                    || psql -c 'create database alyosha owner alyosha'
                '';
              };

              # A daily dump to the host's backup folder; the last 7 are kept.
              alyosha-db-backup = {
                requires = [ "alyosha-db.service" ];
                after = [ "alyosha-db.service" ];
                startAt = "daily";
                path = [
                  pkgs.postgresql_17
                  pkgs.gzip
                  pkgs.findutils
                  pkgs.util-linux
                ];
                serviceConfig.Type = "oneshot";
                script = ''
                  umask 077
                  file="${backups}/alyosha-$(date +%F).sql.gz"
                  runuser -u postgres -- pg_dump alyosha | gzip > "$file.tmp"
                  mv "$file.tmp" "$file"
                  find ${backups} -name 'alyosha-*.sql.gz' -mtime +7 -delete
                '';
              };

              # Commands may not reach the container's own loopback, where
              # GPROXY listens. systemd's IPAddressDeny= needs a BPF
              # firewall, which the container lacks, so a cgroup-matched rule
              # does it. It can only be added once the slice's cgroup exists.
              alyosha-run-firewall = {
                wantedBy = [ "multi-user.target" ];
                requires = [ "alyosha-run.slice" ];
                after = [ "alyosha-run.slice" ];
                path = [ pkgs.iptables ];
                serviceConfig = {
                  Type = "oneshot";
                  RemainAfterExit = true;
                };
                script = ''
                  for tables in iptables ip6tables; do
                    $tables -C OUTPUT -o lo -m cgroup --path alyosha-run.slice -j REJECT 2>/dev/null \
                      || $tables -I OUTPUT -o lo -m cgroup --path alyosha-run.slice -j REJECT
                  done
                '';
                preStop = ''
                  for tables in iptables ip6tables; do
                    $tables -D OUTPUT -o lo -m cgroup --path alyosha-run.slice -j REJECT 2>/dev/null || true
                  done
                '';
              };
            }
            (lib.genAttrs bridges (name: {
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
                "alyosha-run-firewall.service"
              ];
              # Reach systemctl and the commands MCP servers use.
              path = [ "/run/current-system/sw" ];
              serviceConfig = {
                User = "alyosha";
                Group = "alyosha";
                ExecStart = "${alyosha}/bin/${name}";
                UMask = "0007";
                Restart = "on-failure";
                RestartSec = 5;
              };
            }))
            (lib.listToAttrs (map (folder: lib.nameValuePair "${folder.template}@" (runUnit folder)) folders))
          ];

          # Started at boot, so its cgroup exists for the loopback rule.
          systemd.slices.alyosha-run = {
            wantedBy = [ "multi-user.target" ];
          };

          # The agent may start, stop, and clear its command units, and
          # nothing else.
          security.polkit.enable = true;
          security.polkit.extraConfig = ''
            polkit.addRule(function(action, subject) {
              if (action.id == "org.freedesktop.systemd1.manage-units" &&
                  subject.user == "alyosha") {
                var unit = action.lookup("unit") || "";
                var verb = action.lookup("verb");
                if (/^alyosha-run-[a-z0-9_-]+@[0-9]+-[0-9]+\.service$/.test(unit) &&
                    (verb == "start" || verb == "stop" || verb == "reset-failed")) {
                  return polkit.Result.YES;
                }
              }
            });
          '';
        };
    };

    systemd.tmpfiles.rules = [
      "d ${cfg.ownerHome}/.agents 0755 ${cfg.owner} - -"
      "d ${cfg.ownerHome}/.agents/skills 0755 ${cfg.owner} - -"
      "d ${backups} 0700 root root -"
    ];

    # Kill processes inside the container instead of freezing the host when
    # memory runs out.
    systemd.services."container@alyosha".serviceConfig.MemoryMax = cfg.memoryMax;

    environment.systemPackages = lib.mkIf cfg.hostCommand [ hostCommand ];

    networking.nat = {
      enable = true;
      internalInterfaces = [ "ve-+" ];
      inherit (cfg) externalInterface;
    };

    # The container reaches the internet, never the host itself or the LAN
    # (docs/Deployment.md#network).
    networking.firewall.extraCommands = ''
      iptables -I nixos-fw -i ve-alyosha -m conntrack --ctstate NEW -j nixos-fw-refuse
      ip6tables -I nixos-fw -i ve-alyosha -m conntrack --ctstate NEW -j nixos-fw-refuse
      ${lib.concatMapStringsSep "\n" (range: "iptables -I FORWARD -i ve-alyosha -d ${range} -j REJECT") privateV4}
      ${lib.concatMapStringsSep "\n" (range: "ip6tables -I FORWARD -i ve-alyosha -d ${range} -j REJECT") privateV6}
    '';
    networking.firewall.extraStopCommands = ''
      iptables -D nixos-fw -i ve-alyosha -m conntrack --ctstate NEW -j nixos-fw-refuse 2>/dev/null || true
      ip6tables -D nixos-fw -i ve-alyosha -m conntrack --ctstate NEW -j nixos-fw-refuse 2>/dev/null || true
      ${lib.concatMapStringsSep "\n" (range: "iptables -D FORWARD -i ve-alyosha -d ${range} -j REJECT 2>/dev/null || true") privateV4}
      ${lib.concatMapStringsSep "\n" (range: "ip6tables -D FORWARD -i ve-alyosha -d ${range} -j REJECT 2>/dev/null || true") privateV6}
    '';
  };
}
