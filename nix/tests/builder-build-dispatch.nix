# M14b NixOS test: end-to-end build dispatch through the dynamic
# builder pool over side channels.
#
# Forces dispatch by giving the test derivation a `requiredSystemFeatures`
# that only the builder advertises. Without dispatch through the pool,
# the medusa node literally cannot build the derivation. So a successful
# realise via the side-channel transport proves the full wire protocol:
#
#   medusactl test-dispatch-drv --builder smoke-builder <drv>
#     -> daemon: nix-store --query --requisites <drv> (compute closure)
#     -> daemon: open SSH session channel into agent (ClosurePush)
#     -> daemon: write side-channel header + nix-store --export bytes
#     -> agent:  read header, run nix-store --import (drv + deps land in builder store)
#     -> daemon: send Build over control channel
#     -> agent:  spawn nix-store --realise <drv>, stream stderr as BuildLogChunk frames
#     -> agent:  send BuildFinished{Success, output_paths=[...]}
#     -> daemon: open another SSH session channel (ClosurePull)
#     -> daemon: write header asking for output_paths
#     -> agent:  run nix-store --export <output_paths>, stream stdout
#     -> daemon: pipe bytes into local nix-store --import
#     -> daemon: nix-store --add-root <gcroot> --indirect --realise <output>
#
# The test then asserts the realised path is in the daemon's local
# store with the expected contents — proof every byte made the round-trip.
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
    # Each VM needs its own writable nix store overlay; without
    # this, builds and store imports go into a tmpfs that the VM's
    # nix-daemon doesn't actually own, so `nix copy` and `nix-store
    # --import` see "path does not exist" even when the file is on
    # disk. Default in newer nixpkgs but safer to pin explicitly.
    virtualisation.writableStore = true;
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
    virtualisation.writableStore = true;
  };

  testScript = ''
    import json

    start_all()
    medusa.wait_for_unit("medusa.service")
    medusa.wait_for_open_port(2222)
    builder.wait_for_unit("medusa-builder.service")

    # Wait for the agent to enrol and announce its capabilities.
    medusa.wait_until_succeeds(
        "medusactl --socket /run/medusa/control.sock builders list --json"
        " | tr -d ' \\n' | grep -q '\"connected\":true'",
        timeout=30,
    )
    # Builder must have advertised "medusa-test" via its hello.
    medusa.succeed(
        "medusactl --socket /run/medusa/control.sock builders list --json"
        " | tr -d ' \\n' | grep -q 'medusa-test'",
    )

    # Instantiate the test derivation locally on the medusa node.
    drv = medusa.succeed(
        "nix-instantiate ${derivExpr}"
    ).strip()
    assert drv.endswith(".drv"), f"unexpected nix-instantiate output: {drv!r}"

    # Negative control: a bare `nix-store --realise <drv>` on the
    # medusa node must fail — the host doesn't advertise "medusa-test"
    # and we've removed the legacy --builders fallback. This proves
    # the follow-up succeeded *via* dispatch.
    rc, _ = medusa.execute(
        f"sudo -u medusa nix-store --realise {drv} 2>&1"
    )
    assert rc != 0, "medusa node should NOT be able to build medusa-test deriv locally"

    # Positive: dispatch via the pool. medusactl drives the daemon's
    # side-channel transport end-to-end. The daemon allocates a
    # synthetic build_id, pushes the drv closure, sends Build, drains
    # the lifecycle, pulls the output closure, and registers a gcroot.
    raw = medusa.succeed(
        f"medusactl --socket /run/medusa/control.sock test-dispatch-drv"
        f" --builder smoke-builder {drv}"
    )
    payload = json.loads(raw)
    if payload.get("status") != "success":
        # On failure, dump the per-build zstd log and tails of both
        # daemon journals so the test driver shows what went wrong.
        log_path = payload.get("log_path")
        log_dump = ""
        if log_path:
            rc, log_dump = medusa.execute(f"zstdcat {log_path} || cat {log_path}")
        journal_tail = medusa.succeed(
            "journalctl -u medusa.service --no-pager -n 80"
        )
        builder_journal = builder.succeed(
            "journalctl -u medusa-builder.service --no-pager -n 80"
        )
        raise AssertionError(
            f"test-dispatch-drv reported non-success: {payload!r}\n"
            f"--- build log ({log_path}) ---\n{log_dump}\n"
            f"--- medusa journal tail ---\n{journal_tail}\n"
            f"--- builder journal tail ---\n{builder_journal}\n"
        )
    output_paths = payload.get("output_paths") or []
    assert output_paths, f"no output paths reported: {payload!r}"
    out_path = output_paths[0]
    assert out_path.startswith("/nix/store/"), (
        f"unexpected output path: {out_path!r}"
    )

    # The output must now be present in the medusa node's local
    # store — the daemon-side `nix-store --import` for the pull
    # channel imported it. Read it back and verify the contents.
    contents = medusa.succeed(f"cat {out_path}").strip()
    assert contents == "built-by-builder", (
        f"unexpected output: {contents!r}"
    )

    # Regression guard: the agent must log the build lifecycle so an
    # operator debugging a flaky pool dispatch can correlate.
    builder.wait_until_succeeds(
        "journalctl -u medusa-builder.service --no-pager"
        " | grep -q 'side channel finished'",
        timeout=10,
    )
  '';
}
