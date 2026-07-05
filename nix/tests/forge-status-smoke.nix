# Sandbox test: verify the daemon posts the right forge checks at the
# right times.
#
# Architecture:
# - A small Python http.server stands in for github's API. It accepts
#   `POST /repos/.../statuses/<sha>`, logs every request body to a file,
#   and replies with `201 {"id":N}`.
# - `argunix serve` is configured with `web_url = http://<fake forge>`.
# - We send a webhook, wait for the daemon to finish, and grep the forge
#   log for the sequence of expected checks.
#
# Build outcomes are sandbox-determined. The coordinator's build path is
# pool-only — no local fallback — and a `runCommand` sandbox cannot enrol
# a real `argunix-builder`, so every dispatched job lands as `Interrupted`
# (no eligible builder). `Interrupted` maps to forge `CheckState::Error`,
# so the per-job posts here are state:"error". Per-job pass/fail
# verification is covered by the VM tests where real builders execute
# the jobs.
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

  fakeFlake = writeText "fake-flake.nix" ''
    {
      description = "fake-cloned";
      outputs = { self }: { };
    }
  '';

  fakeGit = writeShellScriptBin "git" ''
    set -eu
    # argunix passes credential config as leading `-c <k=v>` pairs
    # (SEC-1 credential helper); real git accepts them, so skip them.
    while [ "$1" = "-c" ]; do shift 2; done
    if [ "$1" = "-C" ]; then
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
  # builder enrolled in this sandbox the dispatcher's `--add-root` call
  # after a pull is unreachable too.

  fakeNix = writeShellScriptBin "nix" ''
    set -eu
    case "$1" in
      path-info) exit 1 ;;
      *) exit 2 ;;
    esac
  '';

  fakeForge = writeText "fake-forge.py" ''
    import http.server, json, os, sys, threading

    log_path = os.environ.get("FAKE_FORGE_LOG", "forge.log")
    log = open(log_path, "w")
    counter = [0]
    lock = threading.Lock()

    class H(http.server.BaseHTTPRequestHandler):
        def do_POST(self):
            length = int(self.headers.get("Content-Length", 0))
            body = self.rfile.read(length).decode("utf-8", errors="replace")
            with lock:
                counter[0] += 1
                cid = counter[0]
                log.write(f"POST {self.path}\n{body}\n---\n")
                log.flush()
            self.send_response(201)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            # POST /hooks is the ensure_webhook install path; the
            # provider deserializes the response as `HookView { id, config }`,
            # where `config` is required (not Option). Other POST paths
            # (commit statuses) don't constrain the response shape.
            if self.path.endswith("/hooks"):
                payload = {"id": cid, "config": {}}
            else:
                payload = {"id": cid, "state": "pending"}
            self.wfile.write(json.dumps(payload).encode())

        def do_GET(self):
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            # ensure_webhook GETs `/repos/{slug}/hooks` and decodes the
            # body as `Vec<HookView>`. Returning `{}` here would fail to
            # deserialize and ensure_webhook would error out before
            # persisting the secret. Empty array is "no existing hook,
            # please POST a new one" — what we want.
            if self.path.endswith("/hooks"):
                self.wfile.write(b"[]")
            else:
                self.wfile.write(b"{}")

        def log_message(self, *_a):
            pass

    srv = http.server.HTTPServer(("127.0.0.1", 0), H)
    print(f"fake-forge listening on 127.0.0.1:{srv.server_port}", flush=True)
    srv.serve_forever()
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
runCommand "argunix-forge-status-smoke"
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
    meta.description = "webhook + worker post the expected forge status checks";
  }
  ''
    set -euo pipefail

    workdir=$(mktemp -d)
    cd "$workdir"

    # 1. Start the fake forge and discover its address.
    FAKE_FORGE_LOG="$workdir/forge.log" \
      python3 ${fakeForge} > forge.stdout 2> forge.stderr &
    forge_pid=$!
    # SIGKILL — argunix's graceful TERM can wait for the worker
    # JoinHandle which only completes when the worker rx closes.
    # On any failure, dump the daemon stderr so the runCommand log is
    # actionable instead of just showing the last echo.
    on_exit() {
      ec=$?
      if [ "$ec" -ne 0 ]; then
        echo "--- daemon.stdout ---"; cat daemon.stdout 2>/dev/null || true
        echo "--- daemon.stderr ---"; cat daemon.stderr 2>/dev/null || true
        echo "--- forge.log ---"; cat forge.log 2>/dev/null || true
        echo "--- forge.stdout ---"; cat forge.stdout 2>/dev/null || true
        echo "--- forge.stderr ---"; cat forge.stderr 2>/dev/null || true
        echo "--- db ---"
        sqlite3 db.sqlite '.headers on' 'SELECT * FROM repos;' 2>/dev/null || true
        sqlite3 db.sqlite '.headers on' 'SELECT * FROM evaluations;' 2>/dev/null || true
        sqlite3 db.sqlite '.headers on' 'SELECT * FROM jobs;' 2>/dev/null || true
      fi
      kill -KILL $forge_pid 2>/dev/null || true
      kill -KILL ''${daemon_pid:-0} 2>/dev/null || true
      wait 2>/dev/null || true
    }
    trap on_exit EXIT

    forge_addr=""
    for _ in $(seq 1 100); do
      if grep -q '^fake-forge listening on ' forge.stdout 2>/dev/null; then
        forge_addr=$(awk '/^fake-forge listening on / { print $4; exit }' forge.stdout)
        break
      fi
      sleep 0.05
    done
    test -n "$forge_addr"
    echo "fake forge listening on $forge_addr"

    # 2. Materialise argunix.yaml with the right web_url.
    sed "s|WEB_URL_PLACEHOLDER|http://$forge_addr|" ${configTemplate} > argunix.yaml

    # 3. Start argunix serve.
    argunix serve \
      --config "$workdir/argunix.yaml" \
      --listen "127.0.0.1:0" \
      --work-dir "$workdir/work" \
      --log-dir "$workdir/logs" \
      --gc-root-dir "$workdir/gcroots" \
      --systems x86_64-linux \
      > daemon.stdout 2> daemon.stderr &
    daemon_pid=$!

    listen=""
    for _ in $(seq 1 100); do
      if grep -q '^listening on ' daemon.stdout 2>/dev/null; then
        listen=$(awk '/^listening on / { print $3; exit }' daemon.stdout)
        break
      fi
      sleep 0.05
    done
    test -n "$listen"
    echo "argunix listening on $listen"

    # 4. Send the webhook. The secret is argunix-generated at boot;
    # read it from sqlite to sign with the right key.
    webhookSecretHex=$(sqlite3 db.sqlite \
      "SELECT hex(webhook_secret) FROM repos WHERE forge='github-myorg' AND slug='myorg/myrepo';")
    test -n "$webhookSecretHex"
    body='${pushBody}'
    sig=$(printf %s "$body" \
      | openssl dgst -sha256 -mac HMAC -macopt "hexkey:$webhookSecretHex" \
      | awk '{print "sha256="$2}')

    code=$(curl -s -o /dev/null -w '%{http_code}' \
      -X POST "http://$listen/webhook/github" \
      -H 'Content-Type: application/json' \
      -H 'X-GitHub-Event: push' \
      -H "X-Hub-Signature-256: $sig" \
      -d "$body")
    test "$code" = "202"

    # 5. Wait for the worker to finish.
    for _ in $(seq 1 200); do
      status=$(sqlite3 db.sqlite 'SELECT status FROM evaluations WHERE id=1;' 2>/dev/null || echo "")
      [ "$status" = "done" ] && break
      sleep 0.05
    done
    test "$status" = "done"

    # 6. Wait for forge posts to drain (worker spawns post_check tasks).
    for _ in $(seq 1 100); do
      pending=$(grep -c '^POST ' forge.log 2>/dev/null || echo 0)
      [ "$pending" -ge 4 ] && break
      sleep 0.05
    done

    echo "--- forge log ---"
    cat forge.log
    echo "--- end forge log ---"

    # 7. Assertions on the recorded posts.
    sha=0123456789abcdef0123456789abcdef01234567
    # GHES-style API path: web_url has no `api.` subdomain so the
    # provider derives `<web>/api/v3` (see ForgeConfig::api_url).
    grep -F "POST /api/v3/repos/myorg/myrepo/statuses/$sha" forge.log

    # Initial pending check.
    grep -F '"state":"pending"' forge.log | grep -F '"context":"argunix: evaluation"'

    # Per-job: every job is Interrupted (no builder enrolled in the
    # sandbox), which the forge mapping reports as state:"error".
    grep -F '"context":"argunix: packages.x86_64-linux.hello"' forge.log | grep -F '"state":"error"'
    grep -F '"context":"argunix: packages.x86_64-linux.goodbye"' forge.log | grep -F '"state":"error"'

    # Final overall eval check. Every job Interrupted (no builder in the
    # sandbox) must NOT read as green: the overall state is "error" and
    # the description reports the interrupted count. See bugs.md COR-1.
    final=$(grep -F '"context":"argunix: evaluation"' forge.log | tail -n 1)
    echo "final overall check: $final"
    echo "$final" | grep -F '"state":"error"'
    echo "$final" | grep -F '0 ok, 0 cached, 0 failed, 2 interrupted'

    # Shutdown — see trap above for why SIGKILL.
    kill -KILL $daemon_pid 2>/dev/null || true
    kill -KILL $forge_pid 2>/dev/null || true
    wait 2>/dev/null || true
    trap - EXIT

    touch $out
  ''
