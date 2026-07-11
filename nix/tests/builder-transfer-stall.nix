# NixOS test: a saturated/stalled closure transfer must NOT get a
# healthy builder evicted (regression for the 2026-07-10 incident).
#
# The incident mechanism, reproduced deterministically: mid-push, the
# builder's nix-daemon stops draining (here: SIGSTOP on its cgroup once
# ≥32 MiB of import bytes have demonstrably flowed), the agent-side
# side-channel pump stalls, the russh per-channel buffer fills, and the
# agent's single session loop parks — heartbeats stop on a perfectly
# alive TCP connection. Before the fix, the coordinator's liveness
# watchdog evicted the builder at 95s and both agent timers (also 95s)
# raced it; the eviction failed jobs over to whatever else advertised
# the system.
#
# Pinned behavior after the fix:
#   - coordinator: NO "went silent past the liveness threshold"
#     eviction while the builder has an in-flight Push/Pull phase
#     (LIVENESS_MAX_SILENCE_TRANSFER applies, registry.rs);
#   - agent: its own self-liveness watchdog (60s, well under the
#     coordinator's 95s) tears the wedged session down and reconnects
#     as a *displacement* — the builder never leaves the registry;
#   - the wedged dispatch surfaces as a transport failure, and once
#     the daemon is resumed a re-dispatch on the SAME builder succeeds.
{ pkgs, ... }:

let
  enrollmentToken = pkgs.writeText "argunix-builder-enrollment-token" "tok";
  githubToken = pkgs.writeText "argunix-test-github-token" "ghtok";
  # 256 MiB input source: far more than the SSH channel window (32 MiB)
  # + socket buffers can absorb, and big enough that the mid-flight
  # SIGSTOP below reliably lands while most of it is still in transit.
  blobMib = 256;
  bigBlob = pkgs.runCommand "argunix-test-big-blob" { } ''
    head -c ${toString (blobMib * 1024 * 1024)} /dev/zero > $out
  '';
  derivExpr = pkgs.writeText "argunix-transfer-stall-deriv.nix" ''
    derivation {
      name = "built-after-stall";
      system = "x86_64-linux";
      builder = "/bin/sh";
      # `builtins.storePath` re-attaches string context at
      # nix-instantiate time (a bare interpolated path string would
      # carry none), so the blob lands in the drv's inputSrcs and the
      # push actually moves it over the wire.
      blob = builtins.storePath "${bigBlob}";
      # Shell builtins only — the sandbox /bin/sh ships no coreutils.
      # The blob's presence is what matters: nix refuses to start the
      # builder at all unless every inputSrc (incl. the 256 MiB blob)
      # was imported, so reaching this line proves the push landed.
      args = [
        "-c"
        "[ -e \"$blob\" ] && echo pushed-ok > $out"
      ];
      # Feature only the builder advertises; forces pool dispatch.
      requiredSystemFeatures = [ "argunix-test" ];
    }
  '';
in
{
  name = "argunix-builder-transfer-stall";
  globalTimeout = 25 * 60; # 2 VMs, a 256 MiB push, a 130s stall window

  defaults = {
    networking.dhcpcd.enable = false;
  };

  nodes.argunix = {
    imports = [ ../module.nix ];

    services.argunix = {
      enable = true;
      listen = "127.0.0.1:8080";
      settings = {
        external_url = "https://argunix.example.com";
        builder_enrollment = {
          listen = "[::]:2222";
          token_path = "${enrollmentToken}";
        };
        forges.gh = {
          kind = "github";
          web_url = "https://github.com";
          token_path = "${githubToken}";
          repos = { };
        };
      };
    };

    environment.systemPackages = [ pkgs.argunix ];
    virtualisation.memorySize = 2048;
    virtualisation.diskSize = 4096;
    virtualisation.writableStore = true;
    virtualisation.writableStoreUseTmpfs = false;
    # The blob must pre-exist in the argunix node's store so the push
    # actually moves it over the wire.
    virtualisation.additionalPaths = [ bigBlob ];
  };

  nodes.builder = {
    imports = [ ../builder-module.nix ];

    services.argunix-builder = {
      enable = true;
      argunixHost = "argunix";
      argunixPort = 2222;
      enrollmentTokenFile = "${enrollmentToken}";
      name = "stall-builder";
    };

    nix.settings.system-features = [
      "kvm"
      "nixos-test"
      "benchmark"
      "big-parallel"
      "argunix-test"
    ];

    virtualisation.memorySize = 2048;
    virtualisation.diskSize = 4096;
    virtualisation.writableStore = true;
    # The 256 MiB import must land on disk, not the tmpfs overlay.
    virtualisation.writableStoreUseTmpfs = false;
  };

  testScript = ''
    import json
    import time

    start_all()
    argunix.wait_for_unit("argunix.service")
    argunix.wait_for_open_port(2222)
    builder.wait_for_unit("argunix-builder.service")

    argunix.wait_until_succeeds(
        "argunixctl --socket /run/argunix/control.sock builders list --json"
        " | tr -d ' \\n' | grep -q '\"connected\":true'",
        timeout=30,
    )

    drv = argunix.succeed(
        "nix-instantiate ${derivExpr}"
    ).strip()
    assert drv.endswith(".drv"), f"unexpected nix-instantiate output: {drv!r}"

    # nix-daemon is socket-activated; start it explicitly so its cgroup
    # exists for the SIGSTOP below.
    builder.succeed("systemctl start nix-daemon.service")

    # Throttle the coordinator's egress (the push direction) to
    # ~2.5 MB/s so the 256 MiB push takes ~100s — a wide, deterministic
    # window in which to freeze the daemon demonstrably mid-transfer.
    # Heartbeats run builder→coordinator and are unaffected.
    argunix.succeed(
        "tc qdisc add dev eth1 root tbf rate 20mbit burst 128kb latency 400ms"
    )
    rx_baseline = int(
        builder.succeed("cat /sys/class/net/eth1/statistics/rx_bytes").strip()
    )

    # Kick off the dispatch in the background.
    argunix.succeed(
        "argunixctl --socket /run/argunix/control.sock test-dispatch-drv"
        f" --builder stall-builder {drv}"
        " > /tmp/stalled-dispatch.json 2>&1 &"
    )

    # Wait until ≥8 MiB of push bytes have crossed the wire — the
    # daemon-protocol handshake is long done and bulk data fills the
    # pipeline — then freeze the daemon's whole cgroup. From here the
    # agent-side pump backs up: duplex, unix-socket buffer, and russh
    # channel buffer fill, and the agent's session loop parks exactly
    # like the incident. (SIGSTOP the cgroup via systemd — a plain
    # `pkill -f` could also catch the agent process, whose cmdline
    # mentions the daemon socket path.)
    try:
        builder.wait_until_succeeds(
            "[ $(cat /sys/class/net/eth1/statistics/rx_bytes)"
            f" -ge $(({rx_baseline} + 8 * 1024 * 1024)) ]",
            timeout=120,
        )
    except Exception:
        print(argunix.succeed("journalctl -u argunix.service --no-pager -n 80"))
        print(builder.succeed("journalctl -u argunix-builder.service --no-pager -n 80"))
        print(argunix.execute("cat /tmp/stalled-dispatch.json")[1])
        raise
    builder.succeed("systemctl kill --signal=SIGSTOP nix-daemon.service")

    # Sit out the window in which the OLD behavior evicted (95s + 15s
    # watchdog scan, with margin). The agent's own 60s self-liveness
    # watchdog fires in here and reconnects — as a displacement, so the
    # builder never leaves the registry.
    time.sleep(130)

    # THE regression assertion: no liveness eviction happened while the
    # builder had an in-flight transfer phase.
    argunix.fail(
        "journalctl -u argunix.service --no-pager"
        " | grep -q 'went silent past the liveness threshold'"
    )
    # The agent detected the wedged session itself and reconnected.
    builder.succeed(
        "journalctl -u argunix-builder.service --no-pager"
        " | grep -q 'self-liveness watchdog fired'"
    )
    # ...and the builder is (still / again) registered.
    argunix.succeed(
        "argunixctl --socket /run/argunix/control.sock builders list --json"
        " | tr -d ' \\n' | grep -q '\"connected\":true'"
    )

    # Thaw the daemon, lift the shaper, and prove the builder is fully
    # functional on the SAME registration: a fresh dispatch of the same
    # drv succeeds and the output round-trips (the blob byte count).
    builder.succeed("systemctl kill --signal=SIGCONT nix-daemon.service")
    argunix.succeed("tc qdisc del dev eth1 root")
    raw = argunix.succeed(
        "argunixctl --socket /run/argunix/control.sock test-dispatch-drv"
        f" --builder stall-builder {drv}",
        timeout=600,
    )
    payload = json.loads(raw)
    if payload.get("status") != "success":
        log_path = payload.get("log_path")
        log_dump = ""
        if log_path:
            rc, log_dump = argunix.execute(f"zstdcat {log_path} || cat {log_path}")
        journal_tail = argunix.succeed(
            "journalctl -u argunix.service --no-pager -n 100"
        )
        builder_journal = builder.succeed(
            "journalctl -u argunix-builder.service --no-pager -n 100"
        )
        raise AssertionError(
            f"post-thaw dispatch failed: {payload!r}\n"
            f"--- build log ({log_path}) ---\n{log_dump}\n"
            f"--- argunix journal tail ---\n{journal_tail}\n"
            f"--- builder journal tail ---\n{builder_journal}\n"
        )
    out_path = (payload.get("output_paths") or [""])[0]
    contents = argunix.succeed(f"cat {out_path}").strip()
    assert contents == "pushed-ok", f"unexpected output: {contents!r}"
  '';
}
