# Fast smoke of the eval pipeline: webhook → worker clones → evals →
# persists jobs → tries to dispatch → DB reflects the outcome.
#
# `argunix serve` runs in the background with stand-in `git`,
# `nix-eval-jobs`, and `nix` (`path-info`) inside a sealed `runCommand`
# sandbox. The daemon's build path is pool-only — there is no local
# fallback — and a sandbox cannot enrol a real `argunix-builder`, so
# every dispatched job lands as `Interrupted` (no eligible builder). The
# test validates the *eval-side* of the pipeline; the build-side is
# covered end-to-end by the VM tests (`builder-build-dispatch`,
# `live-log-stream`, `builders-parallel`, `registry-push-oci`).
{
  runCommand,
  writeShellScriptBin,
  writeText,
  writers,
  curl,
  openssl,
  python3,
  sqlite,
  argunix,
}:
let
  token = writeText "argunix-test-github-token" "tok-value";

  # Trivial fake forge so ensure_webhook persists the generated secret
  # to sqlite (we read it back for HMAC signing of the test payload).
  fakeForge = writeText "fake-forge.py" ''
    import http.server, json
    class H(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            self.send_response(200); self.send_header("Content-Type", "application/json"); self.end_headers()
            self.wfile.write(b"[]")
        def do_POST(self):
            length = int(self.headers.get("Content-Length", 0))
            self.rfile.read(length)
            self.send_response(201); self.send_header("Content-Type", "application/json"); self.end_headers()
            # GitHub provider deserialises POST response as `{id, config}`;
            # `config` is required, so we include it as an empty object.
            self.wfile.write(json.dumps({"id": 1, "config": {}}).encode())
        def log_message(self, *_a):
            pass
    srv = http.server.HTTPServer(("127.0.0.1", 0), H)
    print(f"fake-forge listening on 127.0.0.1:{srv.server_port}", flush=True)
    srv.serve_forever()
  '';

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

  # No fake `nix-store` is needed: the coordinator never invokes
  # `nix-store --realise` itself (build path is pool-only), and with no
  # builder enrolled in this sandbox the dispatcher's `--add-root`
  # call after a pull is unreachable too.

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

  configTemplate = writers.writeYAML "argunix.yaml.template" {
    external_url = "https://argunix.example.com";
    listen = "127.0.0.1:0";
    forges.github-myorg = {
      kind = "github";
      web_url = "WEB_URL_PLACEHOLDER";
      token_path = "${token}";
      repos = {
        "myorg/myrepo" = { };
      };
    };
  };

  pushBody = builtins.toJSON {
    ref = "refs/heads/main";
    after = "0123456789abcdef0123456789abcdef01234567";
    repository.full_name = "myorg/myrepo";
    pusher.name = "alice";
  };
in
runCommand "argunix-serve-pipeline-smoke"
  {
    nativeBuildInputs = [
      argunix
      fakeGit
      fakeNixEvalJobs
      fakeNix
      curl
      openssl
      python3
      sqlite
    ];
    meta.description = "webhook triggers worker, which clones, evals, builds, and updates DB";
  }
  ''
    set -euo pipefail

    workdir=$(mktemp -d)
    cd "$workdir"

    # Fake forge → ensure_webhook persists the generated secret in sqlite.
    python3 ${fakeForge} > forge.stdout 2> forge.stderr &
    forge_pid=$!
    trap 'kill -KILL $forge_pid 2>/dev/null || true; wait 2>/dev/null || true' EXIT
    forge_addr=""
    for _ in $(seq 1 100); do
      if grep -q '^fake-forge listening on ' forge.stdout 2>/dev/null; then
        forge_addr=$(awk '/^fake-forge listening on / { print $4; exit }' forge.stdout)
        break
      fi
      sleep 0.05
    done
    test -n "$forge_addr"
    sed "s|WEB_URL_PLACEHOLDER|http://$forge_addr|" ${configTemplate} > argunix.yaml

    argunix serve \
      --config "$workdir/argunix.yaml" \
      --listen "127.0.0.1:0" \
      --work-dir "$workdir/work" \
      --log-dir "$workdir/logs" \
      --gc-root-dir "$workdir/gcroots" \
      --systems x86_64-linux \
      > daemon.stdout 2> daemon.stderr &
    daemon_pid=$!
    # SIGKILL on cleanup — argunix's graceful shutdown can wait for an
    # idle worker that never closes its receiver, blocking the trap.
    trap 'kill -KILL $daemon_pid 2>/dev/null || true; kill -KILL $forge_pid 2>/dev/null || true; wait 2>/dev/null || true' EXIT

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

    # Argunix generates the webhook secret per (forge, slug) at boot;
    # read it from sqlite to sign the test payload.
    webhookSecretHex=$(sqlite3 db.sqlite \
      "SELECT hex(webhook_secret) FROM repos WHERE forge='github-myorg' AND slug='myorg/myrepo';")
    test -n "$webhookSecretHex"

    body='${pushBody}'
    sig=$(printf %s "$body" \
      | openssl dgst -sha256 -mac HMAC -macopt "hexkey:$webhookSecretHex" \
      | awk '{print "sha256="$2}')

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

    # Both jobs reached the dispatch loop and exited via the pool-only
    # `None`-branch — no eligible builder, mark `Interrupted`. We just
    # assert both jobs landed there; the build-side is covered by the
    # VM tests.
    job_statuses=$(sqlite3 db.sqlite \
      "SELECT status FROM jobs ORDER BY attr_path;" | tr '\n' ',')
    echo "$job_statuses"
    test "$job_statuses" = "interrupted,interrupted,"

    # Sanity: the persisted jobs carry the attr paths the fake
    # `nix-eval-jobs` emitted — proves the eval phase ran and stored
    # them before the dispatch loop interrupted them.
    attr_paths=$(sqlite3 db.sqlite \
      "SELECT attr_path FROM jobs ORDER BY attr_path;" | tr '\n' ',')
    test "$attr_paths" = "packages.x86_64-linux.goodbye,packages.x86_64-linux.hello,"

    kill -KILL $daemon_pid 2>/dev/null || true
    kill -KILL $forge_pid 2>/dev/null || true
    wait 2>/dev/null || true
    trap - EXIT

    echo "--- daemon stderr (last 30 lines) ---"
    tail -n 30 daemon.stderr || true

    touch $out
  ''
