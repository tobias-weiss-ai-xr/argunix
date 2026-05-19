# NixOS test: the live build-log SSE endpoint streams a real build.
#
# A webhook-driven evaluation produces one job; its derivation runs a
# shell script that prints many lines and then sleeps, holding the
# build open long enough for the test to subscribe to the live tap.
# The build is dispatched through the dynamic builder pool (gated by a
# `requiredSystemFeatures` only the builder advertises), so the daemon
# runs `nix-store --realise --log-format internal-json`, the agent
# streams the stderr back as `BuildLogChunk` frames, and the worker
# parses it into `NomEvent`s on a per-job `LiveLog`.
#
# What's being verified — the regression guard for the
# `job_log_stream` deadlock:
#   The SSE handler replays a job's *buffered prefix* of events when a
#   client connects mid-build. That replay used to push into a bounded
#   (64) channel *before* the handler returned its response, so a job
#   with more than 64 buffered events deadlocked the handler — it never
#   returned, and the live view stayed blank. The build script emits
#   ~150 lines (well over 64), so a single `curl` to the endpoint
#   exercises exactly that path: a passing run delivers >64 `nom`
#   events; a regressed run delivers zero (curl hangs until --max-time).
{ pkgs, ... }:

let
  enrollmentToken = pkgs.writeText "argunix-builder-enrollment-token" "tok";
  githubToken = pkgs.writeText "argunix-test-github-token" "ghtok";

  fakeForgePort = 7777;

  # Number of log lines the build prints. Must be comfortably above the
  # SSE channel bound (64) so the buffered-prefix replay is the case
  # under test.
  logLines = 150;

  # How long the build sleeps after printing, holding the `LiveLog`
  # open. Must exceed the test's subscribe-and-read window with margin.
  sleepSecs = 90;

  # Representative deriv: built only for its *input closure* (stdenv →
  # bash → coreutils → …), which both VMs pre-stage via
  # `additionalPaths`. The concrete job derivation is minted at runtime
  # inside the coord VM, so it stays robust against flake-eval vs
  # VM-context hash differences.
  representative =
    pkgs.runCommand "argunix-livelog-rep" { requiredSystemFeatures = [ "argunix-test" ]; }
      ''
        echo rep > $out
      '';

  # The Nix expression the coord VM `nix-instantiate`s at runtime. Its
  # `runCommand` matches `representative`, so the pre-staged input
  # closure covers it. `${toString …}` interpolates the test knobs
  # here (outer string); `$i`/`$out` stay literal shell variables, and
  # `'''` is the `''` escape for the inner `runCommand` body.
  derivExpr = pkgs.writeText "argunix-livelog-deriv.nix" ''
    let
      pkgs = import ${pkgs.path} { };
    in
    pkgs.runCommand "argunix-livelog-build" { requiredSystemFeatures = [ "argunix-test" ]; } '''
      i=1
      while [ "$i" -le ${toString logLines} ]; do
        echo "live-log build line $i of ${toString logLines}"
        i=$((i + 1))
      done
      echo "argunix-livelog: emitted ${toString logLines} lines; sleeping ${toString sleepSecs}s to hold the live tap open"
      sleep ${toString sleepSecs}
      echo built-livelog > $out
    '''
  '';

  # Stub forge: GETs return [], POST returns a created-webhook body.
  fakeForgeScript = pkgs.writeText "argunix-fake-forge.py" ''
    import http.server, json
    PORT = ${toString fakeForgePort}
    class H(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(b"[]")
        def do_POST(self):
            length = int(self.headers.get("Content-Length", 0))
            self.rfile.read(length)
            self.send_response(201)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps({"id": 1, "config": {}}).encode())
        def log_message(self, *_a):
            pass
    srv = http.server.HTTPServer(("127.0.0.1", PORT), H)
    srv.serve_forever()
  '';

  # Fake git: drop a trivial flake.nix into a `git clone` dest, no-op
  # everything else. The flake is never really evaluated — the fake
  # nix-eval-jobs below produces the job list.
  fakeFlakeStub = pkgs.writeText "fake-flake.nix" ''
    { description = "argunix-livelog-stub"; outputs = { self }: { }; }
  '';
  fakeGit = pkgs.writeShellScriptBin "git" ''
    set -eu
    if [ "$1" = "-C" ]; then
      exit 0
    fi
    case "$1" in
      clone)
        dst=""
        for a in "$@"; do dst="$a"; done
        ${pkgs.lib.getExe' pkgs.coreutils "mkdir"} -p "$dst"
        ${pkgs.lib.getExe' pkgs.coreutils "cp"} ${fakeFlakeStub} "$dst/flake.nix"
        exit 0
        ;;
      *)
        exit 0
        ;;
    esac
  '';

  # Fake nix-eval-jobs: for the `packages.x86_64-linux` fragment, emit
  # the job record the test staged into `.fake-jobs.txt`; everything
  # else evaluates to zero jobs.
  fakeNixEvalJobs = pkgs.writeShellScriptBin "nix-eval-jobs" ''
    set -eu
    flake=""
    while [ $# -gt 0 ]; do
      case "$1" in
        --flake) flake="$2"; shift 2 ;;
        *) shift ;;
      esac
    done
    case "$flake" in
      *"#packages.x86_64-linux"*) cat /var/lib/argunix/.fake-jobs.txt ;;
      *) exit 0 ;;
    esac
  '';
in
{
  name = "argunix-live-log-stream";
  globalTimeout = 20 * 60; # 2 VMs + closure push + a 90s-sleep build

  defaults = {
    networking.dhcpcd.enable = false;
  };

  nodes.coord =
    { lib, ... }:
    {
      imports = [ ../module.nix ];

      services.argunix = {
        enable = true;
        listen = "127.0.0.1:8080";
        settings = {
          external_url = "http://127.0.0.1:8080";
          builder_enrollment = {
            listen = "[::]:2222";
            token_path = "${enrollmentToken}";
          };
          forges.gh = {
            kind = "github";
            web_url = "http://127.0.0.1:${toString fakeForgePort}";
            token_path = "${githubToken}";
            repos = {
              "myorg/myrepo" = {
                watched_branches = [ "main" ];
              };
            };
          };
        };
      };

      # The fake forge must be listening before argunix.service starts,
      # so `ensure_webhooks` can persist the per-repo webhook secret.
      systemd.services.fake-forge = {
        description = "argunix test fake forge";
        wantedBy = [ "multi-user.target" ];
        before = [ "argunix.service" ];
        serviceConfig = {
          ExecStart = "${pkgs.lib.getExe pkgs.python3} ${fakeForgeScript}";
          Restart = "on-failure";
          RestartSec = 1;
        };
      };
      systemd.services.argunix = {
        after = [ "fake-forge.service" ];
        requires = [ "fake-forge.service" ];
        # Prepend the stubs so the daemon's subprocesses hit them
        # before the real binaries; mkBefore preserves the module's
        # own path entries (real nix, socat).
        path = lib.mkBefore [
          fakeNixEvalJobs
          fakeGit
        ];
      };

      environment.systemPackages = [
        pkgs.argunix
        pkgs.curl
        pkgs.jq
        pkgs.openssl
        pkgs.sqlite
        pkgs.zstd
      ];

      # Coordinator must NOT advertise `argunix-test`: any realisation
      # of the job derivation must route through the dispatch pool.
      nix.settings.system-features = [
        "kvm"
        "nixos-test"
        "benchmark"
        "big-parallel"
      ];

      virtualisation.memorySize = 1536;
      virtualisation.writableStore = true;
      virtualisation.additionalPaths = [
        pkgs.path
        representative.inputDerivation
        derivExpr
      ];
    };

  nodes.builder = {
    imports = [ ../builder-module.nix ];

    services.argunix-builder = {
      enable = true;
      argunixHost = "coord";
      argunixPort = 2222;
      enrollmentTokenFile = "${enrollmentToken}";
      name = "live-builder";
    };

    # Advertise the gating feature so dispatch routes here.
    nix.settings.system-features = [
      "kvm"
      "nixos-test"
      "benchmark"
      "big-parallel"
      "argunix-test"
    ];

    virtualisation.memorySize = 1536;
    virtualisation.writableStore = true;
    virtualisation.additionalPaths = [
      pkgs.path
      representative.inputDerivation
    ];
  };

  testScript = ''
    import json
    import shlex
    import time

    log_lines = ${toString logLines}
    sleep_secs = ${toString sleepSecs}

    start_all()

    with subtest("services start and the builder enrols"):
        coord.wait_for_unit("fake-forge.service", timeout=60)
        coord.wait_for_unit("argunix.service", timeout=60)
        coord.wait_for_open_port(8080, timeout=60)
        coord.wait_for_open_port(2222, timeout=60)
        coord.wait_for_open_port(${toString fakeForgePort}, timeout=60)
        builder.wait_for_unit("argunix-builder.service", timeout=60)

        coord.wait_until_succeeds(
            "argunixctl --socket /run/argunix/control.sock builders list --json"
            " | tr -d ' \\n' | grep -q '\"connected\":true'",
            timeout=60,
        )
        coord.succeed(
            "argunixctl --socket /run/argunix/control.sock builders list --json"
            " | tr -d ' \\n' | grep -q 'argunix-test'",
        )

    with subtest("mint the job derivation and stage the eval-jobs payload"):
        drv = coord.succeed("nix-instantiate ${derivExpr}").strip().splitlines()[0].strip()
        assert drv.endswith(".drv"), f"unexpected nix-instantiate output: {drv!r}"
        out_path = coord.succeed(f"nix-store -q --outputs {drv}").strip()
        assert out_path.startswith("/nix/store/"), f"unexpected output path: {out_path!r}"

        record = json.dumps({
            "attr": "livelog",
            "drvPath": drv,
            "system": "x86_64-linux",
            "outputs": {"out": out_path},
            "requiredSystemFeatures": ["argunix-test"],
        })
        coord.succeed(
            "install -o argunix -g argunix -m 0644 /dev/null /var/lib/argunix/.fake-jobs.txt"
        )
        coord.succeed(
            f"printf '%s\\n' {shlex.quote(record)} >> /var/lib/argunix/.fake-jobs.txt"
        )

    with subtest("trigger the evaluation via a signed webhook"):
        secret_hex = coord.succeed(
            "sqlite3 /var/lib/argunix/db.sqlite"
            " \"SELECT hex(webhook_secret) FROM repos WHERE forge='gh' AND slug='myorg/myrepo';\""
        ).strip()
        assert secret_hex, "no webhook secret in db — ensure_webhooks didn't run?"

        body = json.dumps({
            "ref": "refs/heads/main",
            "after": "1111111111111111111111111111111111111111",
            "repository": {"full_name": "myorg/myrepo"},
            "pusher": {"name": "alice"},
        })
        q_body = shlex.quote(body)
        sig = coord.succeed(
            f"printf %s {q_body}"
            f" | openssl dgst -sha256 -mac HMAC -macopt hexkey:{secret_hex}"
            " | awk '{print \"sha256=\" $2}'"
        ).strip()
        code = coord.succeed(
            "curl -s -o /tmp/resp -w '%{http_code}'"
            " -X POST http://127.0.0.1:8080/webhook/github"
            " -H 'Content-Type: application/json'"
            " -H 'X-GitHub-Event: push'"
            f" -H 'X-Hub-Signature-256: {sig}'"
            f" -d {q_body}"
        ).strip()
        assert code == "202", f"webhook not accepted: HTTP {code}"

    def running_job_id():
        raw = coord.succeed(
            "sqlite3 /var/lib/argunix/db.sqlite"
            " \"SELECT id FROM jobs WHERE status = 'running' LIMIT 1;\""
        ).strip()
        return int(raw) if raw else None

    job_id = None
    with subtest("the job dispatches to the builder and starts running"):
        deadline = time.monotonic() + 240
        while time.monotonic() < deadline:
            job_id = running_job_id()
            if job_id is not None:
                break
            time.sleep(1)
        assert job_id is not None, (
            "no job reached 'running'\n"
            + coord.succeed("journalctl -u argunix.service --no-pager -n 120")
        )
        print(f"running job id = {job_id}")

    with subtest("the live-log SSE streams the buffered prefix without deadlocking"):
        # The build prints all ~150 lines within a second or two of
        # starting, then sleeps — so once events have propagated, a
        # client connecting to the SSE hits the buffered-prefix replay
        # (>64 events). Poll until that replay is observed; a regressed
        # `job_log_stream` would deadlock and deliver zero events here,
        # so the loop would exhaust and fail.
        url = f"http://127.0.0.1:8080/api/jobs/{job_id}/log/stream"
        best = 0
        sample = ""
        deadline = time.monotonic() + 50
        while time.monotonic() < deadline:
            # curl streams for --max-time then exits non-zero (28);
            # execute() so the timeout exit isn't treated as failure.
            _rc, out = coord.execute(f"curl -sN --max-time 6 {url}")
            data_lines = [ln for ln in out.splitlines() if ln.startswith("data: ")]
            if len(data_lines) > best:
                best = len(data_lines)
                sample = out
            if best > 64:
                break
            time.sleep(1)

        if best <= 64:
            print("--- coord journal tail ---")
            print(coord.succeed("journalctl -u argunix.service --no-pager -n 120"))
            raise AssertionError(
                f"live-log SSE delivered only {best} events (need >64); "
                "job_log_stream likely deadlocked replaying the buffered prefix"
            )
        print(f"live-log SSE delivered {best} buffered events on connect")

        # The events must be the real build output: per-derivation log
        # lines attributed to the derivation, plus its build activity
        # (what the nom-style "currently building" view renders).
        assert '"kind":"line"' in sample, f"no line events in SSE stream:\n{sample[:600]}"
        assert '"kind":"act_start"' in sample, f"no activity events in SSE stream:\n{sample[:600]}"
        assert "live-log build line" in sample, (
            f"build output not present in SSE stream:\n{sample[:600]}"
        )

    with subtest("the build finishes and the stored log is per-derivation prefixed"):
        coord.wait_until_succeeds(
            f"sqlite3 /var/lib/argunix/db.sqlite \"SELECT status FROM jobs WHERE id = {job_id};\""
            " | grep -qx success",
            timeout=sleep_secs + 240,
        )
        eval_statuses = coord.succeed(
            "sqlite3 /var/lib/argunix/db.sqlite"
            " 'SELECT status FROM evaluations;'"
        ).split()
        assert eval_statuses == ["done"], f"unexpected eval statuses: {eval_statuses!r}"

        log_path = coord.succeed(
            "sqlite3 /var/lib/argunix/db.sqlite"
            f" \"SELECT log_path FROM jobs WHERE id = {job_id};\""
        ).strip()
        assert log_path, "finished job has no log_path"
        # log_path is stored relative to the daemon's state dir.
        if log_path.startswith("./"):
            log_path = "/var/lib/argunix/" + log_path[2:]
        # nom storage rendering prefixes every build-log line with the
        # short derivation name: `argunix-livelog-build> <text>`.
        stored = coord.succeed(f"zstdcat {log_path}")
        needle = f"argunix-livelog-build> live-log build line {log_lines} of {log_lines}"
        assert needle in stored, (
            f"stored log missing per-derivation-prefixed line {needle!r}\n"
            f"--- stored log tail ---\n" + "\n".join(stored.splitlines()[-20:])
        )

    print("")
    print("=" * 64)
    print("argunix live-log-stream test summary")
    print("=" * 64)
    print(f"build log lines emitted:               {log_lines}")
    print(f"buffered events delivered on SSE connect: {best}  (channel bound is 64)")
    print("job_log_stream buffered-prefix replay:  no deadlock")
    print("=" * 64)
  '';
}
