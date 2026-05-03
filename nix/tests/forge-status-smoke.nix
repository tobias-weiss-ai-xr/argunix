# M5c2 sandbox test: verify the daemon posts the right forge checks at the
# right times.
#
# Architecture:
# - A small Python http.server stands in for github's API. It accepts
#   `POST /repos/.../statuses/<sha>`, logs every request body to a file,
#   and replies with `201 {"id":N}`.
# - `medusa serve` is configured with `api_url = http://<fake forge>`.
# - We send a webhook, wait for the daemon to finish, and grep the forge
#   log for the sequence of expected checks.
{
  runCommand,
  writeShellScriptBin,
  writeText,
  writers,
  curl,
  openssl,
  python3,
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

  fakeNixStore = writeShellScriptBin "nix-store" ''
    set -eu
    case "$1" in
      --realise) shift ;;
      --add-root)
        root="$2"; out="$5"
        mkdir -p "$(dirname "$root")"
        ln -sfn "$out" "$root"
        exit 0
        ;;
      *) exit 2 ;;
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
        echo "[fake-build] hello ok" >&2
        exit 0
        ;;
      /nix/store/bbbb-goodbye.drv)
        echo "[fake-build] goodbye failing" >&2
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
            self.wfile.write(json.dumps({"id": cid, "state": "pending"}).encode())

        def do_GET(self):
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(b"{}")

        def log_message(self, *_a):
            pass

    srv = http.server.HTTPServer(("127.0.0.1", 0), H)
    print(f"fake-forge listening on 127.0.0.1:{srv.server_port}", flush=True)
    srv.serve_forever()
  '';

  configTemplate = writers.writeYAML "medusa.yaml.template" {
    external_url = "https://medusa.example.com";
    listen = "127.0.0.1:0";
    forges.github-myorg = {
      kind = "github";
      api_url = "API_URL_PLACEHOLDER";
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
runCommand "medusa-forge-status-smoke"
  {
    nativeBuildInputs = [
      medusa
      fakeGit
      fakeNixEvalJobs
      fakeNixStore
      fakeNix
      curl
      openssl
      python3
      sqlite
    ];
    meta.description = "M5c2: webhook + worker post the expected forge status checks";
  }
  ''
    set -euo pipefail

    workdir=$(mktemp -d)
    cd "$workdir"

    # 1. Start the fake forge and discover its address.
    FAKE_FORGE_LOG="$workdir/forge.log" \
      python3 ${fakeForge} > forge.stdout 2> forge.stderr &
    forge_pid=$!
    trap 'kill $forge_pid 2>/dev/null || true; kill $daemon_pid 2>/dev/null || true; wait 2>/dev/null || true' EXIT

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

    # 2. Materialise medusa.yaml with the right api_url.
    sed "s|API_URL_PLACEHOLDER|http://$forge_addr|" ${configTemplate} > medusa.yaml

    # 3. Start medusa serve.
    medusa serve \
      --config "$workdir/medusa.yaml" \
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
    echo "medusa listening on $listen"

    # 4. Send the webhook.
    body='${pushBody}'
    sig=$(printf %s "$body" | openssl dgst -sha256 -hmac '${webhookSecret}' | awk '{print "sha256="$2}')

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
    grep -F "POST /repos/myorg/myrepo/statuses/$sha" forge.log

    # Initial pending check.
    grep -F '"state":"pending"' forge.log | grep -F '"context":"medusa: evaluation"'

    # Per-job: one success and one failure.
    grep -F '"context":"medusa: packages.x86_64-linux.hello"' forge.log | grep -F '"state":"success"'
    grep -F '"context":"medusa: packages.x86_64-linux.goodbye"' forge.log | grep -F '"state":"failure"'

    # Final overall: failure (because goodbye failed).
    final=$(grep -F '"context":"medusa: evaluation"' forge.log | tail -n 1)
    echo "final overall check: $final"
    echo "$final" | grep -F '"state":"failure"'
    echo "$final" | grep -F '1 ok, 0 cached, 1 failed'

    # Shutdown.
    kill -TERM $daemon_pid
    wait $daemon_pid || true
    kill $forge_pid || true
    wait $forge_pid || true
    trap - EXIT

    touch $out
  ''
