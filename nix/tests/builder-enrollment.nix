# NixOS test: end-to-end builder enrollment.
#
# Two nodes:
#   - `argunix`  — the daemon with `builder_enrollment` configured.
#   - `builder` — the agent dialing argunix.
#
# Asserts the agent reaches `Active` in argunix's registry within a
# reasonable timeout, surfaced via `argunixctl builders --json`.
{ pkgs, ... }:

let
  enrollmentToken = pkgs.writeText "argunix-builder-enrollment-token" "tok";
  githubToken = pkgs.writeText "argunix-test-github-token" "ghtok";
in
{
  name = "argunix-builder-enrollment";

  nodes.argunix = {
    imports = [ ../module.nix ];

    services.argunix = {
      enable = true;
      listen = "127.0.0.1:8080";
      credentials = {
        gh-token = "${githubToken}";
        builder-enrollment-token = "${enrollmentToken}";
      };
      settings = {
        external_url = "https://argunix.example.com";
        builder_enrollment = {
          # `[::]:2222` listens on both IPv4 and IPv6 — the test
          # network gives each node both, and the agent's
          # `lookup_host` may return either family first.
          listen = "[::]:2222";
          token_path = "$CREDENTIALS_DIRECTORY/builder-enrollment-token";
        };
        forges.gh = {
          kind = "github";
          web_url = "https://github.com";
          token_path = "$CREDENTIALS_DIRECTORY/gh-token";
          repos = { };
        };
      };
    };

    # Module auto-opens settings.builder_enrollment.listen — no
    # operator-side firewall config needed.
    environment.systemPackages = [ pkgs.argunix ];
    virtualisation.memorySize = 1024;
  };

  nodes.builder = {
    imports = [ ../builder-module.nix ];

    services.argunix-builder = {
      enable = true;
      argunixHost = "argunix";
      argunixPort = 2222;
      enrollmentTokenFile = "${enrollmentToken}";
      name = "smoke-builder";
    };

    virtualisation.memorySize = 1024;
  };

  testScript = ''
    start_all()
    argunix.wait_for_unit("argunix.service")
    argunix.wait_for_open_port(2222)
    builder.wait_for_unit("argunix-builder.service")

    # Builder dials argunix, enrols via TOFU password auth, and lands
    # in the registry as `active`. The agent's first dial races
    # against argunix's startup; the reconnect-backoff loop covers it.
    # JSON output is pretty-printed (spaces after `:`), so match on
    # the values rather than literal `"key":"value"` substrings.
    argunix.wait_until_succeeds(
        "argunixctl --socket /run/argunix/control.sock builders list --json"
        " | grep -q smoke-builder",
        timeout=30,
    )
    argunix.wait_until_succeeds(
        "argunixctl --socket /run/argunix/control.sock builders list --json"
        " | tr -d ' \\n' | grep -q '\"connected\":true'",
        timeout=30,
    )
  '';
}
