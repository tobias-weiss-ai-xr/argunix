# M14b NixOS test: end-to-end build dispatch through the dynamic
# builder pool over side channels.
#
# Forces dispatch by giving the test derivation a `requiredSystemFeatures`
# that only the builder advertises. Without dispatch through the pool,
# the argunix node literally cannot build the derivation. So a successful
# realise via the side-channel transport proves the full wire protocol:
#
#   argunixctl test-dispatch-drv --builder smoke-builder <drv>
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
  enrollmentToken = pkgs.writeText "argunix-builder-enrollment-token" "tok";
  githubToken = pkgs.writeText "argunix-test-github-token" "ghtok";
  # Stand-alone derivation expression. Kept as a file so the test
  # script can `nix-instantiate` it from the argunix node — that
  # mints the .drv path into the local store the same way an
  # operator's CI build would.
  derivExpr = pkgs.writeText "argunix-test-deriv.nix" ''
    derivation {
      name = "built-on-builder";
      system = "x86_64-linux";
      builder = "/bin/sh";
      args = [ "-c" "echo built-by-builder > $out" ];
      # Feature only the builder advertises; argunix node refuses to
      # build it locally. Forces dispatch through the pool.
      requiredSystemFeatures = [ "argunix-test" ];
    }
  '';
in
{
  name = "argunix-builder-build-dispatch";

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
    # "argunix-test", so the argunix node is incapable of building
    # the test derivation locally. Any successful realise must have
    # gone through the dynamic pool.
    environment.systemPackages = [ pkgs.argunix ];
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

    services.argunix-builder = {
      enable = true;
      argunixHost = "argunix";
      argunixPort = 2222;
      enrollmentTokenFile = "${enrollmentToken}";
      name = "smoke-builder";
    };

    # Advertise the gating feature so dispatch routes here.
    nix.settings.system-features = [
      "kvm"
      "nixos-test"
      "benchmark"
      "big-parallel"
      "argunix-test"
    ];

    virtualisation.memorySize = 1536;
    virtualisation.writableStore = true;
  };

  testScript = ''
    import json

    start_all()
    argunix.wait_for_unit("argunix.service")
    argunix.wait_for_open_port(2222)
    builder.wait_for_unit("argunix-builder.service")

    # Wait for the agent to enrol and announce its capabilities.
    argunix.wait_until_succeeds(
        "argunixctl --socket /run/argunix/control.sock builders list --json"
        " | tr -d ' \\n' | grep -q '\"connected\":true'",
        timeout=30,
    )
    # Builder must have advertised "argunix-test" via its hello.
    argunix.succeed(
        "argunixctl --socket /run/argunix/control.sock builders list --json"
        " | tr -d ' \\n' | grep -q 'argunix-test'",
    )

    # Instantiate the test derivation locally on the argunix node.
    drv = argunix.succeed(
        "nix-instantiate ${derivExpr}"
    ).strip()
    assert drv.endswith(".drv"), f"unexpected nix-instantiate output: {drv!r}"

    # Negative control: a bare `nix-store --realise <drv>` on the
    # argunix node must fail — the host doesn't advertise "argunix-test"
    # and we've removed the legacy --builders fallback. This proves
    # the follow-up succeeded *via* dispatch.
    rc, _ = argunix.execute(
        f"sudo -u argunix nix-store --realise {drv} 2>&1"
    )
    assert rc != 0, "argunix node should NOT be able to build argunix-test deriv locally"

    # Positive: dispatch via the pool. argunixctl drives the daemon's
    # side-channel transport end-to-end. The daemon allocates a
    # synthetic build_id, pushes the drv closure, sends Build, drains
    # the lifecycle, pulls the output closure, and registers a gcroot.
    raw = argunix.succeed(
        f"argunixctl --socket /run/argunix/control.sock test-dispatch-drv"
        f" --builder smoke-builder {drv}"
    )
    payload = json.loads(raw)
    if payload.get("status") != "success":
        # On failure, dump the per-build zstd log and tails of both
        # daemon journals so the test driver shows what went wrong.
        log_path = payload.get("log_path")
        log_dump = ""
        if log_path:
            rc, log_dump = argunix.execute(f"zstdcat {log_path} || cat {log_path}")
        journal_tail = argunix.succeed(
            "journalctl -u argunix.service --no-pager -n 80"
        )
        builder_journal = builder.succeed(
            "journalctl -u argunix-builder.service --no-pager -n 80"
        )
        raise AssertionError(
            f"test-dispatch-drv reported non-success: {payload!r}\n"
            f"--- build log ({log_path}) ---\n{log_dump}\n"
            f"--- argunix journal tail ---\n{journal_tail}\n"
            f"--- builder journal tail ---\n{builder_journal}\n"
        )
    output_paths = payload.get("output_paths") or []
    assert output_paths, f"no output paths reported: {payload!r}"
    out_path = output_paths[0]
    assert out_path.startswith("/nix/store/"), (
        f"unexpected output path: {out_path!r}"
    )

    # The output must now be present in the argunix node's local
    # store — the daemon-side `nix-store --import` for the pull
    # channel imported it. Read it back and verify the contents.
    contents = argunix.succeed(f"cat {out_path}").strip()
    assert contents == "built-by-builder", (
        f"unexpected output: {contents!r}"
    )

    # Regression guard: the agent must log the build lifecycle so an
    # operator debugging a flaky pool dispatch can correlate.
    builder.wait_until_succeeds(
        "journalctl -u argunix-builder.service --no-pager"
        " | grep -q 'side channel finished'",
        timeout=10,
    )
  '';
}
