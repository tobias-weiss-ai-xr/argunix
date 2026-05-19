# End-to-end test of the `registry-push` effect for an *OCI* image —
# a derivation marked `meta.image-format = "oci"`.
#
# Counterpart to `registry-push.nix`, which covers the `docker`
# format. Here the fixture's build output is an `oci-archive` tarball
# (`oci-layout` + `index.json` + `blobs/`): argunix must select the
# `oci-archive:` skopeo transport — not `docker-archive:` — pass
# `--multi-arch all`, copy the image into a separately-running
# `registry:2`, and a real podman client must then pull and run it
# from *that* registry.
#
# The fixture is single-architecture. That still exercises the whole
# OCI path: transport selection, the `--multi-arch all` flag (a
# harmless no-op on a single-image archive), and OCI media types
# surviving the round trip onto the registry — the test asserts the
# pushed manifest is `application/vnd.oci.image.*`, which a wrongly
# chosen `docker-archive:` transport could not even produce (skopeo
# would fail to read the OCI tar as a docker-archive). Assembling a
# true cross-system multi-arch *index* is future work (see
# `argunix-effects::registry`), so there is no manifest list here.
#
# The OCI archive itself is produced *by the test script*, not at
# flake-eval time: `skopeo copy docker-archive:… oci-archive:…` stages
# through a writable `/var/tmp` that a live NixOS system has but the
# nix build sandbox does not (and `TMPDIR` does not override it). The
# test script converts the prebuilt docker image, drops the result
# into a writable copy of the fixture flake, and the fixture's job is
# then a clean, sandboxed `cp` of that archive out of `${self}` —
# exactly the shape `registry-push.nix` uses for the docker case.
#
# This test also covers the `sbom-attach` effect (`design/sbom.md`):
# with no flake cooperation at all, argunix reads the image's runtime
# contents straight out of its OCI layer blobs, generates a CycloneDX
# SBOM, and attaches it to the pushed image as an OCI *referrer* — a
# separate artifact, leaving the image bytes untouched. The test then
# rediscovers and pulls the SBOM back with `oras`.
{ pkgs, ... }:

let
  argunixPort = 8080;
  registryPort = 5000;
  registryHost = "127.0.0.1:${toString registryPort}";

  # A real `dockerTools.buildLayeredImage` output — a docker-archive.
  # The test script converts this to an OCI archive in-VM.
  prebuiltImage = pkgs.dockerTools.buildLayeredImage {
    name = "oci-image";
    tag = "fixture";
    contents = [ pkgs.busybox ];
    config.Cmd = [
      "/bin/sh"
      "-c"
      "echo hello-from-argunix-oci-push"
    ];
  };

  # The fixture flake. Its one derivation references *only* `${self}`,
  # so it has a single declared input and builds cleanly under the
  # normal sandbox: a static `busybox` copies the OCI archive (placed
  # alongside it by the test script) to `$out`. Flagged
  # `meta.image-format = "oci"`.
  flakeNix = pkgs.writeText "flake.nix" ''
    {
      outputs = { self }: {
        packages.x86_64-linux.oci-image = (derivation {
          name = "argunix-registry-push-oci-fixture-image.tar";
          system = "x86_64-linux";
          builder = "''${self}/busybox";
          args = [
            "sh"
            "-c"
            "''${self}/busybox cp ''${self}/oci-image.tar $out"
          ];
        }) // { meta.image-format = "oci"; };
      };
    }
  '';

  # Host-built base of the fixture flake — everything fixed at eval
  # time. The OCI archive is missing on purpose: the test script
  # converts it in-VM and adds it to a writable copy of this tree.
  fixtureFlakeBase = pkgs.runCommand "argunix-registry-push-oci-fixture-base" { } ''
    mkdir -p $out
    cp ${flakeNix} $out/flake.nix
    cp ${pkgs.pkgsStatic.busybox}/bin/busybox $out/busybox
    chmod +x $out/busybox
    cat > $out/flake.lock <<'EOF'
    { "nodes": { "root": {} }, "root": "root", "version": 7 }
    EOF
  '';

  githubToken = pkgs.writeText "argunix-test-github-token" "tok";
in
{
  name = "argunix-registry-push-oci";
  globalTimeout = 900; # 15 min: build + OCI registry push

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
              # The binding under test: this repo's built images get
              # pushed to the `local` registry.
              push_to_registries = [ "local" ];
            };
          };
        };
      };

      # The external registry argunix pushes to — upstream `registry:2`
      # (docker-distribution), plain HTTP on 127.0.0.1:5000, no auth.
      # It accepts OCI media types as readily as docker ones.
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
        # The `sbom-attach` effect shells out to `oras` to attach the
        # CycloneDX SBOM as an OCI referrer; the test script also uses
        # it to rediscover and pull the SBOM back.
        pkgs.oras
        pkgs.podman
        pkgs.gnutar
        pkgs.zstd
        pkgs.sqlite
        pkgs.curl
      ];

      # The fixture's job is a trivial `cp` (a cache miss, but tiny),
      # so the VM needs only modest resources.
      virtualisation = {
        memorySize = 6144;
        diskSize = 8 * 1024;
        writableStore = true;
        writableStoreUseTmpfs = false;
      };
      boot.tmp.useTmpfs = false;

      # The host-built fixture base and the prebuilt docker image the
      # test script converts.
      virtualisation.additionalPaths = [
        fixtureFlakeBase
        prebuiltImage
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

    # Assemble the fixture flake in a writable dir: the host-built base
    # plus the OCI archive, produced now by converting the prebuilt
    # docker image. skopeo's docker-archive reader stages through a
    # writable `/var/tmp`, which the live system has — doing this here
    # rather than in a sandboxed build job is what keeps the fixture's
    # job a clean copy out of the flake source alone.
    machine.succeed("cp -rL ${fixtureFlakeBase} /tmp/fixture")
    # Make the tree writable so the OCI archive can be added; keep the
    # builder `busybox` executable (the copy out of the read-only store
    # must not have dropped its mode bits).
    machine.succeed("chmod -R u+w /tmp/fixture && chmod 0755 /tmp/fixture/busybox")
    machine.succeed(
        "skopeo --insecure-policy copy"
        " docker-archive:${prebuiltImage} oci:/tmp/oci-layout"
    )
    # gzip-compress the outer archive, matching the `oci-image-*.tar.gz`
    # shape nix produces — argunix must decompress it before scanning.
    machine.succeed("tar -czf /tmp/fixture/oci-image.tar -C /tmp/oci-layout .")

    # Build with the daemon stopped so a single sqlite writer touches
    # /var/lib/argunix/db.sqlite.
    machine.succeed("systemctl stop argunix.service")

    out = machine.succeed(
        "cd /var/lib/argunix && sudo -u argunix"
        " argunix build"
        f" --config {config_path}"
        " --src /tmp/fixture"
        " --slug myorg/myrepo"
        " --forge gh"
        " --systems x86_64-linux"
        # A bare branch name — the shape the daemon stores for a push
        # eval (it strips `refs/heads/`). Exercises the production
        # `git_ref` form, not just the CLI's raw `refs/heads/…`.
        " --git-ref main"
        " --gc-root-dir /var/lib/argunix/gcroots"
        " --log-dir /var/lib/argunix/logs"
        " 2>&1"
    )
    print("--- argunix build summary + tracing ---")
    print(out)

    # Surface the per-job build log on any trouble.
    build_logs = machine.succeed(
        "for f in $(find /var/lib/argunix/logs -type f 2>/dev/null); do"
        " echo \"=== $f ===\";"
        " case \"$f\" in *.zst) zstd -dc \"$f\";; *) cat \"$f\";; esac;"
        " done || echo '(no logs)'"
    )
    print("--- build logs ---")
    print(build_logs)

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
        f"build logs:\n{build_logs}\n"
        f"jobs:\n{jobs_dump}\n"
        f"effect_runs:\n{effect_dump}\n"
    )

    # The fixture job must have built cleanly.
    assert "failure=0" in out, f"expected failure=0 in summary{envelope}"
    assert "errors=0" in out, f"expected errors=0 in summary{envelope}"
    assert "success=1" in out, (
        f"expected the oci-image job to build{envelope}"
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

    # The external registry must now hold the image under both the
    # branch tag and the immutable sha-<short> tag. This proves the
    # bytes left argunix via the `oci-archive:` transport and landed
    # on infrastructure argunix does not own.
    tags_json = machine.succeed(
        "curl -fsS http://${registryHost}/v2/myorg/oci-image/tags/list"
    )
    print(f"--- /v2/myorg/oci-image/tags/list ---\n{tags_json}")
    assert '"main"' in tags_json, (
        f"expected the `main` tag on the external registry, got: {tags_json!r}"
    )
    assert "sha-" in tags_json, (
        f"expected an immutable sha-<short> tag, got: {tags_json!r}"
    )

    # The load-bearing OCI assertion: the manifest stored on the
    # registry must be an OCI manifest, not a docker schema2 one. The
    # build output is an `oci-archive`, so the correct `oci-archive:`
    # transport preserves OCI media types end to end; a
    # `docker-archive:` transport could not have produced this (and
    # would have failed to read the tar at all).
    raw_manifest = machine.succeed(
        "skopeo inspect --raw"
        " --tls-verify=false"
        " docker://${registryHost}/myorg/oci-image:main"
    )
    print(f"--- raw manifest ---\n{raw_manifest}")
    assert "vnd.oci.image" in raw_manifest, (
        f"pushed manifest is not OCI media type{envelope}\n"
        f"raw manifest:\n{raw_manifest}"
    )

    # A real podman client pulls the image *from the external
    # registry* and runs it. The Cmd baked into the fixture echoes a
    # known string — that round trip proves the pushed OCI image is
    # intact and runnable.
    machine.succeed(
        "podman pull ${registryHost}/myorg/oci-image:main"
    )
    run_out = machine.succeed(
        "podman run --rm ${registryHost}/myorg/oci-image:main"
    ).strip()
    print(f"podman run output: {run_out!r}")
    assert run_out == "hello-from-argunix-oci-push", (
        f"unexpected container output: {run_out!r}"
    )

    # The sha-<short> tag must resolve to the same runnable image.
    sha_tag = re.search(r'"(sha-[0-9a-f]+)"', tags_json)
    assert sha_tag, f"could not find a sha- tag in {tags_json!r}"
    machine.succeed(
        f"podman pull ${registryHost}/myorg/oci-image:{sha_tag.group(1)}"
    )

    # --- SBOM ---------------------------------------------------------
    # The `sbom-attach` effect must have read the image's `/nix/store`
    # contents out of its OCI layer blobs (no flake cooperation),
    # generated a CycloneDX SBOM, and attached it to the pushed image
    # as an OCI referrer. Recorded as its own `effect_runs` row.
    assert "sbom-attach|local|success" in rows, (
        f"expected a successful sbom-attach effect_run{envelope}"
    )

    sha = sha_tag.group(1)
    image_ref = "${registryHost}/myorg/oci-image"

    # The SBOM must be discoverable as a referrer of the pushed image,
    # carrying the standard CycloneDX artifact type. The image itself
    # is untouched — the SBOM is a separate artifact whose manifest
    # `subject` points back at the image.
    discover = machine.succeed(f"oras discover --plain-http {image_ref}:{sha}")
    print(f"--- oras discover {image_ref}:{sha} ---\n{discover}")
    assert "application/vnd.cyclonedx+json" in discover, (
        f"SBOM referrer not discoverable via oras{envelope}\n"
        f"oras discover:\n{discover}"
    )

    # Pull the SBOM artifact back by digest and confirm it is a real
    # CycloneDX document naming a component from the image's closure —
    # proof the SBOM has content, not just an empty manifest.
    m = re.search(
        r"application/vnd\.cyclonedx\+json.*?(sha256:[0-9a-f]+)",
        discover,
        re.DOTALL,
    )
    assert m, f"could not find the SBOM digest in oras discover output{envelope}"
    sbom_digest = m.group(1)
    print(f"SBOM referrer digest: {sbom_digest}")

    machine.succeed("rm -rf /tmp/sbom-dl && mkdir -p /tmp/sbom-dl")
    machine.succeed(
        f"cd /tmp/sbom-dl && oras pull --plain-http {image_ref}@{sbom_digest}"
    )
    sbom = machine.succeed("cat /tmp/sbom-dl/*.cdx.json")
    print(f"--- pulled SBOM ---\n{sbom}")
    assert '"bomFormat": "CycloneDX"' in sbom, f"not a CycloneDX SBOM: {sbom!r}"
    assert '"name": "busybox"' in sbom, (
        f"expected a `busybox` component in the SBOM{envelope}\nSBOM:\n{sbom}"
    )

    # --- DB persistence + web UI -------------------------------------
    # The post-build step records the image archive size and persists
    # the SBOM in the database, independent of the registry push.
    img_size = machine.succeed(
        "sqlite3 /var/lib/argunix/db.sqlite "
        "'SELECT image_size_bytes FROM jobs WHERE id = 1;'"
    ).strip()
    print(f"jobs.image_size_bytes: {img_size!r}")
    assert img_size.isdigit() and int(img_size) > 0, (
        f"expected a non-zero jobs.image_size_bytes, got {img_size!r}{envelope}"
    )

    sbom_row = machine.succeed(
        "sqlite3 /var/lib/argunix/db.sqlite "
        "'SELECT format, component_count FROM sboms WHERE job_id = 1;'"
    ).strip()
    print(f"sboms row: {sbom_row!r}")
    assert sbom_row.startswith("cyclonedx|"), (
        f"expected a stored cyclonedx SBOM row{envelope}\nsboms: {sbom_row!r}"
    )

    # Bring the daemon back up to serve the web UI against the same db.
    machine.succeed("systemctl start argunix.service")
    machine.wait_for_open_port(${toString argunixPort})

    job_url = (
        "http://127.0.0.1:${toString argunixPort}"
        "/r/gh/myorg/myrepo/eval/1/job/packages.x86_64-linux.oci-image"
    )
    job_html = machine.succeed(f"curl -fsS {job_url}")
    print(f"--- job page ---\n{job_html}")
    assert "image size" in job_html, (
        f"job page is missing the image-size row{envelope}"
    )
    assert "registry-push" in job_html, (
        f"job page is missing the effects panel{envelope}"
    )
    assert "browse the SBOM" in job_html, (
        f"job page is missing the SBOM link{envelope}"
    )
    # An image job offers `docker run`, not `nix run` (meaningless for
    # an image — the output is an archive, not an app).
    assert "Run from registry" in job_html and "docker run" in job_html, (
        f"job page is missing the `docker run` snippet for the image{envelope}"
    )
    assert "Run from cache" not in job_html, (
        f"job page should not offer `nix run` for an image job{envelope}"
    )

    # The SBOM browser page renders the component table server-side.
    sbom_html = machine.succeed(f"curl -fsS {job_url}/sbom")
    print(f"--- sbom page ---\n{sbom_html}")
    assert "Software Bill of Materials" in sbom_html, "SBOM page did not render"
    assert "busybox" in sbom_html, "SBOM page is missing the busybox component"

    # Content negotiation: the same route yields the raw CycloneDX JSON.
    sbom_api = machine.succeed(
        f"curl -fsS -H 'Accept: application/json' {job_url}/sbom"
    )
    assert '"bomFormat": "CycloneDX"' in sbom_api, (
        f"the SBOM route did not return CycloneDX JSON{envelope}\n{sbom_api}"
    )

    # The eval page lists every registry reference the run published —
    # gathered from its jobs' `registry-push` / `registry-index`
    # effects. The immutable sha-<short> tag must appear there.
    eval_html = machine.succeed(
        "curl -fsS http://127.0.0.1:${toString argunixPort}/r/gh/myorg/myrepo/eval/1"
    )
    print(f"--- eval page ---\n{eval_html}")
    assert "Published images" in eval_html, (
        f"eval page is missing the published-images section{envelope}"
    )
    # The repository path renders once, its tags as separate badges.
    assert "${registryHost}/myorg/oci-image" in eval_html, (
        f"eval page does not list the published image path{envelope}"
    )
    assert ">main<" in eval_html and ">sha-000000000000<" in eval_html, (
        f"eval page does not render the image tags as badges{envelope}"
    )
  '';
}
