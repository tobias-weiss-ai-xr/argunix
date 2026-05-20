# Crash recovery: a build in flight, both services hard-killed, daemon
# resumes the build after coming back up.
#
# Wire-up: a single NixOS VM running both the coordinator
# (`services.argunix`) and a *loopback* builder agent
# (`services.argunix-builder`, dialled at `127.0.0.1`). The coordinator
# is pool-only — it never invokes `nix-store --realise` itself — so the
# agent on the same host is what gives it build capacity. Fake `git` +
# `nix-eval-jobs` stand in for the eval phase; **the build itself is a
# real derivation**: a marker-driven sleeper that the second invocation
# detects and short-circuits.
#
# The sleeper expression is shipped to the VM as a `.nix` file and
# `nix-instantiate`d *inside* the VM by the test script — the host's
# strict-sandbox nix-daemon refuses `__noChroot = true`, while the VM
# (set to `nix.settings.sandbox = "relaxed"`) honours it and the build
# can read/write `/var/lib/argunix-test/` for the marker.
#
# We simulate the crash by `SIGKILL`-ing both `argunix.service` and
# `argunix-builder.service` and then restarting them, rather than
# qemu-quitting the VM. A full `machine.crash()` discards any disk
# writes still in qemu's write-back cache — including the `.drv` files
# nix-daemon does not fsync — and the resumed build then fails on
# "no such file or directory" trying to open the drv. That's a
# qemu/nix interaction, not an argunix recovery concern; killing the
# processes themselves leaves the VM's disk image consistent and
# exercises exactly the daemon's restart-recovery code paths
# (`mark_running_interrupted` + `requeue_interrupted_for_eval` + the
# `Building` eval-resume fast-path).
#
# Behaviour of the sleeper:
#   - first build:  drops `sleeper.attempted` under
#                   `/var/lib/argunix-test/`, fsyncs, `sleep 600` — long
#                   enough that the test crashes the VM first.
#   - second build: sees the marker, drops `sleeper.resumed`, writes
#                   `$out`, and exits.
#
# `schedule.builder_wait_seconds = 60` bridges the post-restart race:
# the daemon's resume pass dispatches the eval immediately at startup,
# whereas the loopback agent needs a couple of seconds to reconnect.
# The dispatcher pauses until the agent is back rather than instantly
# marking the resumed job `Interrupted`.
#
# Script:
#   1. Start argunix + loopback builder; wait for the agent to enrol.
#   2. `nix-instantiate` the sleeper expression on the VM and stage a
#      job record into `/var/lib/argunix/.fake-jobs.txt`.
#   3. POST a signed push webhook → eval row created (`Queued`).
#   4. Worker picks it up → eval becomes `Building`, job becomes
#      `Running` on the loopback builder, sleeper enters `sleep 600`.
#   5. SIGKILL both services. Their sqlite + state markers are on disk
#      already; the job row stays `Running`, the marker stays present.
#   6. Restart both services.
#   7. argunix's startup `mark_running_interrupted` flips the in-flight
#      job to `Interrupted`; the resume pass requeues + redispatches
#      the eval. The dispatcher waits for the loopback agent to
#      reconnect, then dispatches to it. nix-daemon re-runs the
#      sleeper which now sees `.attempted` and succeeds. Test asserts
#      the eval reaches `done` with the job at `success`.
{ pkgs, ... }:

let
  argunixPort = 8080;
  fakeForgePort = 8081;
  builderEnrollmentPort = 2222;

  githubToken = pkgs.writeText "argunix-crash-recovery-token" "tok";
  enrollmentToken = pkgs.writeText "argunix-crash-recovery-enrollment-token" "enrol";

  # Persistent across reboot, world-writable so the build sandbox's
  # nixbld user can touch markers from `__noChroot = true` build
  # scripts. Created up-front by a tmpfiles.d rule below.
  testStateDir = "/var/lib/argunix-test";

  # The sleeper *expression*, materialised as a `.nix` file the VM
  # `nix-instantiate`s at runtime. We can't pre-build it on the host
  # because the host's strict sandbox refuses `__noChroot = true`; the
  # VM is set to `sandbox = relaxed` and accepts it. `'''` is the Nix
  # escape for `''` (the inner string is itself a Nix `''…''`).
  sleeperExpr = pkgs.writeText "sleeper.nix" ''
    derivation {
      name = "sleeper";
      system = "x86_64-linux";
      builder = "${pkgs.bash}/bin/bash";
      args = [ "-c" '''
        export PATH=${pkgs.coreutils}/bin
        if [ -e ${testStateDir}/sleeper.attempted ]; then
          touch ${testStateDir}/sleeper.resumed
          echo resumed > $out
        else
          touch ${testStateDir}/sleeper.attempted
          sync
          sleep 600
        fi
      ''' ];
      __noChroot = true;
    }
  '';

  fakeGit = pkgs.writeShellScriptBin "git" ''
    set -eu
    if [ "$1" = "-C" ]; then exit 0; fi
    case "$1" in
      clone)
        dst=""
        for a in "$@"; do dst="$a"; done
        mkdir -p "$dst"
        cat > "$dst/flake.nix" <<'EOF'
    { description = "fake-cloned"; outputs = { self }: { }; }
    EOF
        exit 0
        ;;
      *) exit 0 ;;
    esac
  '';

  # Read the staged job record. The test materialises the real drv
  # via `nix-instantiate` and writes the matching JSON into this file
  # before posting the webhook.
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
      *"#packages.x86_64-linux"*)
        cat /var/lib/argunix/.fake-jobs.txt
        ;;
      *)
        exit 1
        ;;
    esac
  '';

  fakeForgePy = pkgs.writeText "fake-forge.py" ''
    import http.server, json, sys
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
            if self.path.endswith("/hooks"):
                payload = {"id": 1, "config": {}}
            else:
                payload = {"id": 1, "state": "pending"}
            self.wfile.write(json.dumps(payload).encode())
        def log_message(self, *_a):
            pass
    srv = http.server.HTTPServer(("127.0.0.1", ${toString fakeForgePort}), H)
    print("fake-forge listening on 127.0.0.1:${toString fakeForgePort}", flush=True)
    sys.stdout.flush()
    srv.serve_forever()
  '';

in
{
  name = "argunix-crash-recovery";
  globalTimeout = 900; # 15 min: build, crash, requeue + recovery

  defaults = {
    networking.dhcpcd.enable = false;
  };

  nodes.machine =
    { pkgs, lib, ... }:
    {
      imports = [
        ../module.nix
        ../builder-module.nix
      ];

      services.argunix = {
        enable = true;
        listen = "127.0.0.1:${toString argunixPort}";
        settings = {
          external_url = "https://argunix.example.com";
          builder_enrollment = {
            listen = "[::]:${toString builderEnrollmentPort}";
            token_path = "${enrollmentToken}";
          };
          # Bridge the post-crash reconnect race: the resume pass
          # re-dispatches before the loopback agent has reconnected;
          # the dispatcher waits up to this many seconds for an
          # eligible builder before giving up.
          schedule.builder_wait_seconds = 60;
          forges.gh = {
            kind = "github";
            web_url = "http://127.0.0.1:${toString fakeForgePort}";
            token_path = "${githubToken}";
            repos."myorg/myrepo" = { };
          };
        };
      };

      # Loopback builder: gives this coordinator-only host build
      # capacity by enrolling against itself.
      services.argunix-builder = {
        enable = true;
        argunixHost = "127.0.0.1";
        argunixPort = builderEnrollmentPort;
        enrollmentTokenFile = "${enrollmentToken}";
        name = "loopback";
      };

      # World-writable so the build sandbox's nixbld user can touch
      # the marker under `__noChroot = true`. Persistent across reboot.
      systemd.tmpfiles.rules = [
        "d ${testStateDir} 0777 root root - -"
      ];

      # `__noChroot` is honoured only when the sandbox is not strict.
      nix.settings.sandbox = "relaxed";

      # Stage the sleeper expression + its build inputs in the VM's
      # /nix/store so `nix-instantiate` and the subsequent realise
      # both run offline (no substituter in the test VM).
      virtualisation.additionalPaths = [
        sleeperExpr
        pkgs.bash
        pkgs.coreutils
      ];

      # Inject the eval fakes ahead of the real binaries for the
      # daemon. The build itself is real — no fake `nix-store`.
      systemd.services.argunix = {
        path = lib.mkBefore [
          fakeGit
          fakeNixEvalJobs
        ];
        after = [ "fake-forge.service" ];
        requires = [ "fake-forge.service" ];
      };

      systemd.services.fake-forge = {
        description = "fake forge for argunix-crash-recovery";
        wantedBy = [ "multi-user.target" ];
        before = [ "argunix.service" ];
        serviceConfig = {
          ExecStart = "${lib.getExe pkgs.python3} ${fakeForgePy}";
          Restart = "on-failure";
          RestartSec = 1;
        };
      };

      environment.systemPackages = [
        pkgs.curl
        pkgs.openssl
        pkgs.sqlite
      ];

      virtualisation.memorySize = 1536;
      virtualisation.writableStore = true;
    };

  testScript = ''
    import json
    import shlex

    db = "/var/lib/argunix/db.sqlite"

    def wait_for_builder():
        machine.wait_until_succeeds(
            "argunixctl --socket /run/argunix/control.sock builders list --json"
            " | tr -d ' \\n' | grep -q '\"connected\":true'",
            timeout=60,
        )

    machine.start()
    machine.wait_for_unit("fake-forge.service")
    machine.wait_for_unit("argunix.service")
    machine.wait_for_unit("argunix-builder.service")
    machine.wait_for_open_port(${toString argunixPort})
    machine.wait_for_open_port(${toString fakeForgePort})
    wait_for_builder()

    # Materialise the sleeper drv on the VM (the host cannot — strict
    # sandbox refuses `__noChroot`) and stage the matching record into
    # the fake eval-jobs payload.
    drv = machine.succeed("nix-instantiate ${sleeperExpr}").strip().splitlines()[0].strip()
    assert drv.endswith(".drv"), f"unexpected nix-instantiate output: {drv!r}"
    out = machine.succeed(f"nix-store -q --outputs {drv}").strip()
    assert out.startswith("/nix/store/"), f"unexpected outputs: {out!r}"
    record = json.dumps({
        "attr": "sleeper",
        "drvPath": drv,
        "system": "x86_64-linux",
        "outputs": {"out": out},
    })
    machine.succeed(
        "install -o argunix -g argunix -m 0644 /dev/null /var/lib/argunix/.fake-jobs.txt"
    )
    machine.succeed(
        f"printf '%s\\n' {shlex.quote(record)} >> /var/lib/argunix/.fake-jobs.txt"
    )

    # Webhook secret generated on argunix's first ensure_webhooks
    # sweep; busy-wait for the row before signing.
    machine.wait_until_succeeds(
        f"test -n \"$(sqlite3 {db} \"SELECT hex(webhook_secret) FROM repos WHERE forge='gh' AND slug='myorg/myrepo';\")\"",
        timeout=30,
    )
    secret_hex = machine.succeed(
        f"sqlite3 {db} \"SELECT hex(webhook_secret) FROM repos WHERE forge='gh' AND slug='myorg/myrepo';\""
    ).strip()
    assert secret_hex, "webhook secret never persisted"

    body = (
        '{"ref":"refs/heads/main",'
        '"after":"0123456789abcdef0123456789abcdef01234567",'
        '"repository":{"full_name":"myorg/myrepo"},'
        '"pusher":{"name":"alice"}}'
    )
    sig = machine.succeed(
        f"printf %s '{body}' | openssl dgst -sha256 -mac HMAC -macopt hexkey:{secret_hex} | awk '{{print \"sha256=\"$2}}'"
    ).strip()

    code = machine.succeed(
        "curl -s -o /tmp/resp -w '%{http_code}'"
        " -X POST http://127.0.0.1:${toString argunixPort}/webhook/github"
        " -H 'Content-Type: application/json'"
        " -H 'X-GitHub-Event: push'"
        f" -H 'X-Hub-Signature-256: {sig}'"
        f" -d '{body}'"
    ).strip()
    assert code == "202", f"expected 202 from webhook, got {code!r}"

    # The sleeper touches `sleeper.attempted` before sleeping.
    machine.wait_for_file(
        "/var/lib/argunix-test/sleeper.attempted",
        timeout=120,
    )

    # While the sleeper is sleeping the eval is Building and the job
    # Running. If either invariant changes, the test premise (crash
    # *during* a build) is broken.
    eval_status = machine.succeed(
        f"sqlite3 {db} 'SELECT status FROM evaluations WHERE id=1;'"
    ).strip()
    assert eval_status == "building", f"expected building, got {eval_status!r}"
    job_status = machine.succeed(
        f"sqlite3 {db} 'SELECT status FROM jobs WHERE eval_id=1;'"
    ).strip()
    assert job_status == "running", f"expected running, got {job_status!r}"

    # No `.resumed` marker yet — any later `.resumed` is unambiguous
    # evidence of a post-kill dispatch.
    machine.fail("test -e /var/lib/argunix-test/sleeper.resumed")

    # SIGKILL both services. Their disk state (sqlite rows + markers)
    # is consistent at this point because every commit went through
    # the kernel; only the *process* state is lost. argunix's normal
    # graceful TERM would walk the worker JoinHandle which the running
    # build prevents from closing, so we use SIGKILL outright. The
    # restart that follows must rediscover the Running job via
    # `mark_running_interrupted` and resume the eval.
    machine.execute(
        "systemctl kill --signal=SIGKILL"
        " argunix.service argunix-builder.service"
    )
    machine.wait_until_fails("systemctl is-active --quiet argunix.service")
    machine.wait_until_fails(
        "systemctl is-active --quiet argunix-builder.service"
    )
    machine.execute(
        "systemctl reset-failed"
        " argunix.service argunix-builder.service"
    )
    machine.systemctl("start argunix.service")
    machine.systemctl("start argunix-builder.service")
    machine.wait_for_unit("argunix.service")
    machine.wait_for_unit("argunix-builder.service")
    machine.wait_for_open_port(${toString argunixPort})

    # After restart, the daemon's startup `mark_running_interrupted`
    # flips the Running job to Interrupted; resume requeues +
    # redispatches the eval. `builder_wait_seconds` gives the loopback
    # agent time to reconnect. nix-daemon re-runs the sleeper which
    # now sees the marker and succeeds.
    machine.wait_for_file(
        "/var/lib/argunix-test/sleeper.resumed",
        timeout=180,
    )

    machine.wait_until_succeeds(
        f"test \"$(sqlite3 {db} 'SELECT status FROM evaluations WHERE id=1;')\" = done",
        timeout=60,
    )
    final_job = machine.succeed(
        f"sqlite3 {db} 'SELECT status FROM jobs WHERE eval_id=1;'"
    ).strip()
    assert final_job == "success", (
        f"job should have succeeded on resume, got {final_job!r}"
    )

    # interrupt_count stays at 0 — boot-time interruption is argunix's
    # fault, not the job's.
    icount = machine.succeed(
        f"sqlite3 {db} 'SELECT interrupt_count FROM jobs WHERE eval_id=1;'"
    ).strip()
    assert icount == "0", f"unexpected interrupt_count={icount!r}"
  '';
}
