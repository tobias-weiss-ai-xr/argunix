# M13b NixOS test: end-to-end builder enrollment.
#
# Two nodes:
#   - `medusa`  — the daemon with `builder_enrollment` configured.
#   - `builder` — the agent dialing medusa.
#
# Asserts the agent reaches `Active` in medusa's registry within a
# reasonable timeout, surfaced via `medusactl builders --json`.
{ pkgs, ... }:

let
  enrollmentToken = pkgs.writeText "medusa-builder-enrollment-token" "tok";
  githubToken = pkgs.writeText "medusa-test-github-token" "ghtok";
in
{
  name = "medusa-builder-enrollment";

  nodes.medusa = {
    imports = [ ../module.nix ];

    services.medusa = {
      enable = true;
      listen = "127.0.0.1:8080";
      credentials = {
        gh-token = "${githubToken}";
        builder-enrollment-token = "${enrollmentToken}";
      };
      settings = {
        external_url = "https://medusa.example.com";
        builder_enrollment = {
          # `[::]:2222` listens on both IPv4 and IPv6 — the test
          # network gives each node both, and the agent's
          # `lookup_host` may return either family first.
          listen = "[::]:2222";
          token_path = "$CREDENTIALS_DIRECTORY/builder-enrollment-token";
        };
        forges.gh = {
          kind = "github";
          api_url = "https://api.github.com";
          token_path = "$CREDENTIALS_DIRECTORY/gh-token";
          repos = { };
        };
      };
    };

    # Module auto-opens settings.builder_enrollment.listen — no
    # operator-side firewall config needed.
    environment.systemPackages = [ pkgs.medusa ];
    virtualisation.memorySize = 1024;
  };

  nodes.builder = {
    imports = [ ../builder-module.nix ];

    services.medusa-builder = {
      enable = true;
      medusaHost = "medusa";
      medusaPort = 2222;
      enrollmentTokenFile = "${enrollmentToken}";
      name = "smoke-builder";
    };

    virtualisation.memorySize = 1024;
  };

  testScript = ''
    start_all()
    medusa.wait_for_unit("medusa.service")
    medusa.wait_for_open_port(2222)
    builder.wait_for_unit("medusa-builder.service")

    # Builder dials medusa, enrols via TOFU password auth, and lands
    # in the registry as `active`. The agent's first dial races
    # against medusa's startup; the reconnect-backoff loop covers it.
    # JSON output is pretty-printed (spaces after `:`), so match on
    # the values rather than literal `"key":"value"` substrings.
    medusa.wait_until_succeeds(
        "medusactl --socket /run/medusa/control.sock builders list --json"
        " | grep -q smoke-builder",
        timeout=30,
    )
    medusa.wait_until_succeeds(
        "medusactl --socket /run/medusa/control.sock builders list --json"
        " | tr -d ' \\n' | grep -q '\"connected\":true'",
        timeout=30,
    )

    # SocketServer side: the per-builder unix socket exists for the
    # lifetime of the Active connection. medusa-pipe will connect to
    # this socket on every dispatch. Owned by the medusa user.
    medusa.wait_until_succeeds(
        "test -S /run/medusa/builders/smoke-builder.sock",
        timeout=10,
    )
    medusa.succeed(
        "stat -c '%U' /run/medusa/builders/smoke-builder.sock | grep -q '^medusa$'",
    )
  '';
}
