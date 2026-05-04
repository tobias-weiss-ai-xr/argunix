# M13b NixOS test: end-to-end build dispatch through the dynamic
# builder pool.
#
# Forces dispatch by giving the test derivation a `requiredSystemFeatures`
# that only the builder advertises. Without dispatch through the pool,
# the medusa node literally cannot build the derivation. So a
# successful realise via `--builders ssh-ng://...?ssh-command=medusa-pipe`
# proves the full transport works end-to-end:
#
#   nix-store --realise            (medusa node, as `medusa` user)
#     -> spawns medusa-pipe        (per the ssh-command= URI param)
#     -> connects to /run/medusa/builders/smoke-builder.sock
#     -> medusa daemon opens a fresh SSH build channel into the agent
#     -> agent spawns `nix-daemon --stdio`
#     -> nix daemon protocol round-trips
#     -> built path streamed back into the medusa store
{ pkgs, ... }:

let
  enrollmentToken = pkgs.writeText "medusa-builder-enrollment-token" "tok";
  githubToken = pkgs.writeText "medusa-test-github-token" "ghtok";
  # Stand-alone derivation expression. Kept as a file so the test
  # script can `nix-instantiate` it from the medusa node — that
  # mints the .drv path into the local store the same way an
  # operator's CI build would.
  derivExpr = pkgs.writeText "medusa-test-deriv.nix" ''
    derivation {
      name = "built-on-builder";
      system = "x86_64-linux";
      builder = "/bin/sh";
      args = [ "-c" "echo built-by-builder > $out" ];
      # Feature only the builder advertises; medusa node refuses to
      # build it locally. Forces dispatch through the pool.
      requiredSystemFeatures = [ "medusa-test" ];
    }
  '';
in
{
  name = "medusa-builder-build-dispatch";

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

    # Default `nix.settings.system-features` does NOT include
    # "medusa-test", so the medusa node is incapable of building
    # the test derivation locally. Any successful realise must have
    # gone through the dynamic pool.
    environment.systemPackages = [ pkgs.medusa ];
    virtualisation.memorySize = 1536;
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

    # Advertise the gating feature so dispatch routes here.
    nix.settings.system-features = [
      "kvm"
      "nixos-test"
      "benchmark"
      "big-parallel"
      "medusa-test"
    ];

    virtualisation.memorySize = 1536;
  };

  testScript = ''
    start_all()
    medusa.wait_for_unit("medusa.service")
    medusa.wait_for_open_port(2222)
    builder.wait_for_unit("medusa-builder.service")

    # Wait for the agent to enrol and the per-builder socket to land.
    medusa.wait_until_succeeds(
        "medusactl --socket /run/medusa/control.sock builders list --json"
        " | tr -d ' \\n' | grep -q '\"connected\":true'",
        timeout=30,
    )
    medusa.wait_until_succeeds(
        "test -S /run/medusa/builders/smoke-builder.sock",
        timeout=10,
    )
    # Builder must have advertised "medusa-test" via its hello.
    medusa.succeed(
        "medusactl --socket /run/medusa/control.sock builders list --json"
        " | tr -d ' \\n' | grep -q 'medusa-test'",
    )

    # Compose the `--builders` arg in the same shape medusa-build's
    # compose_builders_arg produces. We reproduce it here rather than
    # asking medusa to dispatch a build — the daemon's worker is
    # gated behind a webhook, which would require a real forge to
    # mock. Going through nix directly proves the wire protocol.
    pipe = "${pkgs.medusa}/bin/medusa-pipe"
    # Authority `localhost` triggers nix's `fakeSSH=true` which skips
    # the `ssh user@host …` prefix and exec's `remote-program`
    # directly. nix tokenises remote-program on whitespace, so
    # `pipe%20smoke-builder` becomes argv `[pipe, smoke-builder]`,
    # and nix appends `--stdio` afterwards.
    builders_arg = (
        f"ssh-ng://localhost?remote-program={pipe}"
        + "%20smoke-builder x86_64-linux - 1 1 medusa-test -"
    )

    # Instantiate the test derivation.
    drv = medusa.succeed(
        "nix-instantiate ${derivExpr}"
    ).strip()
    assert drv.endswith(".drv"), f"unexpected nix-instantiate output: {drv!r}"

    # Negative control: without --builders, the realise must fail
    # (medusa node lacks the gating feature). This proves the
    # follow-up succeeded *via* dispatch.
    rc, _ = medusa.execute(
        f"sudo -u medusa nix-store --realise {drv} 2>&1"
    )
    assert rc != 0, "medusa node should NOT be able to build medusa-test deriv locally"

    # Positive: realise via the dynamic pool. medusa-pipe forks per
    # connect; the daemon proxies to the agent's nix-daemon --stdio.
    out_path = medusa.succeed(
        f"sudo -u medusa nix-store --realise {drv} "
        f"--builders '{builders_arg}'"
    ).strip()
    assert out_path.startswith("/nix/store/"), f"unexpected output path: {out_path!r}"

    # Sanity-check the build artefact: contents prove we ran the
    # builder script, not just substituted from a cache.
    contents = medusa.succeed(f"cat {out_path}").strip()
    assert contents == "built-by-builder", f"unexpected output: {contents!r}"

    # Regression guard: every opened build channel on the agent side
    # must reach the close log too. Without forwarding russh's
    # Handler::channel_close into the pump, channels stayed open
    # forever and `nix-daemon --stdio` leaked.
    builder.wait_until_succeeds(
        "journalctl -u medusa-builder.service --no-pager"
        " | grep -q 'build channel closed; nix subprocess exited'",
        timeout=10,
    )
  '';
}
