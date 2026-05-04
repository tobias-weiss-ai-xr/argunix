# Sandboxed test of the M5b webhook ingestion path.
#
# Starts `medusa serve` in the background, posts a few webhooks via curl
# (with HMAC computed by openssl), verifies HTTP responses and the rows
# medusa created in sqlite, and shuts the daemon down cleanly.
{
  runCommand,
  writeText,
  writers,
  curl,
  openssl,
  python3,
  sqlite,
  medusa,
}:
let
  token = writeText "medusa-test-github-token" "tok-value";

  # Tiny in-process forge that auto-install can reach. Returns an empty hook
  # list on GET (so medusa goes straight to POST) and a fake hook id on POST.
  # Without it, ensure_webhook fails and `repos.webhook_secret` never gets
  # persisted — the test could not sign the payload.
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
            # GitHub provider deserialises the response as
            # `HookView { id, config }`, where `config` is a struct (not
            # Option). Returning just `{"id": 1}` makes serde reject the
            # body and ensure_webhook returns an error — which means the
            # secret never gets persisted to sqlite. Include `config: {}`.
            self.wfile.write(json.dumps({"id": 1, "config": {}}).encode())
        def log_message(self, *_a):
            pass
    srv = http.server.HTTPServer(("127.0.0.1", 0), H)
    print(f"fake-forge listening on 127.0.0.1:{srv.server_port}", flush=True)
    srv.serve_forever()
  '';

  configTemplate = writers.writeYAML "medusa.yaml.template" {
    external_url = "https://medusa.example.com";
    listen = "127.0.0.1:0"; # bind to an ephemeral port; we override at CLI
    forges.github-myorg = {
      kind = "github";
      api_url = "API_URL_PLACEHOLDER";
      token_path = "${token}";
      repos = {
        "myorg/myrepo" = { };
      };
    };
  };

  # Realistic-ish push payload. We pin the body so the HMAC is reproducible.
  pushBody = builtins.toJSON {
    ref = "refs/heads/main";
    after = "0123456789abcdef0123456789abcdef01234567";
    repository.full_name = "myorg/myrepo";
    pusher.name = "alice";
  };

  unknownRepoBody = builtins.toJSON {
    ref = "refs/heads/main";
    after = "0123456789abcdef0123456789abcdef01234567";
    repository.full_name = "stranger/elsewhere";
  };
in
runCommand "medusa-webhook-smoke"
  {
    nativeBuildInputs = [
      medusa
      curl
      openssl
      python3
      sqlite
    ];
    meta.description = "M5b: medusa serve accepts validated webhooks and queues evaluations";
  }
  ''
    set -euo pipefail

    workdir=$(mktemp -d)
    cd "$workdir"

    # Start the fake forge so ensure_webhook can install + persist the
    # generated secret in sqlite (we need the secret to sign payloads).
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
    sed "s|API_URL_PLACEHOLDER|http://$forge_addr|" ${configTemplate} > medusa.yaml

    medusa serve --config "$workdir/medusa.yaml" --listen "127.0.0.1:0" \
      > daemon.stdout 2> daemon.stderr &
    daemon_pid=$!
    # Use SIGKILL on cleanup. medusa's graceful shutdown can wait for
    # an idle worker to drain (worker rx never closes by itself), which
    # would block the trap's `wait` indefinitely.
    trap 'kill -KILL $daemon_pid 2>/dev/null || true; kill -KILL $forge_pid 2>/dev/null || true; wait 2>/dev/null || true' EXIT

    # Wait for the daemon to print its bound address.
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

    # /healthz should come up within a tick.
    for _ in $(seq 1 50); do
      if curl -fs "http://$listen/healthz" > /dev/null 2>&1; then
        break
      fi
      sleep 0.05
    done
    curl -fs "http://$listen/healthz" > /dev/null

    # Medusa generates and stores the webhook secret per (forge, slug)
    # at boot. Read it out of sqlite so we can sign payloads with it.
    webhookSecret=$(sqlite3 db.sqlite \
      "SELECT hex(webhook_secret) FROM repos WHERE forge='github-myorg' AND slug='myorg/myrepo';")
    test -n "$webhookSecret"
    echo "webhook secret: $webhookSecret (hex)"
    sign() {
      local body="$1"
      printf %s "$body" \
        | openssl dgst -sha256 -mac HMAC -macopt "hexkey:$webhookSecret" \
        | awk '{print "sha256="$2}'
    }

    echo "--- 1. valid push webhook ---"
    body='${pushBody}'
    sig=$(sign "$body")
    code=$(curl -s -o resp1.txt -w '%{http_code}' \
      -X POST "http://$listen/webhook/github" \
      -H 'Content-Type: application/json' \
      -H 'X-GitHub-Event: push' \
      -H "X-Hub-Signature-256: $sig" \
      -d "$body")
    cat resp1.txt; echo
    test "$code" = "202"

    echo "--- 2. wrong-secret webhook is rejected ---"
    bad_sig=$(printf %s "$body" | openssl dgst -sha256 -hmac wrong-secret | awk '{print "sha256="$2}')
    code=$(curl -s -o resp2.txt -w '%{http_code}' \
      -X POST "http://$listen/webhook/github" \
      -H 'Content-Type: application/json' \
      -H 'X-GitHub-Event: push' \
      -H "X-Hub-Signature-256: $bad_sig" \
      -d "$body")
    cat resp2.txt; echo
    test "$code" = "401"

    echo "--- 3. unknown repo is rejected ---"
    body3='${unknownRepoBody}'
    sig3=$(sign "$body3")
    code=$(curl -s -o resp3.txt -w '%{http_code}' \
      -X POST "http://$listen/webhook/github" \
      -H 'Content-Type: application/json' \
      -H 'X-GitHub-Event: push' \
      -H "X-Hub-Signature-256: $sig3" \
      -d "$body3")
    cat resp3.txt; echo
    test "$code" = "404"

    echo "--- 4. unknown forge URL path ---"
    code=$(curl -s -o /dev/null -w '%{http_code}' \
      -X POST "http://$listen/webhook/gerrit" \
      -H 'Content-Type: application/json' \
      -H 'X-GitHub-Event: push' \
      -d '{}')
    test "$code" = "404"

    echo "--- 5. ping event is acknowledged but creates no row ---"
    ping_body='{"zen":"hello","repository":{"full_name":"myorg/myrepo"}}'
    ping_sig=$(sign "$ping_body")
    code=$(curl -s -o resp5.txt -w '%{http_code}' \
      -X POST "http://$listen/webhook/github" \
      -H 'Content-Type: application/json' \
      -H 'X-GitHub-Event: ping' \
      -H "X-Hub-Signature-256: $ping_sig" \
      -d "$ping_body")
    test "$code" = "202"

    echo "--- DB shape ---"
    sqlite3 db.sqlite '.headers on' 'SELECT slug, forge FROM repos;'
    sqlite3 db.sqlite '.headers on' \
      'SELECT trigger, git_ref, sha, status FROM evaluations ORDER BY id;'

    rows=$(sqlite3 db.sqlite 'SELECT count(*) FROM evaluations;')
    test "$rows" = "1"
    sha=$(sqlite3 db.sqlite 'SELECT sha FROM evaluations LIMIT 1;')
    test "$sha" = "0123456789abcdef0123456789abcdef01234567"
    git_ref=$(sqlite3 db.sqlite 'SELECT git_ref FROM evaluations LIMIT 1;')
    test "$git_ref" = "refs/heads/main"

    # Hard shutdown — daemon worker rx never closes by itself, so a
    # graceful TERM can wait forever for the worker to drain.
    kill -KILL $daemon_pid 2>/dev/null || true
    kill -KILL $forge_pid 2>/dev/null || true
    wait 2>/dev/null || true
    trap - EXIT

    echo "--- daemon stderr (last 20 lines) ---"
    tail -n 20 daemon.stderr || true

    touch $out
  ''
