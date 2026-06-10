# End-to-end test of the multi-arch OCI fan-in (`design/multi-arch.md`).
#
# A flake exposes the *same* image once per architecture as `docker`
# format — `packages.x86_64-linux.hello` and
# `packages.aarch64-linux.hello`. argunix builds each as its own job
# (the aarch64 one realised under `binfmt` emulation in the VM), then
# the post-build fan-in stitches the two per-arch docker archives into
# one multi-arch OCI image index on the external `registry:2`.
#
# The load-bearing assertions: a `registry-index` effect_run lands
# `success`, the per-job `registry-push` and `sbom-attach` are
# *suppressed* for the grouped jobs (no race on the tag), the manifest
# the registry serves under `:main` is an OCI image index carrying both
# an `amd64` and an `arm64` entry, and each per-arch manifest in that
# index has its own CycloneDX SBOM attached as a referrer of its
# digest — so a consumer can scan the index, see the two platforms,
# and discover a per-platform SBOM for each (`design/multi-arch.md`).
#
# Each arch's docker image is cross-built on the (x86_64) host at
# flake-eval time — `dockerTools` cross-compiles cleanly. The in-VM
# fixture job is, as in `registry-push.nix`, a trivial sandboxed `cp`
# of that archive out of `${self}`; the aarch64 job's `cp` runs under
# qemu-user via `boot.binfmt`.
{ pkgs, ... }:

let
  argunixPort = 8080;
  registryPort = 5000;
  registryHost = "127.0.0.1:${toString registryPort}";

  armPkgs = pkgs.pkgsCross.aarch64-multiplatform;

  # One image per architecture — cross-built on the host. `dockerTools`
  # stamps each image config's `architecture`, which is what the index
  # assembly reads back to place the per-arch entries.
  mkImage =
    p:
    p.dockerTools.buildLayeredImage {
      name = "hello";
      tag = "fixture";
      contents = [ p.busybox ];
      config.Cmd = [
        "/bin/sh"
        "-c"
        "echo hello-multiarch"
      ];
    };
  imageAmd = mkImage pkgs;
  imageArm = mkImage armPkgs;

  # The fixture flake: the same logical image `hello`, exposed once per
  # system, both tagged `meta.image-format = "docker"`. Each derivation
  # references only `${self}` — one declared input, a clean sandboxed
  # build.
  flakeNix = pkgs.writeText "flake.nix" ''
    {
      outputs = { self }: {
        packages.x86_64-linux.hello = (derivation {
          name = "argunix-ma-fixture-amd64.tar.gz";
          system = "x86_64-linux";
          builder = "''${self}/busybox-amd64";
          args = [
            "sh"
            "-c"
            "''${self}/busybox-amd64 cp ''${self}/hello-amd64.tar.gz $out"
          ];
        }) // { meta.image-format = "docker"; };
        packages.aarch64-linux.hello = (derivation {
          name = "argunix-ma-fixture-arm64.tar.gz";
          system = "aarch64-linux";
          builder = "''${self}/busybox-arm64";
          args = [
            "sh"
            "-c"
            "''${self}/busybox-arm64 cp ''${self}/hello-arm64.tar.gz $out"
          ];
        }) // { meta.image-format = "docker"; };
      };
    }
  '';

  fixtureFlake = pkgs.runCommand "argunix-ma-fixture-flake" { } ''
    mkdir -p $out
    cp ${flakeNix} $out/flake.nix
    cp ${imageAmd} $out/hello-amd64.tar.gz
    cp ${imageArm} $out/hello-arm64.tar.gz
    cp ${pkgs.pkgsStatic.busybox}/bin/busybox $out/busybox-amd64
    cp ${armPkgs.pkgsStatic.busybox}/bin/busybox $out/busybox-arm64
    chmod +x $out/busybox-amd64 $out/busybox-arm64
    cat > $out/flake.lock <<'EOF'
    { "nodes": { "root": {} }, "root": "root", "version": 7 }
    EOF
  '';

  githubToken = pkgs.writeText "argunix-test-github-token" "tok";
in
{
  name = "argunix-multi-arch";
  globalTimeout = 1200; # 20 min: cross-builds + binfmt build + assembly

  defaults = {
    networking.dhcpcd.enable = false;
  };

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [ ../module.nix ];

      # Lets the VM realise `aarch64-linux` derivations under qemu-user;
      # NixOS also adds the emulated system to `nix.settings.extra-platforms`.
      boot.binfmt.emulatedSystems = [ "aarch64-linux" ];

      services.argunix = {
        enable = true;
        listen = "127.0.0.1:${toString argunixPort}";
        settings = {
          external_url = "https://argunix.example.com";
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
              push_to_registries = [ "local" ];
            };
          };
        };
      };

      services.dockerRegistry = {
        enable = true;
        listenAddress = "127.0.0.1";
        port = registryPort;
        enableDelete = true;
      };

      virtualisation.containers.enable = true;
      # Newer skopeo/podman only read a v2 `/etc/containers/registries.conf`;
      # the `virtualisation.containers.registries.insecure` option emits v1,
      # now rejected ("must be in v2 format but is in v1"). Override just
      # that one file with a v2 equivalent — policy.json/storage.conf from
      # the module stay intact, so the podman pull+run side still works.
      environment.etc."containers/registries.conf".source = pkgs.lib.mkForce (
        pkgs.writeText "registries.conf" ''
          [[registry]]
          location = "${registryHost}"
          insecure = true
        ''
      );

      environment.systemPackages = [
        pkgs.argunix
        pkgs.nix-eval-jobs
        pkgs.skopeo
        # The fan-in shells out to `oras manifest index create`.
        pkgs.oras
        pkgs.podman
        pkgs.zstd
        pkgs.sqlite
        pkgs.curl
      ];

      virtualisation = {
        memorySize = 6144;
        diskSize = 12 * 1024;
        writableStore = true;
        writableStoreUseTmpfs = false;
      };
      boot.tmp.useTmpfs = false;

      virtualisation.additionalPaths = [ fixtureFlake ];
    };

  testScript = ''
    import json
    import re

    machine.start()
    machine.wait_for_unit("argunix.service")
    machine.wait_for_open_port(${toString argunixPort})
    machine.wait_for_unit("docker-registry.service")
    machine.wait_for_open_port(${toString registryPort})

    exec_start = machine.succeed("systemctl show argunix -p ExecStart --value")
    m = re.search(r"--config\s+(\S+)", exec_start)
    assert m, f"could not parse daemon config path from: {exec_start!r}"
    config_path = m.group(1)

    # Build with the daemon stopped — a single sqlite writer.
    machine.succeed("systemctl stop argunix.service")

    out = machine.succeed(
        "cd /var/lib/argunix && sudo -u argunix"
        " argunix build"
        f" --config {config_path}"
        " --src ${fixtureFlake}"
        " --slug myorg/myrepo"
        " --forge gh"
        " --systems x86_64-linux,aarch64-linux"
        " --git-ref refs/heads/main"
        " --gc-root-dir /var/lib/argunix/gcroots"
        " --log-dir /var/lib/argunix/logs"
        " 2>&1"
    )
    print("--- argunix build summary + tracing ---")
    print(out)

    jobs_dump = machine.succeed(
        "sqlite3 /var/lib/argunix/db.sqlite '.headers on' "
        "'SELECT id, attr_path, system, status FROM jobs ORDER BY id;'"
    )
    effect_dump = machine.succeed(
        "sqlite3 /var/lib/argunix/db.sqlite '.headers on' "
        "'SELECT id, job_id, kind, target, status, detail FROM effect_runs ORDER BY id;'"
    )
    print(f"--- jobs ---\n{jobs_dump}")
    print(f"--- effect_runs ---\n{effect_dump}")
    envelope = f"\n\nbuild:\n{out}\njobs:\n{jobs_dump}\neffect_runs:\n{effect_dump}\n"

    # Both per-arch jobs must have built — the aarch64 one under binfmt.
    assert "failure=0" in out and "errors=0" in out, (
        f"expected a clean build{envelope}"
    )
    assert "success=2" in out, (
        f"expected both per-arch image jobs to build{envelope}"
    )

    rows = machine.succeed(
        "sqlite3 /var/lib/argunix/db.sqlite "
        "'SELECT kind, target, status FROM effect_runs;'"
    ).strip().splitlines()
    print(f"effect_runs rows: {rows!r}")

    # The fan-in must have recorded a successful `registry-index` row.
    assert "registry-index|local|success" in rows, (
        f"expected a successful registry-index effect_run{envelope}"
    )
    # ...recorded against *every* per-arch job, so each job's own page
    # shows the assembly it was part of — not just the lowest-id one.
    index_jobs = machine.succeed(
        "sqlite3 /var/lib/argunix/db.sqlite "
        "\"SELECT job_id FROM effect_runs WHERE kind = 'registry-index' ORDER BY job_id;\""
    ).strip().splitlines()
    print(f"registry-index job_ids: {index_jobs!r}")
    assert index_jobs == ["1", "2"], (
        f"registry-index should be recorded for both per-arch jobs{envelope}"
    )
    # The grouped jobs' per-job `registry-push` *and* `sbom-attach`
    # must have been suppressed — the fan-in owns the tags and attaches
    # a per-arch SBOM to each per-arch manifest digest, so neither
    # per-job effect may run.
    assert not any(r.startswith("registry-push|") for r in rows), (
        f"a grouped job's per-job registry-push was not suppressed{envelope}"
    )
    assert not any(r.startswith("sbom-attach|") for r in rows), (
        f"a grouped job's per-job sbom-attach was not suppressed{envelope}"
    )
    assert not any("running" in r for r in rows), (
        f"an effect_run was left in `running`{envelope}"
    )

    # The registry must serve, under `:main`, an OCI image *index* —
    # not a single-platform manifest — carrying both architectures.
    raw = machine.succeed(
        "skopeo inspect --raw --tls-verify=false"
        " docker://${registryHost}/myorg/hello:main"
    )
    print(f"--- raw :main manifest ---\n{raw}")
    assert "image.index" in raw, (
        f"`:main` is not an OCI image index{envelope}\nraw:\n{raw}"
    )
    assert '"amd64"' in raw and '"arm64"' in raw, (
        f"the index is missing an architecture{envelope}\nraw:\n{raw}"
    )

    # The immutable sha-<short> tag must resolve to the same index.
    tags_json = machine.succeed(
        "curl -fsS http://${registryHost}/v2/myorg/hello/tags/list"
    )
    print(f"--- tags ---\n{tags_json}")
    assert '"main"' in tags_json and "sha-" in tags_json, (
        f"expected `main` + an immutable sha- tag{envelope}\ntags: {tags_json!r}"
    )
    sha_tag = re.search(r'"(sha-[0-9a-f]+)"', tags_json)
    assert sha_tag, f"no sha- tag in {tags_json!r}"
    raw_sha = machine.succeed(
        "skopeo inspect --raw --tls-verify=false"
        f" docker://${registryHost}/myorg/hello:{sha_tag.group(1)}"
    )
    assert "image.index" in raw_sha and '"arm64"' in raw_sha, (
        f"the sha- tag is not a multi-arch index{envelope}"
    )

    # Each per-arch manifest in the index carries its *own* CycloneDX
    # SBOM, attached by the fan-in as a referrer of that arch's
    # manifest digest. A consumer scans the index, sees the two
    # platforms, and discovers a per-platform SBOM for each — entirely
    # in standard OCI terms (`design/multi-arch.md`).
    index = json.loads(raw)
    per_arch = {
        e["platform"]["architecture"]: e["digest"] for e in index["manifests"]
    }
    print(f"--- per-arch manifest digests ---\n{per_arch}")
    assert set(per_arch) == {"amd64", "arm64"}, (
        f"index does not carry exactly amd64+arm64{envelope}\n{per_arch}"
    )
    for arch, digest in per_arch.items():
        discover = machine.succeed(
            f"oras discover --plain-http ${registryHost}/myorg/hello@{digest}"
        )
        print(f"--- oras discover {arch} ({digest}) ---\n{discover}")
        assert "application/vnd.cyclonedx+json" in discover, (
            f"no CycloneDX SBOM referrer on the {arch} manifest{envelope}\n"
            f"oras discover:\n{discover}"
        )

    # The fan-in's `registry-index` effect_run reports the SBOM attach.
    index_detail = machine.succeed(
        "sqlite3 /var/lib/argunix/db.sqlite "
        "\"SELECT detail FROM effect_runs WHERE kind = 'registry-index';\""
    )
    print(f"--- registry-index detail ---\n{index_detail}")
    assert "per-arch SBOM" in index_detail, (
        f"the fan-in did not report attaching per-arch SBOMs{envelope}\n{index_detail}"
    )

    # Each per-arch job also has its SBOM persisted in the DB — the
    # per-job `record_image_artifacts`, which now runs for `docker`
    # images, not just `oci`.
    sbom_rows = machine.succeed(
        "sqlite3 /var/lib/argunix/db.sqlite "
        "'SELECT job_id, format FROM sboms ORDER BY job_id;'"
    ).strip().splitlines()
    print(f"--- sboms rows ---\n{sbom_rows!r}")
    assert len(sbom_rows) == 2 and all("cyclonedx" in r for r in sbom_rows), (
        f"expected a stored CycloneDX SBOM for each per-arch job{envelope}\n{sbom_rows!r}"
    )
  '';
}
