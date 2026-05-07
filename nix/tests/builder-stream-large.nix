# M16 stress test: prove that the closure-pull path streams per-file
# with bounded memory, by transferring an output that is *much*
# bigger than each VM's RAM and capping each daemon's cgroup memory
# below the file size.
#
# What's being verified:
#   - `nix copy --from ssh-ng://localhost?remote-program=...` does not
#     buffer the NAR end-to-end. With the legacy `nix-store --import`
#     path, a 5 GiB single-NAR pull peaked the daemon at ~3 GiB RSS;
#     the M16 daemon-protocol path should stay well below the cap
#     enforced by `MemoryMax=512M`.
#   - The full closure round-trip works for outputs that don't fit
#     in tmpfs — the writable-store overlay must be on disk
#     (`writableStoreUseTmpfs = false`) for either VM to even hold
#     the file.
#   - On success the test prints a characteristics block so an
#     operator triaging a regression can see the actual RAM, disk,
#     cgroup peaks, and elapsed times next to the configured
#     limits.
{ pkgs, ... }:

let
  enrollmentToken = pkgs.writeText "argunix-builder-enrollment-token" "tok";
  githubToken = pkgs.writeText "argunix-test-github-token" "ghtok";

  # File size produced by the test derivation. Set well above each
  # VM's RAM so a buffering implementation would OOM the cgroup
  # before transfer completes. 2 GiB on 1 GiB-RAM VMs gives a 2×
  # margin — enough to falsify any buffered implementation, small
  # enough to keep the test under a few minutes and the disk image
  # under 10 GiB. Bump to 5 GiB only if you specifically want to
  # match a production OOM profile.
  bigFileMib = 512;

  # The test derivation. Define it normally with `pkgs.runCommand`
  # so its build inputs (stdenv → coreutils → bash → glibc → …) are
  # tracked and end up in the closure properly. We DON'T realise
  # `largeBlob` at flake-eval time (the host can't — it lacks the
  # `argunix-test` system feature). Instead we ship its
  # `inputDerivation` to both VMs (which collects all build inputs
  # into a single drv with no special features required), so when
  # the agent runs `nix-store --realise` on the .drv inside the
  # builder VM, every input is already present and registered.
  largeBlob =
    pkgs.runCommand "stream-large-blob"
      {
        requiredSystemFeatures = [ "argunix-test" ];
      }
      ''
        dd if=/dev/zero of=$out bs=1M count=${toString bigFileMib} status=none
      '';

  # Nix expression file the argunix VM nix-instantiate's at runtime.
  # Imports nixpkgs from a path we ship (`pkgs.path`), so
  # `runCommand` is available with full input-tracking. This is
  # what makes the resulting .drv usable: when the argunix daemon
  # then `nix copy --to`s it to the builder, all build-input drvs
  # are in the closure (already in both VMs' stores via
  # `additionalPaths`).
  #
  # Inside the writeText's `''…''`: `'''` is the literal `''`
  # escape (so the inner `runCommand` body uses `''…''`), and
  # `''$` is the literal `$` escape (to keep `$out` as a shell
  # variable, not Nix interpolation).
  derivExpr = pkgs.writeText "argunix-test-large-deriv.nix" ''
    let
      pkgs = import ${pkgs.path} {};
      env = {
        requiredSystemFeatures = [ "argunix-test" ];
      };
    in
      pkgs.runCommand "stream-large-blob" env '''
        dd if=/dev/zero of=$out bs=1M count=${toString bigFileMib} status=none
      '''
  '';

  # Memory cap enforced on each argunix* systemd unit. Below the
  # file size by design — if streaming works, peak stays well
  # under this; if anything starts buffering NARs, the cgroup
  # OOM-killer fires and the test fails loudly.
  memoryMax = "512M";

  # VM RAM: also below file size. Belt + braces — even with
  # MemoryMax disabled, a buffered import would hit the kernel's
  # OOM killer at this RAM level.
  vmRamMib = 1024;

  # Disk image size — must comfortably hold the file twice (source
  # store on builder + dest store on argunix) plus the system
  # closure. 16 GiB is generous; smaller wedges the test under
  # tmpfs/qcow2 pressure.
  vmDiskMib = 16384;
in
{
  name = "argunix-builder-stream-large";

  nodes.argunix =
    { pkgs, ... }:
    {
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

      environment.systemPackages = [
        pkgs.argunix
        pkgs.jq
      ];

      virtualisation.memorySize = vmRamMib;
      virtualisation.diskSize = vmDiskMib;
      virtualisation.writableStore = true;
      # Pre-stage everything `nix-instantiate` and `nix copy --to`
      # need for the test deriv on the argunix side: the nixpkgs
      # source so `import` works, and the build-input closure of
      # `largeBlob` (= stdenv + coreutils + bash + …) so the resulting
      # drv's closure can be assembled and pushed to the builder
      # without an internet substituter.
      virtualisation.additionalPaths = [
        pkgs.path
        largeBlob.inputDerivation
      ];
      # CRITICAL: with the default `writableStoreUseTmpfs = true`, the
      # upper overlay layer is RAM-backed. A multi-GiB output cannot
      # fit in 1 GiB of RAM no matter how well the protocol streams;
      # the test would OOM the kernel before testing what it's
      # supposed to test. Disk-backed overlay = the file actually
      # lands on disk, and the streaming claim becomes falsifiable.
      virtualisation.writableStoreUseTmpfs = false;
      # Same reasoning for /tmp: nix's build sandbox lives under
      # /tmp by default. With tmpfs-backed /tmp, `dd … of=$out` for
      # an output that doesn't fit in RAM OOMs the sandbox before
      # the build can finish. Move /tmp to disk.
      boot.tmp.useTmpfs = false;

      # Cgroup cap on the daemon. If `nix copy --from` ever reverts
      # to buffering NARs, the daemon's cgroup will exceed this and
      # systemd OOM-kills the unit.
      systemd.services.argunix.serviceConfig = {
        MemoryMax = memoryMax;
        MemoryAccounting = true;
      };
    };

  nodes.builder =
    { pkgs, ... }:
    {
      imports = [ ../builder-module.nix ];

      services.argunix-builder = {
        enable = true;
        argunixHost = "argunix";
        argunixPort = 2222;
        enrollmentTokenFile = "${enrollmentToken}";
        name = "smoke-builder";
      };

      nix.settings.system-features = [
        "kvm"
        "nixos-test"
        "benchmark"
        "big-parallel"
        "argunix-test"
      ];

      virtualisation.memorySize = vmRamMib;
      virtualisation.diskSize = vmDiskMib;
      virtualisation.writableStore = true;
      virtualisation.writableStoreUseTmpfs = false;
      # Same as on the argunix node — nixpkgs source + build-input
      # closure of the test deriv. The agent's `nix-store --realise`
      # finds every input under /nix/store on this VM, runs the
      # build inside the standard sandbox, and produces the 2 GiB
      # output to disk.
      virtualisation.additionalPaths = [
        pkgs.path
        largeBlob.inputDerivation
      ];
      # Builds happen here — sandbox /tmp must be disk-backed.
      boot.tmp.useTmpfs = false;

      # Cap the agent unit too. Same reasoning, mirrored side: if the
      # agent's `nix-daemon --stdio` (forwarded via socket) buffered
      # a NAR for the export, this catches it.
      systemd.services.argunix-builder.serviceConfig = {
        MemoryMax = memoryMax;
        MemoryAccounting = true;
      };
    };

  testScript = ''
    import json
    import time

    expected_bytes = ${toString bigFileMib} * 1024 * 1024

    start_all()
    argunix.wait_for_unit("argunix.service")
    argunix.wait_for_open_port(2222)
    builder.wait_for_unit("argunix-builder.service")

    argunix.wait_until_succeeds(
        "argunixctl --socket /run/argunix/control.sock builders list --json"
        " | jq '.[].connected' | grep -q 'true'",
        timeout=30,
    )

    # Mint the .drv into the argunix VM's own store at runtime via
    # nix-instantiate. The .nix expression has no
    # Nix-store-tracked inputs, so this works without an internet
    # substituter and produces a drv whose closure is just itself.
    drv = argunix.succeed("nix-instantiate ${derivExpr}").strip()
    assert drv.endswith(".drv"), f"unexpected nix-instantiate output: {drv!r}"

    t0 = time.monotonic()
    raw = argunix.succeed(
        f"argunixctl --socket /run/argunix/control.sock test-dispatch-drv"
        f" --builder smoke-builder {drv}",
        timeout=600,  # 5 GiB transfer over QEMU virtio is slow
    )
    elapsed = time.monotonic() - t0
    payload = json.loads(raw)

    if payload.get("status") != "success":
        log_path = payload.get("log_path")
        log_dump = ""
        if log_path:
            _rc, log_dump = argunix.execute(f"zstdcat {log_path} || cat {log_path}")
        argunix_journal = argunix.succeed(
            "journalctl -u argunix.service --no-pager -n 200"
        )
        builder_journal = builder.succeed(
            "journalctl -u argunix-builder.service --no-pager -n 200"
        )
        # Build itself ran on the builder via the system nix-daemon;
        # if the build failed, the actual stderr is in nix-daemon's
        # journal, not in argunix-builder's.
        nix_daemon_journal = builder.succeed(
            "journalctl -u nix-daemon.service --no-pager -n 200"
        )
        # Also peek at builder VM disk + RAM usage at the time of
        # failure — the most common reason `dd` of a multi-GiB file
        # fails in a NixOS test is sandbox tmpfs running out.
        df_out = builder.succeed("df -h /tmp /nix/store /var/tmp 2>&1; echo ---; free -h")
        raise AssertionError(
            f"test-dispatch-drv reported non-success: {payload!r}\n"
            f"--- build log ---\n{log_dump}\n"
            f"--- argunix journal ---\n{argunix_journal}\n"
            f"--- builder argunix-builder journal ---\n{builder_journal}\n"
            f"--- builder nix-daemon journal ---\n{nix_daemon_journal}\n"
            f"--- builder disk/RAM ---\n{df_out}\n"
        )

    out_paths = payload.get("output_paths") or []
    assert out_paths, f"no output paths reported: {payload!r}"
    out_path = out_paths[0]

    # Integrity: file size must match exactly on both VMs.
    argunix_size = int(argunix.succeed(f"stat -c %s {out_path}").strip())
    builder_size = int(builder.succeed(f"stat -c %s {out_path}").strip())
    assert argunix_size == expected_bytes, (
        f"argunix-side size {argunix_size} != expected {expected_bytes}"
    )
    assert builder_size == expected_bytes, (
        f"builder-side size {builder_size} != expected {expected_bytes}"
    )

    # Streaming proof: the unit's MemoryPeak must stay well under
    # the cap. systemd reports it in bytes via show -p MemoryPeak.
    def memory_peak(machine, service):
        line = machine.succeed(
            f"systemctl show {service} -p MemoryPeak --value"
        ).strip()
        # `0` or `[not set]` if accounting was off — in our config
        # it's always on, so we expect a real number.
        return int(line) if line.isdigit() else 0

    argunix_peak = memory_peak(argunix, "argunix.service")
    builder_peak = memory_peak(builder, "argunix-builder.service")

    cap_bytes = ${
      toString (
        let
          m = builtins.match "([0-9]+)([A-Za-z]+)" memoryMax;
          n = builtins.fromJSON (builtins.elemAt m 0);
          unit = builtins.elemAt m 1;
          mult =
            if unit == "M" then
              1024 * 1024
            else if unit == "G" then
              1024 * 1024 * 1024
            else
              1;
        in
        n * mult
      )
    }

    # The streaming claim: peak < cap. (Cap < file size by
    # construction, so peak < file size transitively.)
    assert argunix_peak < cap_bytes, (
        f"argunix daemon peaked at {argunix_peak} bytes, cap is {cap_bytes}; "
        f"a buffered NAR would have OOM-killed the cgroup before this assert ran, "
        f"but if it didn't, this catches a near-miss."
    )
    assert builder_peak < cap_bytes, (
        f"builder agent peaked at {builder_peak} bytes, cap is {cap_bytes}"
    )

    # Final summary block. Printed unconditionally on success so the
    # nixos-test driver log shows the actual numbers next to the
    # configured limits — easy to eyeball "did anything drift" on
    # follow-up runs.
    def fmt(n):
        for unit in ("B", "KiB", "MiB", "GiB"):
            if n < 1024 or unit == "GiB":
                return f"{n:.1f} {unit}" if isinstance(n, float) or n >= 1024 else f"{n} {unit}"
            n = n / 1024
        return str(n)

    vm_ram_mib = ${toString vmRamMib}
    vm_disk_mib = ${toString vmDiskMib}
    memory_max_str = "${memoryMax}"

    print("")
    print("=" * 72)
    print("argunix stream-large test summary")
    print("=" * 72)
    print("Test goal: prove `nix copy --from` streams per-file (no NAR buffer)")
    print("")
    print(f"Output file size:           {expected_bytes:>14} B  ({fmt(expected_bytes)})")
    print("")
    print("VM resources (each):")
    print(f"  RAM:                      {vm_ram_mib * 1024 * 1024:>14} B  ({vm_ram_mib} MiB)")
    print(f"  Disk image:               {vm_disk_mib * 1024 * 1024:>14} B  ({vm_disk_mib} MiB)")
    print("  writableStore:            true (overlay on DISK; useTmpfs=false)")
    print("")
    print("Cgroup memory caps (systemd MemoryMax):")
    print(f"  argunix.service:           {cap_bytes:>14} B  ({memory_max_str})")
    print(f"  argunix-builder.service:   {cap_bytes:>14} B  ({memory_max_str})")
    print("")
    print("Observed peak RSS (systemd MemoryPeak):")
    headroom_m = (cap_bytes - argunix_peak) * 100.0 / cap_bytes
    headroom_b = (cap_bytes - builder_peak) * 100.0 / cap_bytes
    print(f"  argunix.service:           {argunix_peak:>14} B  ({fmt(argunix_peak)}, {headroom_m:.1f}% headroom under cap)")
    print(f"  argunix-builder.service:   {builder_peak:>14} B  ({fmt(builder_peak)}, {headroom_b:.1f}% headroom under cap)")
    print("")
    print("Wall clock:")
    print(f"  test-dispatch-drv total:  {elapsed:>14.2f} s")
    if elapsed > 0:
        throughput = expected_bytes / elapsed / 1024 / 1024
        print(f"  effective throughput:     {throughput:>14.1f} MiB/s")
    print("")
    print(f"Output path:                {out_path}")
    print(f"  on argunix:                {argunix_size} bytes (matches expected)")
    print(f"  on builder:               {builder_size} bytes (matches expected)")
    print("=" * 72)
  '';
}
