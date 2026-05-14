# End-to-end test of the post-build cache push.
#
# A real NixOS VM with real nix + real nix-eval-jobs, driven through
# `argunix build` (the single-shot CLI). The build runs locally — no
# builder enrolment, no forge calls — and the push step writes the
# output closure to a `file://` cache. We assert:
#
#   - the cache directory contains at least one `*.narinfo`,
#   - `nix path-info --store file://<cache>` resolves the output
#     (proving the narinfo is well-formed and the NAR payload is
#     there),
#   - the narinfo carries a signature made with the configured key,
#   - the build output content is what we asked for (sanity check
#     that argunix ran the real build, not a stub).
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
  cacheUrl = "file:///var/cache/argunix-cache";

  githubToken = pkgs.writeText "argunix-test-github-token" "tok";

  # Generate a fresh ed25519 binary-cache key pair. `nix-store
  # --generate-binary-cache-key` won't run in a build sandbox
  # (it touches /nix/var/nix/profiles); `nix key …` is the pure
  # crypto path. We hand the secret to argunix and use the public
  # side to verify signed narinfo from the testScript.
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
      {
        url = cacheUrl;
        signing_key_path = "${signingKeys}/secret";
        push = true;
        substitute = false;
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

      environment.systemPackages = [
        pkgs.argunix
        pkgs.nix-eval-jobs
      ];

      # `file://` cache target — argunix's push step writes
      # `<hash>.narinfo` + `nar/<hash>.nar.xz` here. Owned by the
      # argunix user so the daemon (and our argunix-user testScript
      # invocation) can write.
      systemd.tmpfiles.rules = [
        "d /var/cache/argunix-cache 0755 argunix argunix - -"
      ];

      virtualisation = {
        memorySize = 1536;
        writableStore = true;
      };
    };

  testScript = ''
    machine.start()
    machine.wait_for_unit("argunix.service")
    machine.wait_for_open_port(${toString argunixPort})

    # Confirm the module wired the cache config through: the
    # generated YAML must contain our cache URL.
    machine.succeed(
        "grep -q 'file:///var/cache/argunix-cache' ${testConfig}"
    )

    # Drive a real eval+build+push through the single-shot CLI.
    # Running as the argunix user matches how the daemon would
    # invoke it: same uid, same nix-daemon trust level, same
    # access to /var/cache/argunix-cache.
    machine.succeed(
        "install -d -o argunix -g argunix /var/lib/argunix-test"
    )
    out = machine.succeed(
        "cd /var/lib/argunix-test && sudo -u argunix"
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

    # The cache must have at least one narinfo and the corresponding
    # NAR payload. `nix copy --to file:///...` writes both atomically
    # per push, so a populated narinfo without its NAR would be a
    # regression in our subprocess wiring.
    print("--- /var/cache/argunix-cache ---")
    print(machine.succeed("ls -la /var/cache/argunix-cache"))
    print(machine.succeed("ls -la /var/cache/argunix-cache/nar"))

    narinfo_count = int(machine.succeed(
        "find /var/cache/argunix-cache -maxdepth 1 -name '*.narinfo' | wc -l"
    ).strip())
    assert narinfo_count > 0, (
        f"expected at least one narinfo in the cache, got {narinfo_count}"
    )
    nar_count = int(machine.succeed(
        "find /var/cache/argunix-cache/nar -name '*.nar*' | wc -l"
    ).strip())
    assert nar_count > 0, (
        f"expected at least one NAR payload in the cache, got {nar_count}"
    )

    # `nix path-info --store <cache>` is the closest thing to "is
    # the cache able to serve this path" — it parses the narinfo,
    # verifies the references, and (with require-sigs) checks the
    # signature against trusted-public-keys. We add the public key
    # so the signature check goes through the same path a
    # real-world substituter does.
    pubkey = machine.succeed("cat ${signingKeys}/public").strip()
    print(f"trusted-public-key: {pubkey}")
    machine.succeed(
        "nix --extra-experimental-features nix-command"
        f" path-info --extra-trusted-public-keys '{pubkey}'"
        f" --store ${cacheUrl} {out_path}"
    )

    # And the narinfo must actually be signed by our key — not just
    # well-formed. `nix store verify --store <cache>` exits non-zero
    # if signatures are missing or untrusted; this guards against a
    # regression where we drop the `secret-key=` query param and
    # cache the path unsigned.
    machine.succeed(
        "nix --extra-experimental-features nix-command"
        " store verify"
        f" --extra-trusted-public-keys '{pubkey}'"
        f" --store ${cacheUrl} {out_path}"
    )
  '';
}
