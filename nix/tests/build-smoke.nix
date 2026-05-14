# Sandboxed test of the single-shot build pipeline.
#
# Real `nix-store --realise` would need recursive nix and a writable store,
# neither of which is available in a `runCommand` sandbox. We ship
# stand-ins for `nix-eval-jobs`, `nix-store`, and `nix` (for `path-info`)
# that fake the surface area argunix-build talks to. The test then verifies
# the daemon's wiring: cache-skip → no build, success → log + gc root,
# failure → log + no gc root.
{
  runCommand,
  writeShellScriptBin,
  writeText,
  writers,
  sqlite,
  zstd,
  argunix,
}:
let
  fakeNixEvalJobs = writeShellScriptBin "nix-eval-jobs" ''
    set -eu
    flake=""
    while [ $# -gt 0 ]; do
      case "$1" in
        --flake) flake="$2"; shift 2 ;;
        *) shift ;;
      esac
    done
    case "$flake" in
      *"#packages.x86_64-linux"*)
        # `isCached: true` is how `nix-eval-jobs --check-cache-status`
        # signals "output is already reachable via local store or a
        # system-wide substituter". Argunix short-circuits on it and
        # marks the job `Cached` without dispatching a build. The
        # other two derivations omit the field (defaults to false)
        # and exercise the real build path.
        echo '{"attr":"cached","drvPath":"/nix/store/cccc-cached.drv","system":"x86_64-linux","outputs":{"out":"/nix/store/cccc-cached"},"isCached":true}'
        echo '{"attr":"succeed","drvPath":"/nix/store/dddd-succeed.drv","system":"x86_64-linux","outputs":{"out":"/nix/store/dddd-succeed"}}'
        echo '{"attr":"fail","drvPath":"/nix/store/eeee-fail.drv","system":"x86_64-linux","outputs":{"out":"/nix/store/eeee-fail"}}'
        ;;
      *)
        # Empty-stderr non-zero is argunix's "no such output" signal.
        exit 1
        ;;
    esac
  '';

  fakeNixStore = writeShellScriptBin "nix-store" ''
    set -eu
    # argunix-build invokes a combined form:
    #   nix-store --realise [--add-root <root>] [-L] <drv>
    # We parse all args into (root, drv) and then dispatch on drv.
    case "$1" in
      --realise) shift ;;
      *)
        echo "fake nix-store: unsupported subcommand $*" >&2
        exit 2
        ;;
    esac
    root=""
    drv=""
    while [ $# -gt 0 ]; do
      case "$1" in
        -L) shift ;;
        --add-root) root="$2"; shift 2 ;;
        --indirect) shift ;;
        *) drv="$1"; shift ;;
      esac
    done

    install_root() {
      # On success, register the GC root as `--add-root --indirect` would:
      # an indirect symlink at $root pointing at the output path.
      if [ -n "$root" ]; then
        mkdir -p "$(dirname "$root")"
        ln -sfn "$1" "$root"
      fi
    }

    case "$drv" in
      /nix/store/cccc-cached.drv)
        # Should never be invoked — the cache hit short-circuits the build.
        echo "fake nix-store: cccc-cached.drv should not be built (cache hit expected)" >&2
        exit 99
        ;;
      /nix/store/dddd-succeed.drv)
        echo "/nix/store/dddd-succeed"
        echo "[fake-build] building succeed" >&2
        echo "[fake-build] step 1/2" >&2
        echo "[fake-build] step 2/2" >&2
        install_root /nix/store/dddd-succeed
        exit 0
        ;;
      /nix/store/eeee-fail.drv)
        echo "[fake-build] building fail" >&2
        echo "error: simulated build failure" >&2
        exit 1
        ;;
      *)
        echo "fake nix-store: unknown drv $drv" >&2
        exit 3
        ;;
    esac
  '';

  fakeNix = writeShellScriptBin "nix" ''
    set -eu
    # The single-shot build pipeline shells out to `nix` only for the
    # post-build `nix copy --to <cache>` step. The test's
    # `binary_caches` list is empty (no push targets), so no `nix
    # copy` ever runs and this stub stays cold. We keep the stub
    # present anyway so an accidental wiring regression that adds a
    # `nix` invocation fails loudly instead of inheriting the
    # sandbox's real nix and going off-script.
    echo "fake nix: unexpected invocation: $*" >&2
    exit 2
  '';

  token = writeText "argunix-test-github-token" "tok-value";

  config = writers.writeYAML "argunix.yaml" {
    external_url = "https://argunix.example.com";
    forges.github-myorg = {
      kind = "github";
      web_url = "https://github.com";
      token_path = "${token}";
      repos = {
        "myorg/myrepo" = { };
      };
    };
    # No `binary_caches` here — system-wide `nix.settings.substituters`
    # owns reads, and we have nothing to push to in a sandbox test.
    # The `cached` derivation gets marked `Cached` via the
    # `isCached: true` flag in the fake nix-eval-jobs output.
  };

  fixtureFlake = runCommand "argunix-build-fixture-flake" { } ''
    mkdir -p $out
    cat > $out/flake.nix <<'EOF'
    { description = "fixture"; outputs = { self }: { }; }
    EOF
  '';
in
runCommand "argunix-build-smoke"
  {
    nativeBuildInputs = [
      argunix
      fakeNixEvalJobs
      fakeNixStore
      fakeNix
      sqlite
      zstd
    ];
    meta.description = "argunix build runs eval, cache-skip, build, log capture, gc roots";
  }
  ''
    set -euo pipefail

    workdir=$(mktemp -d)
    cd "$workdir"

    argunix build \
      --config ${config} \
      --src ${fixtureFlake} \
      --slug myorg/myrepo \
      --forge github-myorg \
      --systems x86_64-linux \
      --gc-root-dir "$workdir/gcroots" \
      --log-dir "$workdir/logs" \
      > summary.txt 2> stderr.log

    echo "--- summary ---"
    cat summary.txt
    echo "--- daemon stderr (tail) ---"
    tail -n 30 stderr.log

    grep -q 'cached=1' summary.txt
    grep -q 'success=1' summary.txt
    grep -q 'failure=1' summary.txt

    # DB shape: one repo, one eval, three jobs with the expected statuses.
    sqlite3 db.sqlite '.headers on' \
      'SELECT attr_path, status FROM jobs ORDER BY id;'

    statuses=$(sqlite3 db.sqlite \
      "SELECT status FROM jobs ORDER BY attr_path;" | tr '\n' ',')
    echo "--- statuses ---"
    echo "$statuses"
    test "$statuses" = "cached,failure,success,"

    sqlite3 db.sqlite '.headers on' \
      'SELECT attr_path, status, log_path, output_path FROM jobs ORDER BY attr_path;'

    # Cached job has output_path but no log_path. sqlite3 prints an
    # empty line for NULL columns, which `test -z` treats as empty.
    cached_log=$(sqlite3 db.sqlite \
      "SELECT log_path FROM jobs WHERE attr_path LIKE '%.cached';")
    echo "cached_log=[$cached_log]"
    test -z "$cached_log"

    # Successful + failed jobs have log files on disk, both zstd-compressed.
    succeed_log=$(sqlite3 db.sqlite \
      "SELECT log_path FROM jobs WHERE attr_path LIKE '%.succeed';")
    echo "succeed_log=[$succeed_log]"
    test -f "$succeed_log"
    zstd -d -c "$succeed_log" | grep -q '\[fake-build\] step 2/2'

    fail_log=$(sqlite3 db.sqlite \
      "SELECT log_path FROM jobs WHERE attr_path LIKE '%.fail';")
    echo "fail_log=[$fail_log]"
    test -f "$fail_log"
    zstd -d -c "$fail_log" | grep -q 'simulated build failure'

    # GC root only for the successful build.
    echo "--- gcroots tree ---"
    find "$workdir/gcroots" -ls
    test -L "$workdir/gcroots"/*/*/*
    succeed_root=$(find "$workdir/gcroots" -type l -lname '/nix/store/dddd-succeed' | head -n1)
    test -n "$succeed_root"
    fail_root=$(find "$workdir/gcroots" -type l -lname '/nix/store/eeee-fail' || true)
    test -z "$fail_root"

    touch $out
  ''
