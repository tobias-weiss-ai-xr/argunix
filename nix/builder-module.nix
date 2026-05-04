# medusa-builder NixOS module (M13b).
#
# Runs the `medusa-builder` agent as a systemd unit. The agent dials
# medusa over SSH on `medusaPort`, authenticates (TOFU on first
# contact via the enrollment token, pubkey thereafter), and serves
# inbound build channels by spawning `nix-store --serve --write` per
# channel.
#
# The agent is the *client* of the SSH connection — there is no
# inbound port to open in the firewall on the builder side.
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.medusa-builder;
in
{
  options.services.medusa-builder = {
    enable = lib.mkEnableOption "the medusa builder agent";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.medusa;
      defaultText = lib.literalExpression "pkgs.medusa";
      description = ''
        The package providing the `medusa-builder` binary. Defaults
        to `pkgs.medusa`, the same package the daemon ships with.
      '';
    };

    user = lib.mkOption {
      type = lib.types.str;
      default = "medusa-builder";
      description = ''
        System user the agent runs as. Added to nix's `trusted-users`
        because `nix-store --serve --write` (the per-channel
        subprocess) requires that privilege to push paths into the
        store via the nix daemon.
      '';
    };

    group = lib.mkOption {
      type = lib.types.str;
      default = "medusa-builder";
      description = "System group for the medusa-builder user.";
    };

    stateDir = lib.mkOption {
      type = lib.types.path;
      default = "/var/lib/medusa-builder";
      description = ''
        Working directory of the agent. The persistent ed25519
        identity lives at `<stateDir>/identity.ed25519` and is
        regenerated on first boot. Created and chowned to `cfg.user`
        by `StateDirectory=`.
      '';
    };

    medusaHost = lib.mkOption {
      type = lib.types.str;
      example = "medusa.example.com";
      description = ''
        Hostname or IP of the medusa daemon. Resolved once at
        startup; if DNS changes, restart the unit.
      '';
    };

    medusaPort = lib.mkOption {
      type = lib.types.port;
      default = 2222;
      description = ''
        SSH port on medusa where `builder_enrollment.listen` is
        bound. Must match the daemon's YAML config.
      '';
    };

    name = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = ''
        Builder name reported in the `hello` message. Becomes the
        primary key in medusa's `builders` sqlite table. Defaults to
        the machine's hostname when null.
      '';
    };

    enrollmentTokenFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      example = "/run/agenix/medusa-builder-token";
      description = ''
        Path on the host to a file containing the shared
        builder-enrollment token. Used only on first contact (or
        after `medusactl builders revoke`); subsequent connects use
        the persistent pubkey. Once medusa has the row, the operator
        can wipe the file and unset this option — the agent keeps
        running on pubkey auth.

        Loaded into the unit via `LoadCredential=`, so the actual
        contents are never on disk inside the unit's namespace.
      '';
    };

    nixBin = lib.mkOption {
      type = lib.types.str;
      default = "${pkgs.nix}/bin/nix";
      defaultText = lib.literalExpression ''"''${pkgs.nix}/bin/nix"'';
      description = ''
        Path to the `nix` binary the agent invokes for
        `show-config --json` (capability discovery). The
        `nix-store --serve --write` subprocess is also resolved via
        the unit's `PATH=`.
      '';
    };

    extraPackages = lib.mkOption {
      type = lib.types.listOf lib.types.package;
      default = [ ];
      description = ''
        Extra packages to add to the unit's `PATH`. The defaults
        already include `nix` (for `nix-store --serve --write`) and
        `git`. Use this for build tools the actual derivations
        require — anything they don't pick up via `requiredSystemFeatures`
        and nix-store substitution.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    users.users.${cfg.user} = {
      isSystemUser = true;
      inherit (cfg) group;
      description = "medusa builder agent";
      home = cfg.stateDir;
    };
    users.groups.${cfg.group} = { };

    # `nix-store --serve --write` requires trusted-user privileges so
    # the nix daemon accepts pushed paths.
    nix.settings.trusted-users = [ cfg.user ];

    systemd.services.medusa-builder = {
      description = "medusa builder agent";
      after = [
        "network-online.target"
        "nix-daemon.service"
      ];
      wants = [ "network-online.target" ];
      wantedBy = [ "multi-user.target" ];

      path = [
        pkgs.nix
        pkgs.git
      ]
      ++ cfg.extraPackages;

      serviceConfig = {
        Type = "simple";
        ExecStart = lib.concatStringsSep " " (
          [
            (lib.getExe' cfg.package "medusa-builder")
            "--medusa-host"
            cfg.medusaHost
            "--medusa-port"
            (toString cfg.medusaPort)
            "--state-dir"
            cfg.stateDir
            "--nix-bin"
            cfg.nixBin
          ]
          ++ lib.optionals (cfg.name != null) [
            "--name"
            cfg.name
          ]
          ++ lib.optionals (cfg.enrollmentTokenFile != null) [
            "--enrollment-token-path"
            "%d/enrollment-token"
          ]
        );
        Restart = "on-failure";
        RestartSec = 5;

        User = cfg.user;
        Group = cfg.group;

        StateDirectory = "medusa-builder";
        StateDirectoryMode = "0700";
        WorkingDirectory = cfg.stateDir;
        RuntimeDirectory = "medusa-builder";

        LoadCredential = lib.optional (
          cfg.enrollmentTokenFile != null
        ) "enrollment-token:${toString cfg.enrollmentTokenFile}";

        # Sandboxing — same conservative-but-realistic profile as the
        # daemon module. The agent itself is a small Rust binary that
        # only needs network + the nix daemon socket; we leave the
        # store strictly read-only and let the nix daemon do the
        # actual writes via its socket.
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        ProtectControlGroups = true;
        ProtectClock = true;
        ProtectHostname = true;
        ProtectProc = "invisible";
        RestrictNamespaces = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        LockPersonality = true;
        RestrictAddressFamilies = [
          "AF_UNIX"
          "AF_INET"
          "AF_INET6"
        ];
        SystemCallArchitectures = "native";
        UMask = "0077";
      };
    };
  };

  meta.maintainers = [ ];
}
