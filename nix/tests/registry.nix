# End-to-end test of the docker-image registry.
#
# A successful build of a derivation marked `meta.image-format = "docker"`
# triggers `argunix-registry::publish`: skopeo copies the
# docker-archive into a content-addressed blob pool, the per-build
# manifest goes into `<state>/manifests/...`, and a `docker_images`
# row is inserted. The same daemon serves the result over HTTP V2 at
# `/v2/<forge>/<owner>/<repo>/<attr>:<tag>` — and a real docker
# client must be able to pull AND run the image.
#
# Strategy:
#
#   1. The argunix systemd unit is enabled so we exercise the
#      module's PATH (which includes pkgs.skopeo).
#   2. The unit is stopped before driving a build, so a single-shot
#      `argunix build` invocation as the argunix user can write into
#      the same `/var/lib/argunix` state dir without sqlite locking
#      against the daemon.
#   3. The fixture flake is trivial (no nixpkgs flake input, no IFD)
#      — its only output is a derivation that `cp`s a prebuilt
#      `dockerTools.buildLayeredImage` tarball into place and
#      attaches `meta.image-format = "docker"`. This keeps eval cheap
#      without sacrificing a real docker-archive payload.
#   4. The unit is restarted after the build, then a real docker
#      client (configured to treat the local registry as insecure
#      since we don't run TLS) pulls and runs the image. The runtime
#      output is asserted character-for-character.
{ pkgs, ... }:

let
  argunixPort = 8080;
  registryHost = "127.0.0.1:${toString argunixPort}";

  # Real `dockerTools.buildLayeredImage` output (a docker-archive
  # tarball at `$out`). Pre-built at flake-eval time so the fixture
  # flake stays free of nixpkgs and IFD: the inner derivation just
  # `cp`s this store path into place.
  prebuiltImage = pkgs.dockerTools.buildLayeredImage {
    name = "hello-image";
    tag = "fixture";
    contents = [ pkgs.busybox ];
    config.Cmd = [
      "/bin/sh"
      "-c"
      "echo hello-from-argunix-registry"
    ];
  };

  # Standalone fixture flake with one output: a derivation that
  # produces a docker-archive tarball (by copying the prebuilt image)
  # and carries `meta.image-format = "docker"`.
  #
  # The derivation routes *every* build input — the image tarball and
  # the static `busybox` that copies it — through `${self}`, the flake
  # source. That gives it one real declared input and lets it build
  # under the normal sandbox: a `sandbox = false` build with
  # *undeclared* dependencies is resource-marginal in a test VM and
  # gets SIGKILL'd under parallel load.
  flakeNix = pkgs.writeText "flake.nix" ''
    {
      outputs = { self }: {
        packages.x86_64-linux.hello-image = (derivation {
          name = "argunix-registry-fixture-image.tar.gz";
          system = "x86_64-linux";
          builder = "''${self}/busybox";
          args = [
            "sh"
            "-c"
            "''${self}/busybox cp ''${self}/hello-image.tar.gz $out"
          ];
        }) // { meta.image-format = "docker"; };
      };
    }
  '';

  fixtureFlake = pkgs.runCommand "argunix-registry-fixture-flake" { } ''
    mkdir -p $out
    cp ${flakeNix} $out/flake.nix
    cp ${prebuiltImage} $out/hello-image.tar.gz
    cp ${pkgs.pkgsStatic.busybox}/bin/busybox $out/busybox
    chmod +x $out/busybox
    cat > $out/flake.lock <<'EOF'
    { "nodes": { "root": {} }, "root": "root", "version": 7 }
    EOF
  '';

  githubToken = pkgs.writeText "argunix-test-github-token" "tok";
in
{
  name = "argunix-registry";
  globalTimeout = 900; # 15 min: embedded registry serve + pull

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
          forges.gh = {
            kind = "github";
            web_url = "https://github.com";
            token_path = "${githubToken}";
            repos."myorg/myrepo" = { };
          };
        };
      };

      # We use podman (daemonless) instead of docker for the
      # pull+run side. Docker's daemon holds a ~150 MiB resident
      # set + heap permanently, and that — plus argunix + nix's
      # build sandbox — was crossing a SIGKILL threshold during
      # the inner fixture-image build in CI ("builder failed due
      # to signal 9"). Podman has the same OCI-distribution
      # semantics for the assertions we care about (pull a real
      # image, run it, capture stdout) without persistent daemon
      # overhead. `registries.insecure` lets it accept the plain-
      # HTTP registry argunix serves.
      virtualisation.containers = {
        enable = true;
        registries.insecure = [ registryHost ];
      };

      environment.systemPackages = [
        pkgs.argunix
        pkgs.nix-eval-jobs
        pkgs.skopeo
        pkgs.podman
        # For decompressing per-job build logs (`*.log.zst`) when the
        # test needs to print them on failure.
        pkgs.zstd
        # The diagnostic + assertion helpers query the daemon's sqlite
        # state directly (`docker_images` rows, job statuses, eval
        # failure_reason).
        pkgs.sqlite
      ];

      virtualisation = {
        # dockerTools layers + docker daemon image cache + nix store
        # for the fixture closure + argunix's own resident set leave
        # the default 1 GiB tight. 4 GiB also tipped over in CI
        # (host running multiple VM tests in parallel under tighter
        # cgroup pressure than a workstation), surfacing as SIGKILL
        # on the inner `cp` build. 8 GiB gives comfortable headroom
        # — bigger than strictly necessary on idle hardware but the
        # test's wall-clock is dominated by the VM boot, not RAM.
        memorySize = 8192;
        diskSize = 8 * 1024;
        writableStore = true;
        # Default `writableStoreUseTmpfs = true` puts the writable
        # overlay on RAM. The fixture build's $out + nix's eval
        # heap + argunix daemon + docker daemon collectively
        # overflow that even at the bumped memory size — keep the
        # store overlay and build-sandbox /tmp disk-backed.
        writableStoreUseTmpfs = false;
      };
      boot.tmp.useTmpfs = false;

      # The fixture flake source carries everything its derivation
      # needs (the image tarball + a static busybox), so the build has
      # a real declared input and runs under the normal sandbox — no
      # `nix.settings.sandbox = false`. Ride the flake source into the
      # VM store explicitly.
      virtualisation.additionalPaths = [
        fixtureFlake
      ];
    };

  testScript = ''
    import re

    machine.start()
    machine.wait_for_unit("argunix.service")
    machine.wait_for_open_port(${toString argunixPort})

    # Pull the rendered YAML path from the running systemd unit so
    # the `argunix build` CLI invocation below reuses the module's
    # generated config file — no test-side YAML to maintain.
    exec_start = machine.succeed(
        "systemctl show argunix -p ExecStart --value"
    )
    m = re.search(r"--config\s+(\S+)", exec_start)
    assert m, f"could not parse daemon config path from: {exec_start!r}"
    config_path = m.group(1)
    print(f"argunix config: {config_path}")

    # Drive the build with the daemon stopped so a single sqlite
    # writer at a time is touching /var/lib/argunix/db.sqlite. The
    # daemon picks back up afterwards to serve the registry.
    machine.succeed("systemctl stop argunix.service")

    # Confirm what the eval pass actually sees on `meta`. If the
    # `image-format` marker is missing here, the publish helper will
    # never even be called downstream — so this is the right
    # diagnostic to read first when docker_images comes up empty.
    eval_json = machine.succeed(
        "sudo -u argunix nix-eval-jobs"
        " --extra-experimental-features 'nix-command flakes'"
        " --flake '${fixtureFlake}#packages.x86_64-linux'"
        " --meta"
    )
    print("--- nix-eval-jobs JSON (raw) ---")
    print(eval_json)
    assert '"image-format":"docker"' in eval_json, (
        "meta.image-format was not surfaced by nix-eval-jobs --meta; "
        f"got: {eval_json!r}"
    )

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
    # `tracing::warn!` from the publish helper formats as JSON and
    # carries the literal text below. Two distinct cases:
    #   - "docker registry publish failed" present → helper ran,
    #     skopeo or the db insert errored.
    #   - absent → helper never ran (spec.image_format was None,
    #     i.e. the meta marker didn't propagate from eval to JobSpec).
    publish_warned = "docker registry publish failed" in out
    print(f"publish helper logged a warning: {publish_warned}")
    # When the helper warned, the line itself carries the actual
    # underlying error (skopeo stderr, sqlx error, etc.). Extract
    # the JSON fields we care about and reformat on multiple lines
    # so a terminal pager doesn't truncate the long single-line
    # tracing-json record.
    import json as _json
    publish_errors = []
    for line in out.splitlines():
        if "docker registry publish failed" not in line:
            continue
        try:
            rec = _json.loads(line)
            fields = rec.get("fields", {})
            publish_errors.append({
                "message": fields.get("message"),
                "error": fields.get("error"),
                "attr": fields.get("attr"),
                "job_id": fields.get("job_id"),
            })
        except Exception:
            publish_errors.append({"raw": line})
    print("--- publish warn (parsed) ---")
    for err in publish_errors:
        for k, v in err.items():
            print(f"  {k}: {v}")

    # On failure the summary alone doesn't say *why* — collect every
    # captured per-job log + the daemon stderr trace and fold it into
    # the assertion message so it travels with whatever failed,
    # regardless of how much output gets quoted back.
    jobs_dump = machine.succeed(
        "sqlite3 /var/lib/argunix/db.sqlite '.headers on' "
        "'SELECT id, attr_path, status, log_path, output_path "
        "FROM jobs ORDER BY id;'"
    )
    print("--- jobs in db ---")
    print(jobs_dump)
    evals_dump = machine.succeed(
        "sqlite3 /var/lib/argunix/db.sqlite "
        "'SELECT id, status, failure_reason FROM evaluations;'"
    )
    print("--- evaluations.failure_reason ---")
    print(evals_dump)
    log_files = machine.succeed(
        "find /var/lib/argunix/logs -type f 2>/dev/null || true"
    ).strip().splitlines()
    per_job_logs = ""
    for log_file in log_files:
        body = machine.succeed(
            f"zstd -d -c {log_file} 2>/dev/null || cat {log_file}"
        )
        print(f"--- {log_file} ---")
        print(body)
        per_job_logs += f"\n--- {log_file} ---\n{body}"

    failure_envelope = (
        f"\n\nbuild output:\n{out}\n"
        f"jobs:\n{jobs_dump}\n"
        f"evals:\n{evals_dump}\n"
        f"per-job logs:{per_job_logs}\n"
    )
    assert "success=1" in out, (
        f"expected success=1 in summary{failure_envelope}"
    )
    assert "failure=0" in out, (
        f"expected failure=0 in summary{failure_envelope}"
    )

    # The publish helper inserts one row per built docker image.
    rows = machine.succeed(
        "sqlite3 /var/lib/argunix/db.sqlite "
        "'SELECT image_name, system, git_ref, manifest_digest FROM docker_images;'"
    ).strip()
    print(f"docker_images rows: {rows!r}")
    # Embed the same envelope used above so the failure traceback by
    # itself tells us whether the publish helper ran (warn line in
    # `out`) and what the per-job log + db state look like.
    pretty_publish_errors = "\n".join(
        "  " + "\n  ".join(f"{k}: {v}" for k, v in err.items())
        for err in publish_errors
    ) or "  (none)"
    assert "gh/myorg/myrepo/hello-image" in rows, (
        f"expected the published image_name in docker_images, got: {rows!r}\n"
        f"publish helper logged a warning: {publish_warned}\n"
        f"publish errors:\n{pretty_publish_errors}\n"
        f"{failure_envelope}"
    )
    assert "x86_64-linux" in rows
    assert "refs/heads/main" in rows
    assert "sha256:" in rows, f"expected a manifest digest, got: {rows!r}"

    # On-disk: blob pool must have at least the manifest + one layer
    # blob. Both files live under /var/lib/argunix/registry-state.
    blobs = machine.succeed(
        "ls /var/lib/argunix/registry-state/blobs"
    ).strip().split()
    print(f"blob pool ({len(blobs)} entries): {blobs}")
    assert len(blobs) >= 2, (
        f"expected manifest + at least one layer in blob pool, got: {blobs}"
    )
    machine.succeed(
        "test -f /var/lib/argunix/registry-state/manifests/*/*/manifest.json"
    )

    # Bring the daemon back up; it now serves /v2/* against the same
    # state dir the build just populated.
    machine.succeed("systemctl start argunix.service")
    machine.wait_for_open_port(${toString argunixPort})

    # /v2/ probe: every Docker Registry V2 client starts here.
    probe = machine.succeed(
        "curl -fsS -i http://${registryHost}/v2/"
    )
    print("--- /v2/ ---")
    print(probe)
    # HTTP/1.1 lowercases header names on the wire (hyper/axum
    # follow the spec's case-insensitive treatment); the substring
    # match has to be case-insensitive.
    assert "docker-distribution-api-version: registry/2.0" in probe.lower(), (
        f"missing required version header on /v2/, got: {probe!r}"
    )

    # Tag-based manifest GET assembles an OCI image index across
    # every per-system row. With one system in this test the index
    # has one entry; the docker client still walks it the same way.
    manifest = machine.succeed(
        "curl -fsS -H 'Accept: application/vnd.oci.image.index.v1+json'"
        " http://${registryHost}/v2/gh/myorg/myrepo/hello-image/manifests/main"
    )
    print("--- /manifests/main ---")
    print(manifest)
    assert '"schemaVersion":2' in manifest
    assert '"mediaType":"application/vnd.oci.image.index.v1+json"' in manifest
    assert '"linux"' in manifest, f"expected linux platform in index: {manifest!r}"
    assert '"amd64"' in manifest, f"expected amd64 platform in index: {manifest!r}"

    # Cross-check the second hop ourselves before the client takes it:
    # the index references each per-arch manifest by digest. If our
    # own /manifests/sha256:... endpoint doesn't return that
    # manifest, podman pull won't either. Collect everything into a
    # single string so the assertion message carries the full
    # picture even when the test driver elides intermediate prints.
    import json as _json2
    index = _json2.loads(manifest)
    docker_images_dump = machine.succeed(
        "sqlite3 /var/lib/argunix/db.sqlite '.headers on' "
        "'SELECT * FROM docker_images;'"
    )
    print("--- docker_images full dump ---")
    print(docker_images_dump)
    print(f"index manifests entries: {index.get('manifests')}")
    digest_probes = ""
    for entry in index.get("manifests", []):
        digest = entry["digest"]
        rc, probe2 = machine.execute(
            f"curl -sS -i http://${registryHost}/v2/gh/myorg/myrepo/hello-image/manifests/{digest}"
        )
        print(f"--- GET /manifests/{digest} (rc={rc}) ---")
        print(probe2)
        digest_probes += f"\n  GET /manifests/{digest} -> rc={rc}\n{probe2}\n"

    # Real podman pull: walks the index, picks the matching child
    # manifest by platform, and downloads each blob via /v2/.../blobs/.
    # Podman is daemonless — pull and run happen as ordinary user-space
    # processes — so it doesn't keep a persistent resident set around
    # while we're driving the eval+build above.
    rc, pull_out = machine.execute(
        "podman pull ${registryHost}/gh/myorg/myrepo/hello-image:main 2>&1"
    )
    assert rc == 0, (
        f"podman pull failed (rc={rc})\n"
        f"podman output:\n{pull_out}\n"
        f"index returned by /manifests/main:\n{manifest}\n"
        f"docker_images:\n{docker_images_dump}\n"
        f"per-digest probes:{digest_probes}"
    )

    # And the pulled image actually runs end-to-end. The Cmd we set
    # in the dockerTools fixture echoes a known string; that round
    # trip is the load-bearing assertion.
    run_out = machine.succeed(
        "podman run --rm ${registryHost}/gh/myorg/myrepo/hello-image:main"
    ).strip()
    print(f"podman run output: {run_out!r}")
    assert run_out == "hello-from-argunix-registry", (
        f"unexpected container output: {run_out!r}"
    )
  '';
}
