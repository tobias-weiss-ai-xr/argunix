# End-to-end test of the synthetic-flake endpoint.
#
# The synthetic-flake feature lets `nix run <argunix-url>#<attr>`
# resolve to an already-cached store path without re-evaluating the
# upstream repo. Two-machine test:
#
#   - `argunix`: runs the daemon, plus `nix-serve` exposing /nix/store
#     over HTTP as a signed binary cache. The fixture we want to "run
#     from cache" is built into this node's /nix/store at test time.
#   - `client`:  a separate machine whose /nix/store has *no* copy of
#     the fixture. The test runs `nix run <argunix>/flake/…#hello`
#     here; the only way that can succeed is for nix to substitute
#     the closure from the argunix-side nix-serve.
#
# This is the literal "push an image, run it from some client" shape
# — the cache push happens implicitly the moment nix-serve answers a
# `narinfo` request, and the client genuinely has no source repo and
# no pre-baked closure.
{ pkgs, lib, ... }:

let
  argunixPort = 8080;
  cacheHttpPort = 5000;

  githubToken = pkgs.writeText "argunix-synth-test-token" "tok";

  # ed25519 binary-cache key pair, hardcoded.
  #
  # We do *not* generate the keypair at build time (e.g. via `nix key
  # generate-secret` inside `runCommand`) because that derivation is
  # non-deterministic: every fresh build produces a different secret
  # at the same input-addressed store path. In single-machine setups
  # the path is built once and consistently reused — but in CI / on
  # multi-host build farms, the eval-time `builtins.readFile` of the
  # public half and the VM-image read of the secret half can end up
  # backed by *different* materialisations of the same path (e.g. one
  # built locally, one fetched from a substituter that stored the
  # previous run's bytes). The result is that nix-serve signs with
  # key A and the client trusts key B → "lacks a signature by a
  # trusted key", deterministically, in CI only.
  #
  # The test isn't validating keygen — only that signed narinfo round-
  # trips through `fetchClosure { inputAddressed = true; }`. A bundled
  # keypair is therefore the right shape: deterministic, identical
  # across hosts, and the secret half is harmless to publish (it
  # signs nothing outside this throwaway test fixture).
  publicKey = "argunix-synth-test:NRr7uNjeT5Sf6V1ddpJHVcidw4DVFkkDOP+O4IfVSBM=";
  secretKeyFile = pkgs.writeText "argunix-synth-test-secret" ''
    argunix-synth-test:jdieG84Do93GiLF/O+Vb1gAjtt405xJcS37kjJTnEf01Gvu42N5PlJ/pXV12kkdVyJ3DgNUWSQM4/47gh9VIEw==
  '';

  cacheUrl = "http://argunix:${toString cacheHttpPort}";
in
{
  name = "argunix-synthetic-flake";

  defaults = {
    networking.dhcpcd.enable = false;
  };

  nodes.argunix =
    { pkgs, ... }:
    {
      imports = [ ../module.nix ];

      services.argunix = {
        enable = true;
        # Bind to 0.0.0.0 so the client node can reach it across the
        # test network.
        listen = "0.0.0.0:${toString argunixPort}";
        settings = {
          external_url = "http://argunix:${toString argunixPort}";
          forges.gh = {
            kind = "github";
            web_url = "https://github.com";
            token_path = "${githubToken}";
            repos."myorg/myrepo" = { };
          };
          binary_caches = [
            {
              # The push step never fires in this test (we inject DB
              # rows directly), but argunix's config validation
              # requires a `push_url`. Point it at a dummy file://
              # path: never written to, never read.
              push_url = "file:///var/cache/argunix-unused";
              public_url = cacheUrl;
              public_key = publicKey;
              signing_key_path = "${secretKeyFile}";
            }
          ];
        };
      };

      # Serve /nix/store as a binary cache. nix-serve signs narinfo on
      # demand with the same key argunix advertises as `public_key`,
      # so the client's nix will accept the signatures.
      services.nix-serve = {
        enable = true;
        port = cacheHttpPort;
        bindAddress = "0.0.0.0";
        secretKeyFile = "${secretKeyFile}";
      };

      networking.firewall.allowedTCPPorts = [
        argunixPort
        cacheHttpPort
      ];

      environment.systemPackages = [
        pkgs.sqlite
      ];

      # `writableStore` lets `nix-build` from the test driver land
      # things in /nix/store at runtime. Without it the build sandbox
      # fights with the cow-fs overlay.
      virtualisation = {
        memorySize = 1024;
        writableStore = true;
      };
    };

  nodes.client = {
    # Enable the experimental features `nix run` against our
    # synthetic flake needs. `fetch-closure` is the load-bearing
    # one — it has to be enabled *before* flake evaluation begins
    # (the flake's own `nixConfig.extra-experimental-features` can't
    # opt itself in retroactively).
    nix.settings.experimental-features = [
      "nix-command"
      "flakes"
      "fetch-closure"
    ];

    # `fetchClosure { inputAddressed = true; }` requires the cache
    # *and* its key to be in `trusted-substituters` /
    # `trusted-public-keys`. The flake's `nixConfig.extra-…` settings
    # plus `--accept-flake-config` are not sufficient: input-addressed
    # paths bypass content-addressing's tamper protection, so nix
    # treats them as operator-level trust — not propagatable by a
    # flake — and refuses to install the path unless the local
    # daemon already trusts both. The downstream UX: one-time
    # `trusted-substituters` + `trusted-public-keys` entries in
    # nix.conf, exactly the snippet argunix's /cache page shows.
    nix.settings.trusted-substituters = [ cacheUrl ];
    nix.settings.trusted-public-keys = [ publicKey ];

    # Wipe the active substituter list so the only one nix can pull
    # from is the cache the synthetic flake declares — without this,
    # cache.nixos.org leaking through would mask a regression where
    # the flake's substituter never actually got consulted.
    nix.settings.substituters = lib.mkForce [ ];

    virtualisation = {
      memorySize = 1024;
      writableStore = true;
    };
  };

  testScript = ''
    start_all()
    argunix.wait_for_unit("argunix.service")
    argunix.wait_for_open_port(${toString argunixPort})
    argunix.wait_for_unit("nix-serve.service")
    argunix.wait_for_open_port(${toString cacheHttpPort})
    client.wait_for_unit("multi-user.target")

    pubkey = "${publicKey}"
    print(f"trusted-public-key: {pubkey}")

    # ------------------------------------------------------------------
    # Build a fresh fixture on the argunix node only. The whole point
    # is that this path lives in argunix's /nix/store (where nix-serve
    # can hand it out) and is *absent* from the client's /nix/store
    # (so the eventual nix-run has no choice but to substitute).
    # ------------------------------------------------------------------
    coreutils = argunix.succeed(
        "dirname \"$(dirname \"$(readlink -f \"$(command -v mkdir)\")\")\""
    ).strip()
    print(f"coreutils path: {coreutils}")
    assert coreutils.startswith("/nix/store/"), (
        f"could not resolve coreutils store path: {coreutils!r}"
    )

    # Build a minimal derivation with `/bin/sh`. coreutils is pulled
    # into the build sandbox by declaring it as a derivation attr —
    # nix scans string-with-context attrs (here via `builtins.storePath`)
    # and registers them as inputs.
    argunix.succeed(f"""
      mkdir -p /tmp/synth-fixture
      cat > /tmp/synth-fixture/build.sh <<'BUILD'
    $coreutils/bin/mkdir -p $out/bin
    $coreutils/bin/cat > $out/bin/synth-hello <<'PROG'
    #!/bin/sh
    echo "hello from synthetic flake"
    PROG
    $coreutils/bin/chmod +x $out/bin/synth-hello
    BUILD
      cat > /tmp/synth-fixture/default.nix <<NIX
    derivation {{
      name = "synth-hello";
      system = builtins.currentSystem;
      builder = "/bin/sh";
      args = [ ./build.sh ];
      coreutils = builtins.storePath {coreutils};
    }}
    NIX
    """)

    fixture_path = argunix.succeed(
        "nix-build /tmp/synth-fixture --no-out-link"
    ).strip()
    print(f"fixture path: {fixture_path}")
    assert fixture_path.startswith("/nix/store/"), (
        f"unexpected fixture path: {fixture_path!r}"
    )
    # Sanity-check it actually works locally before involving the cache.
    local_out = argunix.succeed(f"{fixture_path}/bin/synth-hello").strip()
    assert local_out == "hello from synthetic flake", (
        f"fixture pre-check produced unexpected output: {local_out!r}"
    )

    # nix-serve must answer the binary-cache probe. The narinfo for
    # the fixture is computed on demand the first time anyone asks.
    argunix.succeed(
        "curl -fsS http://localhost:${toString cacheHttpPort}/nix-cache-info"
    )
    # And reach across the test network from the client.
    client.succeed("curl -fsS '${cacheUrl}/nix-cache-info'")
    # Pre-condition for the substitution assertion: client's local
    # store must NOT have the fixture yet.
    client.fail(f"test -e {fixture_path}")

    # ------------------------------------------------------------------
    # Inject DB rows so `/flake/gh/myorg/myrepo/eval/N` knows about
    # the fixture. The full eval pipeline (clone → nix-eval-jobs →
    # build) is covered by other tests; here we're testing the
    # HTTP-serving + flake-rendering layer in isolation.
    # ------------------------------------------------------------------
    argunix.systemctl("stop argunix.service")

    sql_path = "/tmp/synth-inject.sql"
    argunix.succeed(f"""cat > {sql_path} <<SQL
    INSERT OR IGNORE INTO repos (forge, slug) VALUES ('gh', 'myorg/myrepo');
    INSERT INTO evaluations (
      repo_id, trigger, git_ref, sha, status, started_at, finished_at
    ) VALUES (
      (SELECT id FROM repos WHERE forge='gh' AND slug='myorg/myrepo'),
      'push', 'refs/heads/main',
      '0000000000000000000000000000000000000000', 'done',
      '2025-01-01T00:00:00Z', '2025-01-01T00:01:00Z'
    );
    INSERT INTO jobs (
      eval_id, attr_path, drv_path, system, status, output_path,
      main_program, outputs_json
    ) VALUES (
      (SELECT MAX(id) FROM evaluations),
      'packages.x86_64-linux.synth-hello',
      NULL, 'x86_64-linux', 'success', '{fixture_path}',
      'synth-hello',
      '{{"out":"{fixture_path}"}}'
    );
    SQL""")
    argunix.succeed(f"sqlite3 /var/lib/argunix/db.sqlite < {sql_path}")

    eval_id = argunix.succeed(
        "sqlite3 /var/lib/argunix/db.sqlite "
        "'SELECT MAX(id) FROM evaluations;'"
    ).strip()
    print(f"injected eval id: {eval_id}")

    argunix.systemctl("start argunix.service")
    argunix.wait_for_open_port(${toString argunixPort})

    # ------------------------------------------------------------------
    # Fetch the synthetic flake tarball directly first; surface
    # malformed-tarball / wrong-attr regressions cleanly before
    # `nix run` swallows them in a generic error message.
    # ------------------------------------------------------------------
    flake_url = (
        f"http://argunix:${toString argunixPort}"
        f"/flake/gh/myorg/myrepo/eval/{eval_id}"
    )
    tar_path = "/tmp/synth-flake.tar"
    client.succeed(f"curl -fsS -o {tar_path} '{flake_url}'")
    flake_src = client.succeed(f"tar -xOf {tar_path} flake.nix")
    print("--- synthetic flake.nix ---")
    print(flake_src)
    assert "builtins.fetchClosure" in flake_src, (
        "synthetic flake must use builtins.fetchClosure; got:"
        f"\\n{flake_src}"
    )
    assert "inputAddressed = true" in flake_src, (
        "fetchClosure call must set inputAddressed=true; got:"
        f"\\n{flake_src}"
    )
    assert fixture_path in flake_src, (
        f"synthetic flake must reference {fixture_path}; got:"
        f"\\n{flake_src}"
    )
    assert "${cacheUrl}" in flake_src, (
        "synthetic flake must declare the public cache URL in nixConfig; got:"
        f"\\n{flake_src}"
    )
    assert pubkey in flake_src, (
        "synthetic flake must declare the public key in nixConfig; got:"
        f"\\n{flake_src}"
    )

    # ------------------------------------------------------------------
    # The payoff: from the client (which has no copy of the fixture
    # in its store), `nix run` against the synthetic flake URL MUST
    # succeed by substituting from the argunix-side nix-serve.
    # `--accept-flake-config` honours the flake's
    # nixConfig.extra-substituters + extra-trusted-public-keys.
    # ------------------------------------------------------------------
    run_out = client.succeed(
        f"nix --accept-flake-config run '{flake_url}#synth-hello'"
    ).strip()
    print(f"nix run output: {run_out!r}")
    assert run_out == "hello from synthetic flake", (
        f"unexpected nix-run output: {run_out!r}"
    )

    # After the run, the fixture's closure should be in the client's
    # /nix/store — proof that nix substituted it from our cache (as
    # opposed to somehow execve'ing without realising it, which would
    # be a silent bypass).
    client.succeed(f"test -e {fixture_path}")
  '';
}
