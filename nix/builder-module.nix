# argunix-builder NixOS module.
#
# Runs the `argunix-builder` agent as a systemd unit. The agent dials
# argunix over SSH on `argunixPort`, authenticates (TOFU on first
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
  cfg = config.services.argunix-builder;
in
{
  options.services.argunix-builder = {
    enable = lib.mkEnableOption "the argunix builder agent";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.argunix;
      defaultText = lib.literalExpression "pkgs.argunix";
      description = ''
        The package providing the `argunix-builder` binary. Defaults
        to `pkgs.argunix`, the same package the daemon ships with.
      '';
    };

    user = lib.mkOption {
      type = lib.types.str;
      default = "argunix-builder";
      description = ''
        System user the agent runs as. Added to nix's `trusted-users`
        because `nix-store --serve --write` (the per-channel
        subprocess) requires that privilege to push paths into the
        store via the nix daemon.
      '';
    };

    group = lib.mkOption {
      type = lib.types.str;
      default = "argunix-builder";
      description = "System group for the argunix-builder user.";
    };

    stateDir = lib.mkOption {
      type = lib.types.path;
      default = "/var/lib/argunix-builder";
      description = ''
        Working directory of the agent. The persistent ed25519
        identity lives at `<stateDir>/identity.ed25519` and is
        regenerated on first boot. Created and chowned to `cfg.user`
        by `StateDirectory=`.
      '';
    };

    argunixHost = lib.mkOption {
      type = lib.types.str;
      example = "argunix.example.com";
      description = ''
        Hostname or IP of the argunix daemon. Resolved once at
        startup; if DNS changes, restart the unit.
      '';
    };

    argunixPort = lib.mkOption {
      type = lib.types.port;
      default = 2222;
      description = ''
        SSH port on argunix where `builder_enrollment.listen` is
        bound. Must match the daemon's YAML config.
      '';
    };

    name = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = ''
        Builder name reported in the `hello` message. Becomes the
        primary key in argunix's `builders` sqlite table. Defaults to
        the machine's hostname when null.
      '';
    };

    enrollmentTokenFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      example = "/run/agenix/argunix-builder-token";
      description = ''
        Path on the host to a file containing the shared
        builder-enrollment token. Used on first contact, after
        `argunixctl builders revoke`, and — importantly — whenever
        the coordinator no longer recognises this builder's pubkey,
        e.g. after its `builders` table was reset: the agent tries
        pubkey auth first and falls back to the token.

        The file is handed to the agent via systemd's `LoadCredential=`
        — systemd reads it as root at unit start, before the service
        sandbox exists — so the path may be anywhere readable by
        root (an agenix/sops secret, a plain root-owned file) and its
        ownership is irrelevant. It must, however, *exist* whenever
        this option is set, or the unit fails to start.

        Recommendation: leave this set and the file in place
        permanently. The enrollment token is single-purpose (it only
        authorises enrolment) and keeping it available is what lets a
        builder re-enrol unattended after a coordinator database
        reset or a builder re-image. To deliberately move a builder
        to pubkey-only, set this option to `null` rather than
        deleting the file out from under it.

        Avoid `/tmp` and `/var/tmp`: they are cleared on reboot, so
        the token would vanish across a restart.
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
      description = "argunix builder agent";
      home = cfg.stateDir;
    };
    users.groups.${cfg.group} = { };

    # `nix-store --serve --write` requires trusted-user privileges so
    # the nix daemon accepts pushed paths.
    nix.settings.trusted-users = [ cfg.user ];

    systemd.services.argunix-builder = {
      description = "argunix builder agent";
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
            (lib.getExe' cfg.package "argunix-builder")
            "--argunix-host"
            cfg.argunixHost
            "--argunix-port"
            (toString cfg.argunixPort)
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
            # `%d` is systemd's credentials-directory specifier
            # ($CREDENTIALS_DIRECTORY). The token is handed to the
            # agent via `LoadCredential=` below — systemd reads the
            # operator's file as root at unit start, *before* the
            # service sandbox (PrivateTmp, ProtectHome, …) exists, and
            # re-exposes it here. So the operator's path can be
            # anywhere — `/tmp`, an agenix secret, a `/home` dotfile —
            # and ownership does not matter.
            "--enrollment-token-path"
            "%d/enrollment-token"
          ]
        );
        Restart = "on-failure";
        RestartSec = 5;

        # Hand the enrollment token to the agent through systemd's
        # credential mechanism rather than letting it read the
        # operator's path directly: a direct read collides with the
        # sandbox below (`PrivateTmp` gives the service its own empty
        # `/tmp`; `ProtectHome` hides `/home`), which silently
        # presents as "enrollment token file not found".
        LoadCredential = lib.optional (
          cfg.enrollmentTokenFile != null
        ) "enrollment-token:${toString cfg.enrollmentTokenFile}";

        User = cfg.user;
        Group = cfg.group;

        StateDirectory = "argunix-builder";
        StateDirectoryMode = "0700";
        WorkingDirectory = cfg.stateDir;
        RuntimeDirectory = "argunix-builder";

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
