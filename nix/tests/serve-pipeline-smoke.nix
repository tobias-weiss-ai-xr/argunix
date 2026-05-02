# End-to-end M5c1 test: webhook → worker → eval+build → DB.
#
# `medusa serve` runs in the background. Stand-in `git`, `nix-eval-jobs`,
# `nix-store`, and `nix` (path-info) replace the real binaries so we can
# exercise the worker pipeline inside a sealed `runCommand` sandbox.
{
  runCommand,
  writeShellScriptBin,
  writeText,
  writers,
  curl,
  openssl,
  sqlite,
  medusa,
}:
let
  webhookSecret = "shh-webhook-secret";
  webhookSecretFile = writeText "medusa-test-webhook-secret" webhookSecret;
  token = writeText "medusa-test-github-token" "tok-value";

  fakeFlake = writeText "fake-flake.nix" ''
    {
      description = "fake-cloned";
      outputs = { self }: { };
    }
  '';

  fakeGit = writeShellScriptBin "git" ''
    set -eu
    # Two call shapes:
    #   git clone --filter=blob:none <url> <dst>
    #   git -C <dst> <subcmd> ...
    if [ "$1" = "-C" ]; then
      # fetch / checkout / etc. — just succeed.
      exit 0
    fi
    case "$1" in
      clone)
        dst=""
        for a in "$@"; do dst="$a"; done
        mkdir -p "$dst"
        cp ${fakeFlake} "$dst/flake.nix"
        exit 0
        ;;
      *)
        exit 0
        ;;
    esac
  '';

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
        echo '{"attr":"hello","drvPath":"/nix/store/aaaa-hello.drv","system":"x86_64-linux","outputs":{"out":"/nix/store/aaaa-hello"}}'
        echo '{"attr":"goodbye","drvPath":"/nix/store/bbbb-goodbye.drv","system":"x86_64-linux","outputs":{"out":"/nix/store/bbbb-goodbye"}}'
        ;;
      *)
        exit 1
        ;;
    esac
  '';

  fakeNixStore = writeShellScriptBin "nix-store" ''
    set -eu
    case "$1" in
      --realise)
        shift
        ;;
      --add-root)
        root="$2"; out="$5"
        mkdir -p "$(dirname "$root")"
        ln -sfn "$out" "$root"
        exit 0
        ;;
      *)
        exit 2
        ;;
    esac
    drv=""
    while [ $# -gt 0 ]; do
      case "$1" in
        -L) shift ;;
        *) drv="$1"; shift ;;
      esac
    done
    case "$drv" in
      /nix/store/aaaa-hello.drv)
        echo "/nix/store/aaaa-hello"
        echo "[fake-build] hello: line 1" >&2
        echo "[fake-build] hello: line 2" >&2
        exit 0
        ;;
      /nix/store/bbbb-goodbye.drv)
        echo "[fake-build] goodbye: failing" >&2
        exit 1
        ;;
      *)
        exit 3
        ;;
    esac
  '';

  fakeNix = writeShellScriptBin "nix" ''
    set -eu
    case "$1" in
      path-info)
        # Treat everything as a miss for this test (we want builds to run).
        exit 1
        ;;
      *)
        exit 2
        ;;
    esac
  '';

  config = writers.writeYAML "medusa.yaml" {
    external_url = "https://medusa.example.com";
    listen = "127.0.0.1:0";
    forges.github-myorg = {
      kind = "github";
      api_url = "https://api.github.com";
      webhook_secret_path = "${webhookSecretFile}";
      token_path = "${token}";
    };
    repos = [
      {
        slug = "myorg/myrepo";
        forge = "github-myorg";
      }
    ];
  };

  pushBody = builtins.toJSON {
    ref = "refs/heads/main";
    after = "0123456789abcdef0123456789abcdef01234567";
    repository.full_name = "myorg/myrepo";
    pusher.name = "alice";
  };
in
runCommand "medusa-serve-pipeline-smoke"
  {
    nativeBuildInputs = [
      medusa
      fakeGit
      fakeNixEvalJobs
      fakeNixStore
      fakeNix
      curl
      openssl
      sqlite
    ];
    meta.description = "M5c1: webhook triggers worker, which clones, evals, builds, and updates DB";
  }
  ''
        set -euo pipefail

        workdir=$(mktemp -d)
        cd "$workdir"

        medusa serve \
          --config ${config} \
          --listen "127.0.0.1:0" \
          --work-dir "$workdir/work" \
          --log-dir "$workdir/logs" \
          --gc-root-dir "$workdir/gcroots" \
          --systems x86_64-linux \
          > daemon.stdout 2> daemon.stderr &
        daemon_pid=$!
        trap 'kill $daemon_pid 2>/dev/null || true; wait $daemon_pid 2>/dev/null || true' EXIT

        listen=""
        for _ in $(seq 1 100); do
          if grep -q '^listening on ' daemon.stdout 2>/dev/null; then
            listen=$(awk '/^listening on / { print $3; exit }' daemon.stdout)
            break
          fi
          sleep 0.05
        done
        test -n "$listen"
        echo "daemon listening on $listen"

        body='${pushBody}'
        sig=$(printf %s "$body" | openssl dgst -sha256 -hmac '${webhookSecret}' | awk '{print "sha256="$2}')

        code=$(curl -s -o resp.txt -w '%{http_code}' \
          -X POST "http://$listen/webhook/github" \
          -H 'Content-Type: application/json' \
          -H 'X-GitHub-Event: push' \
          -H "X-Hub-Signature-256: $sig" \
          -d "$body")
        test "$code" = "202"
        echo "webhook accepted"

        # Poll the DB until the worker drives the eval to a terminal state.
        status=""
        for _ in $(seq 1 200); do
          status=$(sqlite3 db.sqlite 'SELECT status FROM evaluations WHERE id=1;' 2>/dev/null || echo "")
          case "$status" in
            done|evaluation_failed|cancelled) break ;;
          esac
          sleep 0.05
        done
        echo "evaluation status: $status"
        test "$status" = "done"

        echo "--- jobs ---"
        sqlite3 db.sqlite '.headers on' \
          'SELECT attr_path, status FROM jobs ORDER BY id;'

        job_statuses=$(sqlite3 db.sqlite \
          "SELECT status FROM jobs ORDER BY attr_path;")
        echo "$job_statuses"
        test "$job_statuses" = "failure
    success"

        # The successful build's log must exist and contain the fake build output.
        succeed_log=$(sqlite3 db.sqlite \
          "SELECT log_path FROM jobs WHERE attr_path LIKE '%.hello';")
        test -f "$succeed_log"

        # GC root only for the success.
        succeed_root=$(find "$workdir/gcroots" -type l -lname '/nix/store/aaaa-hello' | head -n1)
        test -n "$succeed_root"

        kill -TERM $daemon_pid
        wait $daemon_pid || true
        trap - EXIT

        echo "--- daemon stderr (last 30 lines) ---"
        tail -n 30 daemon.stderr || true

        touch $out
  ''
