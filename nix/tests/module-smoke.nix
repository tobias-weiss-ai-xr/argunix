# NixOS smoke test: enable services.argunix, hit /healthz.
#
# Pass directly to `pkgs.testers.runNixOSTest`. The `pkgs` argument is
# the host's pkgs (with the argunix overlay already applied), so the VM
# inherits `pkgs.argunix` and `pkgs.testers.runNixOSTest` doesn't need a
# `nixpkgs.overlays` override.
#
# Verifies:
#   - The module evaluates and the systemd unit starts.
#   - The daemon reads the forge token from a direct path the static
#     `argunix` user can access. (Webhook secrets are now argunix-managed,
#     stored in sqlite — no operator file involved.)
#   - Port 8080 listens and /healthz returns "ok".
#   - The argunix user is in nix.settings.trusted-users.
#
# This deliberately doesn't exercise the worker pipeline — that's
# already covered by serve-pipeline-smoke.nix and forge-status-smoke.nix.
{ pkgs, ... }:

let
  githubToken = pkgs.writeText "argunix-test-github-token" "tok";
in
{
  name = "argunix-module-smoke";

  nodes.machine =
    { ... }:
    {
      imports = [ ../module.nix ];

      services.argunix = {
        enable = true;
        listen = "127.0.0.1:8080";
        settings = {
          external_url = "https://argunix.example.com";
          forges.gh = {
            kind = "github";
            web_url = "https://github.com";
            token_path = "${githubToken}";
            # Empty repos {} — without it the auto-install pass would
            # try to reach api.github.com from within the test VM.
            # Keep it empty so this test stays purely about module
            # shape.
            repos = { };
          };
        };
      };

      environment.systemPackages = [ pkgs.curl ];

      virtualisation.memorySize = 1024;
    };

  testScript = ''
    machine.start()
    machine.wait_for_unit("argunix.service")
    machine.wait_for_open_port(8080)

    out = machine.succeed("curl -fsS http://127.0.0.1:8080/healthz")
    assert out.strip() == "ok", f"unexpected /healthz body: {out!r}"

    machine.succeed("getent passwd argunix")
    machine.succeed("grep -q '^trusted-users.*argunix' /etc/nix/nix.conf")

    # Static assets: the package ships a Tailwind-compiled `ui.css`
    # and an `img/` directory under `$out/share/argunix/static`; the
    # module wires `web.static_dir` to that path. Past versions left
    # `static_dir` defaulted to a non-existent relative path, so
    # every `/static/...` request 404'd and the UI rendered with no
    # styling. Asserting both: the CSS exists and contains the
    # tailwind banner (proves it was built, not the placeholder).
    css = machine.succeed("curl -fsS http://127.0.0.1:8080/static/ui.css")
    assert "tailwindcss" in css, f"expected tailwind-compiled CSS, got: {css[:200]!r}"
    assert len(css) > 1000, f"unexpectedly short CSS ({len(css)} bytes)"
    machine.succeed("curl -fsS -o /dev/null http://127.0.0.1:8080/static/img/argunix.svg")

    # Shutdown drain: systemctl stop must return cleanly within the
    # unit's TimeoutStopSec (default 90s). Past versions hung
    # because:
    #   - mpsc Sender clones never dropped (worker drained forever);
    #   - axum's `with_graceful_shutdown` waited on a parked
    #     keep-alive HTTP connection that never closed.
    # Open a long-lived keep-alive connection so the second pathway
    # would hang the daemon if it ever regressed; assert the unit
    # still stops within a few seconds.
    machine.succeed(
        "exec 3<>/dev/tcp/127.0.0.1/8080;"
        " printf 'GET /healthz HTTP/1.1\\r\\nHost: x\\r\\nConnection: keep-alive\\r\\n\\r\\n' >&3;"
        " head -c 1 <&3 >/dev/null;"  # consume one byte of the response
        " exec 3<>/dev/null &"  # background; nothing closes FD 3 explicitly
    )
    import time
    t0 = time.monotonic()
    machine.succeed("timeout 25 systemctl stop argunix.service")
    elapsed = time.monotonic() - t0
    assert elapsed < 20, f"argunix.service took {elapsed:.1f}s to stop — drain likely hung"
    machine.succeed("systemctl is-failed argunix.service || true")
  '';
}
