# NixOS test: a coordinator and TWO dynamic-pool builders, each with
# `max-jobs = parallelJobs`. Two webhook-driven evaluations land on the
# coordinator; each produces `parallelJobs` derivations. The daemon
# spawns each eval's build phase as a detached task that shares the
# global `build_concurrency` semaphore, so the build phases of the
# two evals run concurrently. We assert that at some moment both
# builders carry `parallelJobs` in-flight builds simultaneously —
# i.e. all 2*parallelJobs derivations build in parallel across the pool.
#
# Concurrency knobs in play:
#   - per-builder `max-jobs`     (set via nix.settings.max-jobs)
#   - daemon `build_concurrency` (settings.schedule.build_concurrency,
#                                  set below to 2 * parallelJobs)
# Both are tied to `parallelJobs` so the "all builders saturated"
# assertion is a function of one knob.
let
  # Test knobs. `parallelJobs` is the per-builder `max-jobs` and the
  # derivations-per-eval count; `sleepSecs` is how long each build
  # spins inside `bash -c "sleep N; …"` so the polling loop has a
  # comfortable window to observe both builders saturated.
  parallelJobs = 4;
  sleepSecs = 30;

  attrNames = builtins.genList (i: "j${toString i}") (2 * parallelJobs);

  # Stub forge listening port — referenced by both the systemd unit
  # text on the coord and by the daemon's `web_url`, so it has to live
  # at the top level (pure data, no pkgs dependency).
  fakeForgePort = 7777;

  # Every helper that builds something Nix-derivable goes through this
  # factory so each VM gets its stubs built against its OWN pkgs. The
  # outer `pkgs` of a NixOS test is the test driver's (host) pkgs; if
  # the host architecture differs from a VM's (e.g. cross-arch test
  # runs), references to `pkgs.coreutils`, `pkgs.gawk`, and the stdenv
  # under `pkgs.runCommand` would otherwise embed host binaries that
  # can't execute on the guest. Calling this from inside each node's
  # module function — where `pkgs` IS that node's pkgs — keeps every
  # arch-sensitive store-path reference local to the VM consuming it.
  mkStubs =
    { pkgs, lib }:
    let
      enrollmentToken = pkgs.writeText "argunix-builder-enrollment-token" "tok";
      githubToken = pkgs.writeText "argunix-test-github-token" "ghtok";

      # A representative test deriv used only to compute the *input
      # closure* (stdenv + coreutils + bash + …) that the VM needs
      # pre-staged. The concrete drvs are minted at runtime inside the
      # coord VM via `nix-instantiate` so the test stays robust against
      # cross-system hash differences between flake-eval and VM contexts.
      representative =
        pkgs.runCommand "argunix-parallel-rep"
          {
            requiredSystemFeatures = [ "argunix-test" ];
          }
          ''
            sleep 0
            echo rep > $out
          '';

      # Nix expression file the coord VM evaluates per attr. Importing
      # `pkgs.path` (staged via `additionalPaths`) means the runtime
      # `runCommand` matches what we used for `representative.inputDerivation`,
      # so the input closure pre-staged on the coord covers every concrete
      # job. `''$out'' escapes to `$out` (shell), `''${}'` escapes Nix
      # interpolation.
      derivExpr = pkgs.writeText "argunix-parallel-deriv.nix" ''
        { name, sleepSecs }:
        let
          pkgs = import ${pkgs.path} { };
          env = {
            requiredSystemFeatures = [ "argunix-test" ];
          };
        in
        pkgs.runCommand "argunix-parallel-${"$"}{name}" env '''
          sleep ${"$"}{sleepSecs}
          echo "built-${"$"}{name}" > ${"$"}out
        '''
      '';

      # Stub forge: GETs return [], POST /hooks returns {id:1,config:{}}.
      # Listens on a fixed port so the daemon can talk to it on startup
      # (ensure_webhooks). Persists the per-repo webhook secret to sqlite,
      # which we read back to sign our test payloads.
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

      # Fake git: emit a trivial flake.nix into the dest of `git clone`
      # and no-op for `git -C <dst> …` subcommands. The flake itself
      # doesn't have to evaluate anything real — the *only* tool reading
      # it is our fake `nix-eval-jobs`, which emits a hard-coded JSON
      # for the `#packages.x86_64-linux` fragment.
      fakeFlakeStub = pkgs.writeText "fake-flake.nix" ''
        { description = "argunix-parallel-stub"; outputs = { self }: { }; }
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
            ${lib.getExe' pkgs.coreutils "mkdir"} -p "$dst"
            ${lib.getExe' pkgs.coreutils "cp"} ${fakeFlakeStub} "$dst/flake.nix"
            exit 0
            ;;
          *)
            exit 0
            ;;
        esac
      '';

      # Fake nix-eval-jobs. Each invocation that targets the
      # `packages.x86_64-linux` fragment consumes one slot of
      # `/var/lib/argunix/.fake-jobs.txt` and emits the corresponding
      # jobs lines. All other fragments (`checks`, `devShells`, …) emit
      # nothing (eval-success with zero jobs).
      #
      # The counter is per `packages.x86_64-linux` call only — `process()`
      # runs the eval phase serially, so increments are race-free: eval 1
      # consumes block 0, eval 2 consumes block 1.
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
          *"#packages.x86_64-linux"*) : ;;
          *) exit 0 ;;
        esac

        DATA=/var/lib/argunix
        counter_file="$DATA/.fake-counter"
        jobs_file="$DATA/.fake-jobs.txt"
        counter=0
        if [ -s "$counter_file" ]; then
          read -r counter < "$counter_file"
        fi
        next=$((counter + 1))
        printf '%s\n' "$next" > "$counter_file"

        block=${toString parallelJobs}
        start=$((counter * block + 1))
        end=$((start + block - 1))
        ${lib.getExe pkgs.gawk} -v s="$start" -v e="$end" 'NR>=s && NR<=e { print }' "$jobs_file"
      '';
    in
    {
      inherit
        enrollmentToken
        githubToken
        representative
        derivExpr
        fakeForgeScript
        fakeFlakeStub
        fakeGit
        fakeNixEvalJobs
        ;
    };

  # Builder VMs differ only in their `name`. Everything else — the
  # imported module, nix system-features, max-jobs, additionalPaths —
  # is identical across them, so we factor it into one function and
  # call it twice. Per-arch stubs and `lib` come from each VM's own
  # module args.
  mkBuilder =
    name:
    { pkgs, lib, ... }:
    let
      stubs = mkStubs { inherit pkgs lib; };
    in
    {
      imports = [ ../builder-module.nix ];

      services.argunix-builder = {
        enable = true;
        argunixHost = "coord";
        argunixPort = 2222;
        enrollmentTokenFile = "${stubs.enrollmentToken}";
        inherit name;
      };

      nix.settings = {
        system-features = [
          "kvm"
          "nixos-test"
          "benchmark"
          "big-parallel"
          "argunix-test"
        ];
        # The agent's `nix show-config --json` reports this verbatim
        # to the coord in its `hello`; the coord stores it as
        # `BuilderCapabilities.max_jobs` and gates dispatch on
        # `in_flight < max_jobs` (see argunix-builders/src/registry.rs).
        max-jobs = parallelJobs;
      };

      virtualisation.memorySize = 1536;
      virtualisation.writableStore = true;
      virtualisation.additionalPaths = [
        pkgs.path
        stubs.representative.inputDerivation
      ];
    };

in
{
  name = "argunix-builders-parallel";

  nodes.coord =
    { pkgs, lib, ... }:
    let
      stubs = mkStubs { inherit pkgs lib; };
    in
    {
      imports = [ ../module.nix ];

      services.argunix = {
        enable = true;
        listen = "127.0.0.1:8080";
        settings = {
          external_url = "http://127.0.0.1:8080";
          # The whole point of this test: two builders should be able
          # to run `parallelJobs` derivations each at the same time, so
          # the daemon's global cap must be at least 2*parallelJobs.
          schedule.build_concurrency = 2 * parallelJobs;
          builder_enrollment = {
            listen = "[::]:2222";
            token_path = "${stubs.enrollmentToken}";
          };
          forges.gh = {
            kind = "github";
            web_url = "http://127.0.0.1:${toString fakeForgePort}";
            token_path = "${stubs.githubToken}";
            repos = {
              # Two watched branches so the two test pushes both land
              # as evaluations (default policy drops pushes to
              # unwatched branches — see argunix-web/src/policy.rs).
              "myorg/myrepo" = {
                watched_branches = [
                  "main"
                  "feat"
                ];
              };
            };
          };
        };
      };

      # Order matters: the daemon's `ensure_webhooks` pass runs at
      # startup and persists the per-repo secret to sqlite *if and
      # only if* the forge POST succeeds. The fake forge must be
      # listening before argunix.service starts, otherwise the test's
      # signed webhook is rejected with 503 (WebhookNotProvisioned).
      systemd.services.fake-forge = {
        description = "argunix test fake forge";
        wantedBy = [ "multi-user.target" ];
        before = [ "argunix.service" ];
        serviceConfig = {
          ExecStart = "${lib.getExe pkgs.python3} ${stubs.fakeForgeScript}";
          Restart = "on-failure";
          RestartSec = 1;
        };
      };
      systemd.services.argunix = {
        after = [ "fake-forge.service" ];
        requires = [ "fake-forge.service" ];
      };

      # Prepend the fake nix-eval-jobs + fake git so the daemon's
      # subprocesses hit our stubs before the real binaries. The
      # rest of the module's `path` (real nix, socat) is preserved
      # because mkBefore merges into the existing list.
      systemd.services.argunix.path = lib.mkBefore [
        stubs.fakeNixEvalJobs
        stubs.fakeGit
      ];

      environment.systemPackages = [
        pkgs.argunix
        pkgs.curl
        pkgs.jq
        pkgs.openssl
        pkgs.sqlite
      ];

      # Coordinator must NOT advertise `argunix-test`: the runtime
      # `nix derivation show --recursive` and `nix copy` are fine, but
      # we want any actual realisation of these derivations to route
      # through the dispatch pool, not the coord's local nix-daemon.
      nix.settings.system-features = [
        "kvm"
        "nixos-test"
        "benchmark"
        "big-parallel"
      ];

      virtualisation.memorySize = 1536;
      virtualisation.writableStore = true;
      # `derivExpr` is staged here so the testScript's
      # `nix-instantiate ${stubs.derivExpr}` finds the file in the
      # coord's store. Built with the coord's own pkgs so the
      # `${pkgs.path}` reference inside its body resolves to the same
      # nixpkgs source the runtime `pkgs.runCommand` is going to call
      # against (system-features come from the env, but the input
      # closure must match the pre-staged `representative.inputDerivation`).
      virtualisation.additionalPaths = [
        pkgs.path
        stubs.representative.inputDerivation
        stubs.derivExpr
      ];
    };

  nodes.builder1 = mkBuilder "b1";
  nodes.builder2 = mkBuilder "b2";

  testScript =
    { nodes, ... }:
    let
      # Rebuild against the coord's own pkgs so the `${...derivExpr}`
      # path interpolated below matches the one the coord staged via
      # `additionalPaths`. `mkStubs` is pure, so this yields the same
      # store path as the coord-side instantiation when the coord's
      # pkgs matches the host's (default), and a *correctly-arch'd*
      # path when it doesn't.
      coordStubs = mkStubs {
        inherit (nodes.coord.nixpkgs) pkgs;
        inherit (nodes.coord) lib;
      };
    in
    ''
      import json
      import shlex
      import time

      attrs = ${builtins.toJSON attrNames}
      parallel_jobs = ${toString parallelJobs}
      sleep_secs = ${toString sleepSecs}
      expected_jobs = len(attrs)

      # ----- helpers (defined up-front; subtests below use them) -----

      def builders_json():
          raw = coord.succeed(
              "argunixctl --socket /run/argunix/control.sock builders list --json"
          )
          return json.loads(raw)

      def all_ready():
          bs = builders_json()
          names = {b["name"]: b for b in bs}
          if set(names) != {"b1", "b2"}:
              return False
          for b in bs:
              if not b.get("connected"):
                  return False
              if "argunix-test" not in b.get("features", []):
                  return False
              if b.get("max_jobs") != parallel_jobs:
                  return False
          return True

      def jobs_status_snapshot():
          return coord.succeed(
              "sqlite3 /var/lib/argunix/db.sqlite"
              " 'SELECT status, COUNT(*) FROM jobs GROUP BY status;'"
          ).strip()

      def evals_status_snapshot():
          return coord.succeed(
              "sqlite3 /var/lib/argunix/db.sqlite"
              " 'SELECT id, status FROM evaluations ORDER BY id;'"
          ).strip()

      def jobs_done_count():
          n = coord.succeed(
              "sqlite3 /var/lib/argunix/db.sqlite"
              " \"SELECT COUNT(*) FROM jobs WHERE status IN"
              " ('success','failure','cancelled','interrupted');\""
          ).strip()
          return int(n)

      def dump_failure_context(reason):
          print(f"--- failure context: {reason} ---")
          print(f"evals: {evals_status_snapshot()!r}")
          print(f"jobs:  {jobs_status_snapshot()!r}")
          print(f"peak:  {peak!r}")
          print("--- coord journal tail ---")
          print(coord.succeed("journalctl -u argunix.service --no-pager -n 120"))
          print("--- builder1 journal tail ---")
          print(builder1.succeed("journalctl -u argunix-builder.service --no-pager -n 60"))
          print("--- builder2 journal tail ---")
          print(builder2.succeed("journalctl -u argunix-builder.service --no-pager -n 60"))

      def post_webhook(branch, sha):
          body = json.dumps({
              "ref": f"refs/heads/{branch}",
              "after": sha,
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
              f"curl -s -o /tmp/resp -w '%{{http_code}}'"
              " -X POST http://127.0.0.1:8080/webhook/github"
              " -H 'Content-Type: application/json'"
              " -H 'X-GitHub-Event: push'"
              f" -H 'X-Hub-Signature-256: {sig}'"
              f" -d {q_body}"
          ).strip()
          assert code == "202", f"webhook for {branch} not accepted: HTTP {code}"

      start_all()

      with subtest("services start and both builders enrol"):
          coord.wait_for_unit("fake-forge.service", timeout=60)
          coord.wait_for_unit("argunix.service", timeout=60)
          coord.wait_for_open_port(8080, timeout=60)
          coord.wait_for_open_port(2222, timeout=60)
          coord.wait_for_open_port(${toString fakeForgePort}, timeout=60)
          builder1.wait_for_unit("argunix-builder.service", timeout=60)
          builder2.wait_for_unit("argunix-builder.service", timeout=60)

          # Both builders must enrol and advertise the gating system
          # feature plus the right max_jobs before we proceed.
          deadline = time.monotonic() + 60
          while time.monotonic() < deadline and not all_ready():
              time.sleep(0.5)
          assert all_ready(), f"builders not ready: {builders_json()!r}"

      drv_paths = {}
      out_paths = {}
      with subtest("mint runtime drvs and stage fake eval-jobs payload"):
          # Mint one .drv per attr inside the coord VM. The drvs share
          # everything-but-the-name, so each is a unique store path and
          # the daemon has to actually build all of them (nothing cached).
          for name in attrs:
              raw = coord.succeed(
                  f"nix-instantiate --argstr name {name} --argstr sleepSecs {sleep_secs}"
                  f" ${coordStubs.derivExpr}"
              ).strip()
              # nix-instantiate prints `<drv>` or `<drv>!out` with --add-root,
              # but plain invocation just prints the drv path.
              drv = raw.splitlines()[0].strip()
              assert drv.endswith(".drv"), f"unexpected nix-instantiate output for {name}: {raw!r}"
              drv_paths[name] = drv
              out = coord.succeed(f"nix-store -q --outputs {drv}").strip()
              assert out.startswith("/nix/store/"), f"unexpected output path for {name}: {out!r}"
              out_paths[name] = out

          # Stage the fake nix-eval-jobs's job list. Order: eval 1 gets
          # the first `parallel_jobs` attrs, eval 2 gets the rest. We
          # write all jobs upfront so the fake script's counter selects
          # the right block on each invocation.
          lines = []
          for name in attrs:
              rec = {
                  "attr": name,
                  "drvPath": drv_paths[name],
                  "system": "x86_64-linux",
                  "outputs": {"out": out_paths[name]},
                  "requiredSystemFeatures": ["argunix-test"],
              }
              lines.append(json.dumps(rec))
          coord.succeed(
              "install -o argunix -g argunix -m 0644 /dev/null /var/lib/argunix/.fake-jobs.txt"
          )
          for line in lines:
              coord.succeed(
                  f"printf '%s\\n' {shlex.quote(line)}"
                  " >> /var/lib/argunix/.fake-jobs.txt"
              )
          coord.succeed(
              "install -o argunix -g argunix -m 0644 /dev/null /var/lib/argunix/.fake-counter"
          )

      secret_hex = ""
      with subtest("trigger two webhook-driven evaluations"):
          # Read back the per-repo webhook secret the daemon's
          # ensure_webhooks pass persisted (forge=`gh`, slug=`myorg/myrepo`).
          secret_hex = coord.succeed(
              "sqlite3 /var/lib/argunix/db.sqlite"
              " \"SELECT hex(webhook_secret) FROM repos WHERE forge='gh' AND slug='myorg/myrepo';\""
          ).strip()
          assert secret_hex, "no webhook secret in db — ensure_webhooks didn't run?"

          # Two distinct branches so cancel-on-push doesn't supersede
          # eval 1 when eval 2 arrives (branch_key drives the cancel
          # match — see argunix-web/src/cancel.rs).
          post_webhook("main", "1111111111111111111111111111111111111111")
          post_webhook("feat", "2222222222222222222222222222222222222222")

      # Poll the control socket and remember the peak in-flight per
      # builder, plus whether we ever saw BOTH builders at full capacity
      # simultaneously. Run until all expected jobs reach a terminal
      # status in sqlite.
      #
      # Floor is `sleep_secs` once both builders are saturated; in
      # practice nix copy + closure walks add several seconds per build.
      # We bound the wait tightly so a wedged dispatch path fails fast
      # rather than burning the test driver's wall clock — and print a
      # diagnostic snapshot every few seconds so an interactive run
      # shows what's actually happening between webhook and completion.
      peak = {"b1": 0, "b2": 0}
      saw_both_saturated = False
      done = 0
      with subtest("poll until all jobs reach a terminal status"):
          overall_deadline = time.monotonic() + sleep_secs + 120
          last_diag = 0.0
          while time.monotonic() < overall_deadline:
              bs = builders_json()
              by_name = {b["name"]: b for b in bs}
              b1 = by_name.get("b1", {}).get("in_flight", 0)
              b2 = by_name.get("b2", {}).get("in_flight", 0)
              peak["b1"] = max(peak["b1"], b1)
              peak["b2"] = max(peak["b2"], b2)
              if b1 >= parallel_jobs and b2 >= parallel_jobs:
                  saw_both_saturated = True
              done = jobs_done_count()
              now = time.monotonic()
              if now - last_diag >= 10:
                  last_diag = now
                  print(
                      f"[poll t={now:.0f}s] in_flight b1={b1} b2={b2} done={done}/{expected_jobs}"
                      f" evals={evals_status_snapshot()!r}"
                  )
              if done >= expected_jobs:
                  break
              time.sleep(1)

          if done < expected_jobs:
              dump_failure_context(f"only {done}/{expected_jobs} jobs reached a terminal status before deadline")
              raise AssertionError(
                  f"jobs did not finish in time: done={done}/{expected_jobs}, peak={peak!r}"
              )

      with subtest("evaluations and jobs all finished successfully"):
          final = coord.succeed(
              "sqlite3 /var/lib/argunix/db.sqlite"
              " '.headers on'"
              " 'SELECT id, status, trigger, git_ref FROM evaluations ORDER BY id;'"
          )
          print("--- evaluations ---")
          print(final)
          eval_statuses = coord.succeed(
              "sqlite3 /var/lib/argunix/db.sqlite"
              " 'SELECT status FROM evaluations ORDER BY id;'"
          ).split()
          if eval_statuses != ["done", "done"]:
              dump_failure_context(f"unexpected eval statuses: {eval_statuses!r}")
              raise AssertionError(f"unexpected eval statuses: {eval_statuses!r}")

          rows = coord.succeed(
              "sqlite3 /var/lib/argunix/db.sqlite"
              " '.headers on'"
              " 'SELECT id, eval_id, attr_path, status FROM jobs ORDER BY id;'"
          )
          print("--- jobs ---")
          print(rows)
          job_statuses = coord.succeed(
              "sqlite3 /var/lib/argunix/db.sqlite"
              " 'SELECT status FROM jobs;'"
          ).split()
          assert all(s == "success" for s in job_statuses), (
              f"some jobs did not succeed: {job_statuses!r}"
          )
          assert len(job_statuses) == expected_jobs, (
              f"expected {expected_jobs} jobs, got {len(job_statuses)}"
          )

      with subtest("each derivation's output landed back in the coord store"):
          # Verifies the closure-pull path: each must contain its
          # attr-specific text.
          for name in attrs:
              contents = coord.succeed(f"cat {out_paths[name]}").strip()
              assert contents == f"built-{name}", (
                  f"deriv {name} produced unexpected output {contents!r}"
              )

      with subtest("both builders ever saturated simultaneously"):
          # The headline assertion: at some sampled moment, both
          # builders were carrying their full `max_jobs` of in-flight
          # builds — i.e. all 2*parallel_jobs derivations were
          # realising in parallel across the pool.
          if not saw_both_saturated:
              dump_failure_context(f"never observed both builders saturated simultaneously; peak={peak!r}")
              raise AssertionError(
                  f"never observed both builders saturated simultaneously; peak={peak!r}"
              )

      print("")
      print("=" * 64)
      print("argunix builders-parallel summary")
      print("=" * 64)
      print(f"parallel_jobs (per builder max-jobs):  {parallel_jobs}")
      print(f"sleep per build (s):                   {sleep_secs}")
      print(f"derivations total:                     {expected_jobs}")
      print("evaluations:                           2 (branches main, feat)")
      print(f"peak in-flight per builder:            b1={peak['b1']} b2={peak['b2']}")
      print(f"both builders ever saturated at once:  {saw_both_saturated}")
      print("=" * 64)
    '';
}
