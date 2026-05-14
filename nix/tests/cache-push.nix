# End-to-end test of the post-build cache push, covering both shapes
# of `BinaryCache` config:
#
#   1. Asymmetric — `push_url` and `public_url` differ. Realistic for
#      S3-class backends where argunix writes via the API endpoint
#      and users read from a CDN or a separate S3-web gateway. We
#      use Garage on the same VM to play both roles.
#
#   2. Symmetric — no `public_url` set, so the same URL is the one
#      operators advertise. Mirrors cachix / attic / a plain
#      file:// cache.
#
# A real NixOS VM with real nix + real nix-eval-jobs + real Garage,
# driven through `argunix build` (the single-shot CLI). The build
# runs locally — no builder enrolment, no forge calls — and the push
# step writes the output closure to *both* configured caches. We
# assert per cache:
#
#   - the cache backend received the narinfo + NAR payload,
#   - `nix path-info --store <url>` resolves the output (proving the
#     narinfo is well-formed, references valid, signature verifies
#     against the configured public key),
#   - `nix store verify` succeeds (signature actually present, not
#     just well-formed).
#
# `services.argunix.enable = true` exercises the module's wiring of
# `binary_caches` + signing key path through to the YAML the daemon
# loads — the actual push, however, comes from a parallel `argunix
# build` invocation from the testScript, not from the service. That
# keeps the test free of forge tokens and webhook simulation while
# still going through the same `argunix-build::push_to_caches`
# code path the service uses.
{ pkgs, ... }:

let
  argunixPort = 8080;

  s3Bucket = "argunix-cache";
  s3Region = "garage";
  s3ApiEndpoint = "http://127.0.0.1:3900";

  # Asymmetric cache: argunix pushes via `s3://` to the Garage API,
  # users would read via a CDN / Garage's s3_web gateway. We pick a
  # synthetic `public_url` to demonstrate the field — it's
  # informational only at this stage (argunix doesn't fan out reads
  # to it).
  s3CachePushUrl = "s3://${s3Bucket}?endpoint=${s3ApiEndpoint}&region=${s3Region}";
  s3CachePublicUrl = "https://cache.example.com";

  # Symmetric cache: write and read on the same URL. file:// is the
  # simplest representative — cachix / attic / harmonia would look
  # the same in config (single URL, no public_url).
  localCacheDir = "/var/cache/argunix-local";
  localCacheUrl = "file://${localCacheDir}";

  githubToken = pkgs.writeText "argunix-test-github-token" "tok";

  # Generate a fresh ed25519 binary-cache key pair. `nix-store
  # --generate-binary-cache-key` won't run in a build sandbox
  # (it touches /nix/var/nix/profiles); `nix key …` is the pure
  # crypto path. We hand the secret to argunix and use the public
  # side to verify signed narinfo from the testScript. Both caches
  # share one key — common in practice when an operator owns both
  # backends.
  signingKeys =
    pkgs.runCommand "argunix-test-cache-keys"
      {
        nativeBuildInputs = [ pkgs.nix ];
      }
      ''
        mkdir -p $out
        export HOME=$TMPDIR
        nix --extra-experimental-features 'nix-command' \
          key generate-secret --key-name argunix-test-cache > "$out/secret"
        nix --extra-experimental-features 'nix-command' \
          key convert-secret-to-public < "$out/secret" > "$out/public"
      '';

  # Standalone flake with no inputs. The pre-generated `flake.lock`
  # avoids `nix-eval-jobs` needing to write one (the source lives in
  # /nix/store and is read-only). `derivation { ... }` keeps the
  # build trivial — no nixpkgs evaluation, no fetcher.
  fixtureFlake = pkgs.runCommand "argunix-cache-fixture-flake" { } ''
    mkdir -p $out
    cat > $out/flake.nix <<'EOF'
    {
      description = "argunix cache-push fixture";
      outputs = { self }: {
        packages.x86_64-linux.hello = derivation {
          name = "argunix-cache-push-output";
          system = "x86_64-linux";
          builder = "/bin/sh";
          args = [ "-c" "echo cache-pushed-by-argunix > $out" ];
        };
      };
    }
    EOF
    cat > $out/flake.lock <<'EOF'
    {
      "nodes": { "root": {} },
      "root": "root",
      "version": 7
    }
    EOF
  '';

  testConfig = (pkgs.formats.yaml { }).generate "argunix-test.yaml" {
    external_url = "https://argunix.example.com";
    forges.gh = {
      kind = "github";
      web_url = "https://github.com";
      token_path = "${githubToken}";
      repos."myorg/myrepo" = { };
    };
    binary_caches = [
      # Asymmetric S3 cache.
      {
        push_url = s3CachePushUrl;
        public_url = s3CachePublicUrl;
        signing_key_path = "${signingKeys}/secret";
      }
      # Symmetric local file cache — no `public_url`.
      {
        push_url = localCacheUrl;
        signing_key_path = "${signingKeys}/secret";
      }
    ];
  };
in
{
  name = "argunix-cache-push";

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
        configFile = testConfig;
      };

      # Self-hosted S3-compatible cache backend. The same VM hosts
      # both argunix and the cache — operationally artificial, but
      # the wire path argunix exercises is identical to a remote
      # Garage cluster (single S3 API endpoint, signed PUTs,
      # narinfo + NAR object writes).
      services.garage = {
        enable = true;
        package = pkgs.garage;
        settings = {
          rpc_bind_addr = "[::]:3901";
          rpc_public_addr = "127.0.0.1:3901";
          rpc_secret = "5c1915fa04d0b6739675c61bf5907eb0fe3d9c69850c83820f51b4d25d13868c";
          replication_factor = 1;
          consistency_mode = "consistent";
          s3_api = {
            s3_region = s3Region;
            api_bind_addr = "127.0.0.1:3900";
            root_domain = ".s3.garage";
          };
        };
      };

      # Symmetric file:// cache target — argunix's push step writes
      # `<hash>.narinfo` + `nar/<hash>.nar.xz` here. Owned by the
      # argunix user so the testScript invocation as that user can
      # write.
      systemd.tmpfiles.rules = [
        "d ${localCacheDir} 0755 argunix argunix - -"
      ];

      environment.systemPackages = [
        pkgs.argunix
        pkgs.nix-eval-jobs
      ];

      virtualisation = {
        memorySize = 2048;
        # Garage requires ~1 GiB free for metadata + data, plus
        # the VM's own store. Default 1 GiB is not enough.
        diskSize = 4 * 1024;
        writableStore = true;
      };
    };

  testScript = ''
    import re

    def match_or_fail(pattern: str, text: str, label: str) -> str:
        mm = re.search(pattern, text)
        assert mm, f"could not parse {label} from: {text!r}"
        return mm.group(1)

    machine.start()
    machine.wait_for_unit("argunix.service")
    machine.wait_for_open_port(${toString argunixPort})

    # Garage takes a moment to bind its rpc + s3-api ports after
    # the systemd unit becomes "active"; wait until the s3-api
    # socket is actually answering.
    machine.wait_for_unit("garage.service")
    machine.wait_for_open_port(3900)

    # Bootstrap the single-node cluster: assign capacity to the
    # only node, then apply the layout so writes are accepted.
    node_line = machine.succeed("garage node id").strip()
    node_id = node_line.split("@", 1)[0]
    print(f"garage node id: {node_id}")
    machine.succeed(f"garage layout assign -z test -c 1G {node_id}")
    # `layout show` reports "Current cluster layout version: N";
    # the next apply must pass N+1.
    layout = machine.succeed("garage layout show")
    next_version = int(
        match_or_fail(r"Current cluster layout version: (\d+)", layout, "layout version")
    ) + 1
    machine.succeed(f"garage layout apply --version {next_version}")

    # Create the bucket argunix will push into and an S3 API key
    # scoped to read+write on it. The `garage key create` output
    # carries the AWS-style credentials in human-readable lines;
    # parse them out.
    machine.succeed("garage bucket create ${s3Bucket}")
    key_info = machine.succeed("garage key create argunix-push")
    print(key_info)
    key_id = match_or_fail(r"Key ID:\s*(\S+)", key_info, "key id")
    secret_key = match_or_fail(r"Secret key:\s*(\S+)", key_info, "secret key")
    machine.succeed(
        "garage bucket allow --read --write ${s3Bucket} --key argunix-push"
    )

    # Confirm the module wired the cache config through to the
    # daemon's YAML — both shapes (asymmetric + symmetric) must
    # land in the same `binary_caches` list.
    machine.succeed(
        "grep -q 'push_url: s3://${s3Bucket}' ${testConfig}"
    )
    machine.succeed(
        "grep -q 'public_url: ${s3CachePublicUrl}' ${testConfig}"
    )
    machine.succeed(
        "grep -q 'push_url: ${localCacheUrl}' ${testConfig}"
    )

    # Drive a real eval+build+push through the single-shot CLI.
    # Running as the argunix user matches how the daemon would
    # invoke it: same uid, same nix-daemon trust level. AWS
    # credentials reach `nix copy` via the env — that's how the
    # nix S3 store reads them, identical to a real S3 deployment.
    # The file:// cache needs no credentials; argunix-build's push
    # iterates over every configured cache and writes to each.
    machine.succeed(
        "install -d -o argunix -g argunix /var/lib/argunix-test"
    )
    out = machine.succeed(
        "cd /var/lib/argunix-test && sudo -u argunix"
        f" AWS_ACCESS_KEY_ID={key_id}"
        f" AWS_SECRET_ACCESS_KEY={secret_key}"
        " argunix build"
        " --config ${testConfig}"
        " --src ${fixtureFlake}"
        " --slug myorg/myrepo"
        " --forge gh"
        " --systems x86_64-linux"
        " --gc-root-dir /var/lib/argunix-test/gcroots"
        " --log-dir /var/lib/argunix-test/logs"
    )
    print("--- argunix build summary ---")
    print(out)
    assert "success=1" in out, f"expected success=1 in summary, got: {out!r}"
    assert "failure=0" in out, f"expected failure=0 in summary, got: {out!r}"

    # Resolve the realised output path via the GC root argunix
    # planted for it. The tree is <gc-root-dir>/<repo>/<eval>/<job>.
    out_path = machine.succeed(
        "readlink -f /var/lib/argunix-test/gcroots/*/*/*"
    ).strip()
    print(f"output path: {out_path}")
    assert out_path.startswith("/nix/store/"), (
        f"expected /nix/store path, got: {out_path!r}"
    )

    # Sanity: the build actually ran (vs. a stubbed substitution),
    # and the file content matches what the derivation produced.
    contents = machine.succeed(f"cat {out_path}").strip()
    assert contents == "cache-pushed-by-argunix", (
        f"unexpected output contents: {contents!r}"
    )

    pubkey = machine.succeed("cat ${signingKeys}/public").strip()
    print(f"trusted-public-key: {pubkey}")

    # ---- Asymmetric cache assertions: S3 (Garage) ----
    #
    # The bucket must hold at least one narinfo object and a NAR
    # payload. `garage bucket info` reports object counts; a
    # non-zero count proves the push reached durable storage on
    # the remote.
    bucket_info = machine.succeed("garage bucket info ${s3Bucket}")
    print("--- garage bucket info ---")
    print(bucket_info)
    object_count = int(
        match_or_fail(r"Objects:\s*(\d+)", bucket_info, "object count")
    )
    assert object_count > 0, (
        f"expected at least one object in the s3 bucket, got {object_count}"
    )

    # `nix path-info --store s3://…` parses the narinfo, verifies
    # the references, and checks the signature against
    # trusted-public-keys — the same path a substituter takes.
    machine.succeed(
        f"AWS_ACCESS_KEY_ID={key_id}"
        f" AWS_SECRET_ACCESS_KEY={secret_key}"
        " nix --extra-experimental-features nix-command"
        f" path-info --extra-trusted-public-keys '{pubkey}'"
        f" --store '${s3CachePushUrl}' {out_path}"
    )
    machine.succeed(
        f"AWS_ACCESS_KEY_ID={key_id}"
        f" AWS_SECRET_ACCESS_KEY={secret_key}"
        " nix --extra-experimental-features nix-command"
        " store verify"
        f" --extra-trusted-public-keys '{pubkey}'"
        f" --store '${s3CachePushUrl}' {out_path}"
    )

    # ---- Symmetric cache assertions: file:// ----
    #
    # The cache dir must hold narinfo + NAR. `nix copy --to
    # file:///…` writes both atomically per push; a populated
    # narinfo without its NAR would be a regression in our
    # subprocess wiring.
    print("--- ${localCacheDir} ---")
    print(machine.succeed("ls -la ${localCacheDir}"))
    print(machine.succeed("ls -la ${localCacheDir}/nar"))

    narinfo_count = int(machine.succeed(
        "find ${localCacheDir} -maxdepth 1 -name '*.narinfo' | wc -l"
    ).strip())
    assert narinfo_count > 0, (
        f"expected at least one narinfo in the file:// cache, got {narinfo_count}"
    )
    nar_count = int(machine.succeed(
        "find ${localCacheDir}/nar -name '*.nar*' | wc -l"
    ).strip())
    assert nar_count > 0, (
        f"expected at least one NAR payload in the file:// cache, got {nar_count}"
    )

    machine.succeed(
        "nix --extra-experimental-features nix-command"
        f" path-info --extra-trusted-public-keys '{pubkey}'"
        f" --store '${localCacheUrl}' {out_path}"
    )
    machine.succeed(
        "nix --extra-experimental-features nix-command"
        " store verify"
        f" --extra-trusted-public-keys '{pubkey}'"
        f" --store '${localCacheUrl}' {out_path}"
    )
  '';
}
