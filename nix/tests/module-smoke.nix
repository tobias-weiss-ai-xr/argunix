# M9-lite NixOS test: enable services.medusa, hit /healthz.
#
# Pass directly to `pkgs.testers.runNixOSTest`. The `pkgs` argument is
# the host's pkgs (with the medusa overlay already applied), so the VM
# inherits `pkgs.medusa` and `pkgs.testers.runNixOSTest` doesn't need a
# `nixpkgs.overlays` override.
#
# Verifies:
#   - The module evaluates and the systemd unit starts.
#   - LoadCredential exposes the forge token under $CREDENTIALS_DIRECTORY
#     and the daemon accepts it. (Webhook secrets are now medusa-managed,
#     stored in sqlite — no operator file involved.)
#   - Port 8080 listens and /healthz returns "ok".
#   - The medusa user is in nix.settings.trusted-users.
#
# This deliberately doesn't exercise the worker pipeline — that's
# already covered by serve-pipeline-smoke.nix and forge-status-smoke.nix.
{ pkgs, ... }:

let
  githubToken = pkgs.writeText "medusa-test-github-token" "tok";
in
{
  name = "medusa-module-smoke";

  nodes.machine =
    { ... }:
    {
      imports = [ ../module.nix ];

      services.medusa = {
        enable = true;
        listen = "127.0.0.1:8080";
        credentials = {
          gh-token = "${githubToken}";
        };
        settings = {
          external_url = "https://medusa.example.com";
          forges.gh = {
            kind = "github";
            api_url = "https://api.github.com";
            token_path = "$CREDENTIALS_DIRECTORY/gh-token";
          };
          # Empty repos[] — without the auto-install pass would try to
          # reach api.github.com from within the test VM. Keep it empty
          # so this test stays purely about module shape.
          repos = [ ];
        };
      };

      environment.systemPackages = [ pkgs.curl ];

      virtualisation.memorySize = 1024;
    };

  testScript = ''
    machine.start()
    machine.wait_for_unit("medusa.service")
    machine.wait_for_open_port(8080)

    out = machine.succeed("curl -fsS http://127.0.0.1:8080/healthz")
    assert out.strip() == "ok", f"unexpected /healthz body: {out!r}"

    machine.succeed("getent passwd medusa")
    machine.succeed("grep -q '^trusted-users.*medusa' /etc/nix/nix.conf")
  '';
}
