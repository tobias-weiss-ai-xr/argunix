# Liveness watchdog: a build in flight, the chosen builder goes
# *silently* dead mid-build, and the coordinator interrupts the job and
# reschedules it onto a healthy builder — without operator action and
# without restarting the daemon.
#
# This is the regression test for the field report "a builder (a laptop)
# went to sleep, the build stayed `running` forever with no visible
# phase". The root cause was that the only liveness signal was
# russh/TCP keepalive, which is starved when the coordinator's outbound
# flush blocks against a frozen peer — so a builder that froze
# mid-transfer was never noticed. See `design/builders.md`
# ("liveness detection").
#
# Wire-up: a coordinator (`services.argunix`, pool-only — it never
# realises locally) plus **two** separate builder VMs running the agent
# (`services.argunix-builder`). The eval phase is faked (fake `git` +
# `nix-eval-jobs`); **the build itself is a real derivation** — a 90s
# sleeper, long enough that the chosen builder is still building when we
# cut it off.
#
# Failure injection — the faithful "slept laptop" repro: rather than
# SIGKILL (which sends a TCP FIN the coordinator notices instantly) or
# SIGSTOP (kernel keeps ACKing, so transport keepalive still works), we
# **drop the agent's packets to the coordinator** with an iptables rule
# on the chosen builder. No FIN, no ACKs, no heartbeats reach the
# coordinator; the connection is silently dead. russh's own keepalive
# can't save us here — only the app-level heartbeat watchdog can.
#
# Script:
#   1. Start the coordinator + both agents; wait for both to enrol.
#   2. `nix-instantiate` the sleeper on the coordinator, stage the fake
#      eval-jobs payload, POST a signed push webhook → eval `Queued`.
#   3. Worker dispatches to whichever builder sorts first; the job goes
#      `Running` with a `builder_id`. Read which builder got it.
#   4. Partition *that* builder from the coordinator (drop dport 2222).
#   5. The heartbeat watchdog evicts the silent builder within
#      `LIVENESS_MAX_SILENCE + WATCHDOG_SCAN_INTERVAL` (~110s), aborts
#      its session, and drains the in-flight build → `build_one`'s
#      retry loop excludes it and re-dispatches to the other builder.
#   6. Assert: the daemon logged the watchdog eviction; the eval reaches
#      `done` with the job `success`; and the job's final `builder_id`
#      is the *other* builder, proving "rescheduled elsewhere".
{ pkgs, ... }:

let
  argunixPort = 8080;
  fakeForgePort = 8081;
  builderEnrollmentPort = 2222;

  githubToken = pkgs.writeText "argunix-watchdog-token" "tok";
  enrollmentToken = pkgs.writeText "argunix-watchdog-enrollment-token" "enrol";

  # A sleeper that runs long enough that the chosen builder is still
  # building when we partition it; the rescheduled run on the surviving
  # builder produces the identical output.
  #
  # `__noChroot = true` (with `nix.settings.sandbox = "relaxed"` on the
  # builders) is required because this expression is `nix-instantiate`d
  # inside the VM from a file in which `${pkgs.bash}` / `${pkgs.coreutils}`
  # have already been interpolated to bare store-path *strings* by the
  # outer (host) Nix eval. As strings they are not proper build
  # dependencies, so a strict sandbox would not mount bash's closure and
  # the builder would fail with "executing bash: No such file or
  # directory". Outside the sandbox the absolute paths resolve against
  # the builder's real store. (Same trick as `crash-recovery.nix`.)
  sleeperExpr = pkgs.writeText "watchdog-sleeper.nix" ''
    derivation {
      name = "watchdog-sleeper";
      system = "x86_64-linux";
      builder = "${pkgs.bash}/bin/bash";
      args = [ "-c" "export PATH=${pkgs.coreutils}/bin; sleep 90; echo built-by-builder > $out" ];
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
    import http.server, json, sys, subprocess
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
    subprocess.run(["${pkgs.systemd}/bin/systemd-notify", "--ready"], check=False)
    srv.serve_forever()
  '';

  # Shared builder-node config: enrol against the coordinator, advertise
  # x86_64-linux (the default), and carry iptables so the test can sever
  # the agent's link to the coordinator.
  builderNode = name: {
    imports = [ ../builder-module.nix ];
    services.argunix-builder = {
      enable = true;
      argunixHost = "coordinator";
      argunixPort = builderEnrollmentPort;
      enrollmentTokenFile = "${enrollmentToken}";
      inherit name;
    };
    environment.systemPackages = [ pkgs.iptables ];
    # `__noChroot` in the sleeper is only honoured when the sandbox
    # isn't strict; the build resolves bash/coreutils against the real
    # store. See the `sleeperExpr` comment.
    nix.settings.sandbox = "relaxed";
    # Guarantee the build inputs are present locally so the build never
    # depends on the closure push having transferred them.
    virtualisation.additionalPaths = [
      pkgs.bash
      pkgs.coreutils
    ];
    virtualisation.memorySize = 1536;
    virtualisation.writableStore = true;
  };
in
{
  name = "argunix-builder-liveness-watchdog";
  globalTimeout = 1200; # 20 min: 3 VMs, a ~110s eviction, then a refull build

  defaults = {
    networking.dhcpcd.enable = false;
  };

  nodes.coordinator =
    { pkgs, lib, ... }:
    {
      imports = [ ../module.nix ];

      services.argunix = {
        enable = true;
        listen = "127.0.0.1:${toString argunixPort}";
        settings = {
          external_url = "https://argunix.example.com";
          builder_enrollment = {
            listen = "[::]:${toString builderEnrollmentPort}";
            token_path = "${enrollmentToken}";
          };
          forges.gh = {
            kind = "github";
            web_url = "http://127.0.0.1:${toString fakeForgePort}";
            token_path = "${githubToken}";
            repos."myorg/myrepo" = { };
          };
        };
      };

      systemd.services.argunix = {
        path = lib.mkBefore [
          fakeGit
          fakeNixEvalJobs
        ];
        after = [ "fake-forge.service" ];
        requires = [ "fake-forge.service" ];
      };

      systemd.services.fake-forge = {
        description = "fake forge for argunix-builder-liveness-watchdog";
        wantedBy = [ "multi-user.target" ];
        before = [ "argunix.service" ];
        serviceConfig = {
          Type = "notify";
          NotifyAccess = "all";
          ExecStart = "${lib.getExe pkgs.python3} ${fakeForgePy}";
          Restart = "on-failure";
          RestartSec = 1;
        };
      };

      # Stage the sleeper expression + its build inputs so the
      # coordinator can `nix-instantiate` it and push its closure to a
      # builder (no substituter in the test VM).
      virtualisation.additionalPaths = [
        sleeperExpr
        pkgs.bash
        pkgs.coreutils
      ];

      environment.systemPackages = [
        pkgs.curl
        pkgs.openssl
        pkgs.sqlite
      ];

      virtualisation.memorySize = 1536;
      virtualisation.writableStore = true;
    };

  nodes.buildera = builderNode "builder-a";
  nodes.builderb = builderNode "builder-b";

  testScript = ''
    import json
    import shlex
    import time

    db = "/var/lib/argunix/db.sqlite"
    sock = "/run/argunix/control.sock"
    nodes_by_builder = {"builder-a": buildera, "builder-b": builderb}

    def builders_json():
        raw = coordinator.succeed(
            f"argunixctl --socket {sock} builders list --json"
        )
        return json.loads(raw)

    def connected_names():
        return {b["name"] for b in builders_json() if b.get("connected")}

    start_all()
    coordinator.wait_for_unit("fake-forge.service")
    coordinator.wait_for_unit("argunix.service")
    coordinator.wait_for_open_port(${toString argunixPort})
    coordinator.wait_for_open_port(${toString fakeForgePort})
    buildera.wait_for_unit("argunix-builder.service")
    builderb.wait_for_unit("argunix-builder.service")

    # Both agents must enrol before we trigger the build, so the
    # dispatcher has a fail-over target.
    coordinator.wait_until_succeeds(
        "argunixctl --socket /run/argunix/control.sock builders list --json"
        " | tr -d ' \\n' | grep -q '\"connected\":true'",
        timeout=60,
    )
    retry_attempts = 0
    while connected_names() != {"builder-a", "builder-b"}:
        retry_attempts += 1
        assert retry_attempts < 60, f"both builders never connected: {connected_names()!r}"
        time.sleep(1)

    # Materialise the sleeper drv on the coordinator and stage the fake
    # eval-jobs payload (one job, x86_64-linux — matches both builders).
    drv = coordinator.succeed("nix-instantiate ${sleeperExpr}").strip().splitlines()[0].strip()
    assert drv.endswith(".drv"), f"unexpected nix-instantiate output: {drv!r}"
    out = coordinator.succeed(f"nix-store -q --outputs {drv}").strip()
    assert out.startswith("/nix/store/"), f"unexpected outputs: {out!r}"
    record = json.dumps({
        "attr": "sleeper",
        "drvPath": drv,
        "system": "x86_64-linux",
        "outputs": {"out": out},
    })
    coordinator.succeed(
        "install -o argunix -g argunix -m 0644 /dev/null /var/lib/argunix/.fake-jobs.txt"
    )
    coordinator.succeed(
        f"printf '%s\\n' {shlex.quote(record)} >> /var/lib/argunix/.fake-jobs.txt"
    )

    # Sign + POST the push webhook.
    coordinator.wait_until_succeeds(
        f"test -n \"$(sqlite3 {db} \"SELECT hex(webhook_secret) FROM repos WHERE forge='gh' AND slug='myorg/myrepo';\")\"",
        timeout=30,
    )
    secret_hex = coordinator.succeed(
        f"sqlite3 {db} \"SELECT hex(webhook_secret) FROM repos WHERE forge='gh' AND slug='myorg/myrepo';\""
    ).strip()
    assert secret_hex, "webhook secret never persisted"

    body = (
        '{"ref":"refs/heads/main",'
        '"after":"0123456789abcdef0123456789abcdef01234567",'
        '"repository":{"full_name":"myorg/myrepo"},'
        '"pusher":{"name":"alice"}}'
    )
    sig = coordinator.succeed(
        f"printf %s '{body}' | openssl dgst -sha256 -mac HMAC -macopt hexkey:{secret_hex} | awk '{{print \"sha256=\"$2}}'"
    ).strip()
    code = coordinator.succeed(
        "curl -s -o /tmp/resp -w '%{http_code}'"
        " -X POST http://127.0.0.1:${toString argunixPort}/webhook/github"
        " -H 'Content-Type: application/json'"
        " -H 'X-GitHub-Event: push'"
        f" -H 'X-Hub-Signature-256: {sig}'"
        f" -d '{body}'"
    ).strip()
    assert code == "202", f"expected 202 from webhook, got {code!r}"

    # Wait until the job is Running on a builder, then learn which one.
    def running_builder():
        name = coordinator.succeed(
            f"sqlite3 {db} \"SELECT b.name FROM jobs j JOIN builders b"
            f" ON j.builder_id = b.id WHERE j.eval_id = 1 AND j.status = 'running';\""
        ).strip()
        return name

    coordinator.wait_until_succeeds(
        f"test -n \"$(sqlite3 {db} \"SELECT b.name FROM jobs j JOIN builders b"
        f" ON j.builder_id = b.id WHERE j.eval_id = 1 AND j.status = 'running';\")\"",
        timeout=120,
    )
    victim = running_builder()
    assert victim in nodes_by_builder, f"unexpected running builder: {victim!r}"
    survivor = "builder-b" if victim == "builder-a" else "builder-a"
    print(f"build dispatched to {victim!r}; will partition it, expecting failover to {survivor!r}")

    # Sever the chosen builder's link to the coordinator: drop its
    # outbound packets to the enrollment port. No FIN, no ACKs, no
    # heartbeats — a silently dead connection, exactly like a suspended
    # laptop. Transport keepalive cannot detect this; the app-level
    # watchdog must.
    victim_node = nodes_by_builder[victim]
    victim_node.succeed(
        "iptables -A OUTPUT -p tcp --dport ${toString builderEnrollmentPort} -j DROP"
    )
    victim_node.execute(
        "ip6tables -A OUTPUT -p tcp --dport ${toString builderEnrollmentPort} -j DROP"
    )

    # The heartbeat watchdog must notice the silence and evict the
    # builder. Its eviction log line is unique to that path (russh's own
    # teardown would not log it), so seeing it proves the watchdog — not
    # a transport-level signal — caught the silent builder.
    coordinator.wait_until_succeeds(
        "journalctl -u argunix.service --no-pager"
        " | grep -q 'went silent past the liveness threshold'",
        timeout=180,
    )

    # The interrupted job must be re-dispatched to the surviving builder
    # and run to success there — "rescheduled elsewhere".
    coordinator.wait_until_succeeds(
        f"test \"$(sqlite3 {db} 'SELECT status FROM evaluations WHERE id = 1;')\" = done",
        timeout=300,
    )
    final_job, final_builder = coordinator.succeed(
        f"sqlite3 -separator '|' {db} \"SELECT j.status, b.name FROM jobs j"
        f" JOIN builders b ON j.builder_id = b.id WHERE j.eval_id = 1;\""
    ).strip().split("|")
    assert final_job == "success", f"job should have succeeded on failover, got {final_job!r}"
    assert final_builder == survivor, (
        f"job should have been rescheduled onto {survivor!r}, ran on {final_builder!r}"
    )
  '';
}
