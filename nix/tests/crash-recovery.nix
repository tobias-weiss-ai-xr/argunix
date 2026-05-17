# Full-VM crash recovery: a build in flight, hard reboot, daemon
# resumes the build after coming back up.
#
# Wire-up:
#   - one argunix node, fake `git` / `nix-eval-jobs` / `nix-store`
#     injected into the daemon's PATH so we can control build behaviour
#     deterministically inside a sealed VM;
#   - a tiny Python `fake-forge.service` so `ensure_webhooks` succeeds
#     at startup and the test can later sign a webhook payload with
#     the secret argunix persisted.
#
# The fake `nix-store --realise`:
#   - first invocation:  drops `<drv>.attempted` under `/var/lib/argunix-test/`
#                        then `sleep 600` — long enough that we crash
#                        the VM before it returns;
#   - second invocation: sees the marker from the first attempt, drops
#                        `<drv>.resumed`, writes the gc-root symlink,
#                        and exits 0 with an output path on stdout.
#
# Script:
#   1. Start argunix, wait for the daemon + fake forge to come up.
#   2. POST a signed push webhook → eval row created (`Queued`).
#   3. Worker picks it up → eval becomes `Building`, job becomes
#      `Running`, fake nix-store starts sleeping.
#   4. `machine.crash()` (qemu `quit` — power-cut equivalent).
#   5. `machine.start()` re-uses the same qcow2 image, so the sqlite DB
#      + state markers persist; argunix comes back up.
#   6. The daemon's startup pass marks the `Running` job as
#      `Interrupted`, then the resume pass flips it back to `Queued`
#      and redispatches the eval. The worker takes the
#      `eval.status == Building` fast-path, skips clone/eval/persist,
#      and re-invokes the fake nix-store — which now sees the marker
#      and succeeds. Test asserts the eval reaches `Done`.
{ pkgs, ... }:

let
  argunixPort = 8080;
  fakeForgePort = 8081;

  githubToken = pkgs.writeText "argunix-crash-recovery-token" "tok";

  # Persistent across reboot, owned by `argunix`, *outside* the
  # daemon's StateDirectory so we know it's not being touched by
  # systemd at unit-start time. Created by a tmpfiles.d rule below
  # and added to the daemon's ReadWritePaths so the sandbox lets
  # subprocesses (the fake nix-store) write into it.
  testStateDir = "/var/lib/argunix-test";

  fakeGit = pkgs.writeShellScriptBin "git" ''
    set -eu
    # `git -C <dst> <subcmd>` — fetch/checkout after the initial clone.
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
        # Single job — keeps the assertion surface tight.
        echo '{"attr":"sleeper","drvPath":"/nix/store/zzzz-sleeper.drv","system":"x86_64-linux","outputs":{"out":"/nix/store/zzzz-sleeper"}}'
        ;;
      *)
        # Empty-stderr non-zero is argunix's "no such output" signal,
        # so checks/devShells probes are silently dropped.
        exit 1
        ;;
    esac
  '';

  # Marker-driven fake nix-store: on first realise of any drv we sleep
  # forever; on a second realise of the *same* drv we succeed. The
  # marker file persists across reboot via /var/lib/argunix (qcow2).
  fakeNixStore = pkgs.writeShellScriptBin "nix-store" ''
    set -eu
    case "$1" in
      --realise) shift ;;
      *)
        echo "fake nix-store: unsupported $*" >&2
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
    state=${testStateDir}
    mkdir -p "$state"
    drv_base=$(basename "$drv")
    if [ -e "$state/$drv_base.attempted" ]; then
      echo "[fake-build] resumed: $drv" >&2
      touch "$state/$drv_base.resumed"
      out="/nix/store/''${drv_base%.drv}-out"
      echo "$out"
      if [ -n "$root" ]; then
        mkdir -p "$(dirname "$root")"
        ln -sfn "$out" "$root"
      fi
      exit 0
    fi
    touch "$state/$drv_base.attempted"
    # `crash()` is `qemu quit`, which is a kill -9 to qemu — anything
    # still in the guest kernel's page cache is lost. fsync the
    # marker and its parent dir so the second-attempt branch sees
    # it after the reboot.
    sync "$state/$drv_base.attempted" "$state"
    echo "[fake-build] starting (will be interrupted): $drv" >&2
    # Long enough that the test crashes the VM before this returns.
    # `kill_on_drop(true)` on argunix's side means a SIGKILL when the
    # daemon dies; the VM crash kills the cgroup wholesale anyway.
    sleep 600
  '';

  fakeForgePy = pkgs.writeText "fake-forge.py" ''
    # Just enough github API for `ensure_webhook` + `post_check`.
    import http.server, json, sys
    class H(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            # `ensure_webhook` GETs /repos/{slug}/hooks first to look
            # for an existing hook; empty list means "please POST".
            self.wfile.write(b"[]")
        def do_POST(self):
            length = int(self.headers.get("Content-Length", 0))
            self.rfile.read(length)
            self.send_response(201)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            # `/hooks` POSTs deserialise as HookView { id, config };
            # commit-status POSTs are loose.
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
      imports = [ ../module.nix ];

      services.argunix = {
        enable = true;
        listen = "127.0.0.1:${toString argunixPort}";
        settings = {
          external_url = "https://argunix.example.com";
          forges.gh = {
            kind = "github";
            web_url = "http://127.0.0.1:${toString fakeForgePort}";
            token_path = "${githubToken}";
            repos."myorg/myrepo" = { };
          };
        };
      };

      systemd.tmpfiles.rules = [
        "d ${testStateDir} 0750 argunix argunix - -"
      ];

      # Inject the fakes ahead of the real binaries in the daemon's
      # PATH. systemd builds PATH from this list in order; mkForce so
      # we replace the module's default list outright.
      systemd.services.argunix = {
        path = lib.mkForce [
          fakeGit
          fakeNixEvalJobs
          fakeNixStore
          pkgs.nix
          pkgs.socat
          # The fake shell scripts shell out to coreutils (mkdir, touch,
          # ln, basename, dirname, sleep) so they must be on PATH.
          pkgs.coreutils
        ];
        # Don't start until the fake forge is reachable — otherwise
        # `ensure_webhooks` silently fails and the test can't sign a
        # payload because no secret was persisted.
        after = [ "fake-forge.service" ];
        requires = [ "fake-forge.service" ];
        # `ProtectSystem=strict` on the module's serviceConfig makes
        # everything outside StateDirectory + ReadWritePaths read-only
        # for the daemon and its subprocesses. Add the test marker
        # directory so the fake nix-store can write to it.
        serviceConfig.ReadWritePaths = [ testStateDir ];
      };

      # Tiny in-VM forge. Pinned to 127.0.0.1:8081 so the daemon's
      # config can reference it as a literal URL without discovery.
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

      virtualisation.memorySize = 1024;
    };

  testScript = ''
    machine.start()
    machine.wait_for_unit("fake-forge.service")
    machine.wait_for_unit("argunix.service")
    machine.wait_for_open_port(${toString argunixPort})
    machine.wait_for_open_port(${toString fakeForgePort})

    db = "/var/lib/argunix/db.sqlite"

    # Webhook secret is generated on the daemon's first ensure_webhooks
    # sweep; busy-wait for the row to land before signing the payload.
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

    # The fake nix-store touches `<drv>.attempted` *before* sleeping.
    # Its presence is our "worker has dispatched the build" signal.
    machine.wait_for_file(
        "/var/lib/argunix-test/zzzz-sleeper.drv.attempted",
        timeout=60,
    )

    # Sanity: while the fake is sleeping the eval is `Building` and the
    # job is `Running`. If either invariant changes, the test premise
    # (we crash *during* a build) is broken.
    eval_status = machine.succeed(
        f"sqlite3 {db} 'SELECT status FROM evaluations WHERE id=1;'"
    ).strip()
    assert eval_status == "building", f"expected building, got {eval_status!r}"
    job_status = machine.succeed(
        f"sqlite3 {db} 'SELECT status FROM jobs WHERE eval_id=1;'"
    ).strip()
    assert job_status == "running", f"expected running, got {job_status!r}"

    # No `.resumed` marker yet — proves the second attempt hasn't
    # happened, so any later `.resumed` is unambiguous evidence of a
    # post-reboot dispatch.
    machine.fail("test -e /var/lib/argunix-test/zzzz-sleeper.drv.resumed")

    # Power-cut the VM. qemu `quit` drops the process; the qcow2 image
    # on disk persists, so /var/lib/argunix (DB + markers) survives.
    machine.crash()
    machine.start()
    machine.wait_for_unit("fake-forge.service")
    machine.wait_for_unit("argunix.service")
    machine.wait_for_open_port(${toString argunixPort})

    # After reboot the daemon's startup runs `mark_running_interrupted`
    # (Running → Interrupted), then the resume pass flips that job
    # back to Queued and redispatches the eval. The worker takes the
    # `Building` fast-path, skips clone/eval, and re-invokes the fake
    # nix-store — which now sees `.attempted` (synced to disk before
    # the crash) and succeeds, dropping `.resumed`.
    machine.wait_for_file(
        "/var/lib/argunix-test/zzzz-sleeper.drv.resumed",
        timeout=60,
    )

    machine.wait_until_succeeds(
        f"test \"$(sqlite3 {db} 'SELECT status FROM evaluations WHERE id=1;')\" = done",
        timeout=30,
    )
    final_job = machine.succeed(
        f"sqlite3 {db} 'SELECT status FROM jobs WHERE eval_id=1;'"
    ).strip()
    assert final_job == "success", (
        f"job should have succeeded on resume, got {final_job!r}"
    )

    # Spot-check: interrupt_count stayed at 0 because mark_running_interrupted
    # documents that boot-time interruption is argunix's fault, not the job's,
    # and shouldn't push it toward the retry cap.
    icount = machine.succeed(
        f"sqlite3 {db} 'SELECT interrupt_count FROM jobs WHERE eval_id=1;'"
    ).strip()
    assert icount == "0", f"unexpected interrupt_count={icount!r}"
  '';
}
