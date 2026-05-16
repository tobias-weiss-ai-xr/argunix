# End-to-end test of the `registry-push` effect: argunix pushing a
# built docker image out to an *external* registry the operator runs.
#
# Counterpart to `registry.nix` (argunix's own embedded read-only
# registry). Here the image leaves argunix entirely: a docker-image job
# triggers the `registry-push` effect (`argunix-effects::RegistryPush`),
# which `skopeo copy`s the docker-archive into a separately-running
# `registry:2` — and a real podman client then pulls and runs it from
# *that* registry.
#
# The fixture's docker-image job builds *cleanly* inside the VM: every
# build input — the image tarball and the static `busybox` that copies
# it — rides in through the flake's `${self}`, so the derivation has a
# real declared input and builds under the normal sandbox. That is the
# load-bearing choice: a `sandbox = false` build with *undeclared*
# dependencies (the `registry.nix` shape) is resource-marginal in a
# test VM and gets SIGKILL'd under pressure; a clean, declared,
# sandboxed `cp` of a static binary is as robust as any other trivial
# fixture build.
#
# `fixtureImage` is the same derivation realised on the host and
# shipped in via `additionalPaths` — an opportunistic cache-hit fast
# path. The job is `Cached` when the drvs coincide and `Success`
# otherwise; post-build effects run for both, so the `registry-push`
# effect fires either way.
{ pkgs, ... }:

let
  argunixPort = 8080;
  registryPort = 5000;
  registryHost = "127.0.0.1:${toString registryPort}";

  # Real `dockerTools.buildLayeredImage` output — a docker-archive
  # tarball. Built on the host at flake-eval time.
  prebuiltImage = pkgs.dockerTools.buildLayeredImage {
    name = "hello-image";
    tag = "fixture";
    contents = [ pkgs.busybox ];
    config.Cmd = [
      "/bin/sh"
      "-c"
      "echo hello-from-argunix-registry-push"
    ];
  };

  # The fixture flake source tree carries everything its one derivation
  # needs: the image tarball and a static `busybox` (no shared-library
  # closure) to copy it. Because the derivation references only
  # `${self}`, it has exactly one declared input — the flake source —
  # and builds cleanly under the normal sandbox.
  flakeNix = pkgs.writeText "flake.nix" ''
    {
      outputs = { self }: {
        packages.x86_64-linux.hello-image = (derivation {
          name = "argunix-registry-push-fixture-image.tar.gz";
          system = "x86_64-linux";
          builder = "''${self}/busybox";
          args = [
            "sh"
            "-c"
            "''${self}/busybox cp ''${self}/hello-image.tar.gz $out"
          ];
        }) // { meta.docker-image = true; };
      };
    }
  '';

  fixtureFlake = pkgs.runCommand "argunix-registry-push-fixture-flake" { } ''
    mkdir -p $out
    cp ${flakeNix} $out/flake.nix
    cp ${prebuiltImage} $out/hello-image.tar.gz
    cp ${pkgs.pkgsStatic.busybox}/bin/busybox $out/busybox
    chmod +x $out/busybox
    cat > $out/flake.lock <<'EOF'
    { "nodes": { "root": {} }, "root": "root", "version": 7 }
    EOF
  '';

  # Host-built realisation of *exactly* the derivation the in-VM flake
  # evaluates to. `${fixtureFlake}` is the same store path the flake
  # sees as `${self}`, so name / system / builder / args — and thus the
  # drv hash and output path — are identical. Shipping this output into
  # the VM is what turns the in-VM docker-image job into a cache hit.
  fixtureImage = derivation {
    name = "argunix-registry-push-fixture-image.tar.gz";
    system = "x86_64-linux";
    builder = "${fixtureFlake}/busybox";
    args = [
      "sh"
      "-c"
      "${fixtureFlake}/busybox cp ${fixtureFlake}/hello-image.tar.gz $out"
    ];
  };

  githubToken = pkgs.writeText "argunix-test-github-token" "tok";
in
{
  name = "argunix-registry-push";

  defaults = {
    networking.dhcpcd.enable = false;
  };

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [ ../module.nix ];

      services.argunix = {
        enable = true;
        listen = "127.0.0.1:${toString argunixPort}";
        settings = {
          external_url = "https://argunix.example.com";
          # Named registry catalog. `insecure` because the local
          # `registry:2` below runs plain HTTP; no `auth_path` because
          # NixOS's dockerRegistry accepts unauthenticated writes.
          registries.local = {
            url = registryHost;
            namespace = "myorg";
            insecure = true;
          };
          forges.gh = {
            kind = "github";
            web_url = "https://github.com";
            token_path = "${githubToken}";
            repos."myorg/myrepo" = {
              # The binding under test: this repo's built docker
              # images get pushed to the `local` registry.
              push_to_registries = [ "local" ];
            };
          };
        };
      };

      # The external registry argunix pushes to — upstream `registry:2`
      # (docker-distribution), plain HTTP on 127.0.0.1:5000, no auth.
      services.dockerRegistry = {
        enable = true;
        listenAddress = "127.0.0.1";
        port = registryPort;
        enableDelete = true;
      };

      # Daemonless podman for the pull+run side.
      virtualisation.containers = {
        enable = true;
        registries.insecure = [ registryHost ];
      };

      environment.systemPackages = [
        pkgs.argunix
        pkgs.nix-eval-jobs
        pkgs.skopeo
        pkgs.podman
        pkgs.zstd
        pkgs.sqlite
        pkgs.curl
      ];

      # No derivation is realised inside the VM (the docker-image job
      # is a cache hit), so the VM needs only modest resources.
      virtualisation = {
        memorySize = 6144;
        diskSize = 8 * 1024;
        writableStore = true;
        writableStoreUseTmpfs = false;
      };
      boot.tmp.useTmpfs = false;

      # The fixture flake source plus the host-realised output of its
      # one derivation. Shipping the output is what makes the in-VM
      # docker-image job a cache hit.
      virtualisation.additionalPaths = [
        fixtureFlake
        fixtureImage
      ];
    };

  testScript = ''
    import re

    machine.start()
    machine.wait_for_unit("argunix.service")
    machine.wait_for_open_port(${toString argunixPort})

    # External registry must be up before argunix pushes to it.
    machine.wait_for_unit("docker-registry.service")
    machine.wait_for_open_port(${toString registryPort})

    # Pull the rendered YAML path from the running unit so the build
    # CLI reuses the module's generated config.
    exec_start = machine.succeed(
        "systemctl show argunix -p ExecStart --value"
    )
    m = re.search(r"--config\s+(\S+)", exec_start)
    assert m, f"could not parse daemon config path from: {exec_start!r}"
    config_path = m.group(1)
    print(f"argunix config: {config_path}")

    # The module must have threaded the registry catalog + the repo's
    # push_to_registries binding into the daemon YAML.
    machine.succeed(f"grep -q 'namespace: myorg' {config_path}")
    machine.succeed(f"grep -q 'push_to_registries' {config_path}")

    # Build with the daemon stopped so a single sqlite writer touches
    # /var/lib/argunix/db.sqlite. The docker-image job either hits the
    # shipped-in cache or builds cleanly under the sandbox; the
    # registry-push effect fires for both.
    machine.succeed("systemctl stop argunix.service")

    out = machine.succeed(
        "cd /var/lib/argunix && sudo -u argunix"
        " argunix build"
        f" --config {config_path}"
        " --src ${fixtureFlake}"
        " --slug myorg/myrepo"
        " --forge gh"
        " --systems x86_64-linux"
        " --git-ref refs/heads/main"
        " --gc-root-dir /var/lib/argunix/gcroots"
        " --log-dir /var/lib/argunix/logs"
        " 2>&1"
    )
    print("--- argunix build summary + tracing ---")
    print(out)

    jobs_dump = machine.succeed(
        "sqlite3 /var/lib/argunix/db.sqlite '.headers on' "
        "'SELECT id, attr_path, status FROM jobs ORDER BY id;'"
    )
    effect_dump = machine.succeed(
        "sqlite3 /var/lib/argunix/db.sqlite '.headers on' "
        "'SELECT id, job_id, kind, target, status, detail FROM effect_runs ORDER BY id;'"
    )
    print("--- jobs ---")
    print(jobs_dump)
    print("--- effect_runs ---")
    print(effect_dump)

    envelope = (
        f"\n\nbuild output:\n{out}\n"
        f"jobs:\n{jobs_dump}\n"
        f"effect_runs:\n{effect_dump}\n"
    )

    # The docker-image job must have come through cleanly — `cached`
    # (drvs coincided) or `success` (clean sandboxed build), never a
    # failure.
    assert "failure=0" in out, f"expected failure=0 in summary{envelope}"
    assert "errors=0" in out, f"expected errors=0 in summary{envelope}"
    assert ("cached=1" in out) or ("success=1" in out), (
        f"expected the docker-image job to be cached or built{envelope}"
    )

    # The registry-push effect must have recorded a terminal `success`
    # `effect_runs` row naming the `local` target. A `running` row
    # left behind would mean the effect hung or the process died
    # mid-push. sqlite3's default output is `|`-separated columns.
    rows = machine.succeed(
        "sqlite3 /var/lib/argunix/db.sqlite "
        "'SELECT kind, target, status FROM effect_runs;'"
    ).strip().splitlines()
    print(f"effect_runs rows: {rows!r}")
    assert "registry-push|local|success" in rows, (
        f"expected a successful registry-push effect_run{envelope}"
    )
    assert not any("running" in r for r in rows), (
        f"an effect_run was left in `running`{envelope}"
    )

    # The external registry itself must now hold the image under both
    # the branch tag and the immutable sha-<short> tag. This is the
    # load-bearing check — it proves the bytes left argunix and landed
    # on infrastructure argunix does not own.
    tags_json = machine.succeed(
        "curl -fsS http://${registryHost}/v2/myorg/hello-image/tags/list"
    )
    print(f"--- /v2/myorg/hello-image/tags/list ---\n{tags_json}")
    assert '"main"' in tags_json, (
        f"expected the `main` tag on the external registry, got: {tags_json!r}"
    )
    assert "sha-" in tags_json, (
        f"expected an immutable sha-<short> tag, got: {tags_json!r}"
    )

    # A real podman client pulls the image *from the external
    # registry* and runs it. The Cmd baked into the dockerTools
    # fixture echoes a known string — that round trip is the proof the
    # pushed image is intact and runnable.
    machine.succeed(
        "podman pull ${registryHost}/myorg/hello-image:main"
    )
    run_out = machine.succeed(
        "podman run --rm ${registryHost}/myorg/hello-image:main"
    ).strip()
    print(f"podman run output: {run_out!r}")
    assert run_out == "hello-from-argunix-registry-push", (
        f"unexpected container output: {run_out!r}"
    )

    # The sha-<short> tag must resolve to the same runnable image.
    sha_tag = re.search(r'"(sha-[0-9a-f]+)"', tags_json)
    assert sha_tag, f"could not find a sha- tag in {tags_json!r}"
    machine.succeed(
        f"podman pull ${registryHost}/myorg/hello-image:{sha_tag.group(1)}"
    )
  '';
}
