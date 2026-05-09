# NixOS test: enable services.argunix-builder, assert the unit
# comes up and the agent runs through identity + capability discovery
# before entering its reconnect-backoff loop.
#
# A full end-to-end test (argunix node + builder node, agent reaches
# the `Active` state in the registry) lands once the daemon's
# `BuilderServer` is wired up — see TODO at argunix-daemon/src/main.rs.
# Until then, this single-node smoke test verifies the module shape:
#   - The systemd unit evaluates and starts.
#   - The user exists and is in `nix.settings.trusted-users`.
#   - The agent's persistent identity is generated under StateDir.
#   - LoadCredential exposes the enrollment token under
#     $CREDENTIALS_DIRECTORY (and the agent doesn't crash on it).
#   - The agent logs `capabilities discovered` (proves
#     `nix show-config --json` succeeded inside the sandbox).
{ pkgs, ... }:

let
  enrollmentToken = pkgs.writeText "argunix-builder-test-token" "tok";
in
{
  name = "argunix-builder-module-smoke";

  nodes.machine = {
    imports = [ ../builder-module.nix ];

    services.argunix-builder = {
      enable = true;
      # No argunix is running here; the agent enters its
      # connection-refused / backoff loop. That's expected — what
      # we assert below is that the unit reached its run loop in
      # the first place.
      argunixHost = "127.0.0.1";
      argunixPort = 2222;
      enrollmentTokenFile = "${enrollmentToken}";
      name = "smoke-builder";
    };

    virtualisation.memorySize = 1024;
  };

  testScript = ''
    machine.start()
    machine.wait_for_unit("argunix-builder.service")

    # User + nix trust.
    machine.succeed("getent passwd argunix-builder")
    machine.succeed("grep -q '^trusted-users.*argunix-builder' /etc/nix/nix.conf")

    # Persistent identity file is created on first start.
    machine.wait_until_succeeds(
        "test -s /var/lib/argunix-builder/identity.ed25519",
        timeout=20,
    )

    # Capability discovery ran (proves `nix show-config --json` works
    # under the unit's PATH and sandbox profile).
    machine.wait_until_succeeds(
        "journalctl -u argunix-builder.service --no-pager | grep -q 'capabilities discovered'",
        timeout=20,
    )

    # Agent reached the dial loop and is logging connect-or-backoff
    # events — proves the enrollment-token credential was loaded and
    # the binary is past startup.
    machine.wait_until_succeeds(
        "journalctl -u argunix-builder.service --no-pager | grep -q 'agent starting'",
        timeout=20,
    )
  '';
}
