# Sandboxed test that exercises the argunix-eval pipeline end-to-end.
#
# Real `nix-eval-jobs` would need network and recursive nix to evaluate a
# flake from inside a build; that requires the NixOS test framework.
# For now we ship a stand-in that pattern-matches on the
# `--flake` argument and prints canned JSON-lines, which is enough to
# validate argunix's wiring (subprocess spawn, JSON parsing, prefix
# rebuilding, output aggregation).
{
  runCommand,
  writeShellScriptBin,
  argunix,
}:
let
  fakeNixEvalJobs = writeShellScriptBin "nix-eval-jobs" ''
    set -eu
    flake=""
    apply=""
    while [ $# -gt 0 ]; do
      case "$1" in
        --flake)
          flake="$2"
          shift 2
          ;;
        --apply)
          apply="$2"
          shift 2
          ;;
        *)
          shift
          ;;
      esac
    done
    case "$flake" in
      *"#packages.x86_64-linux"*)
        echo '{"attr":"hello","drvPath":"/nix/store/aaaa-hello.drv","system":"x86_64-linux"}'
        echo '{"attr":"world","drvPath":"/nix/store/bbbb-world.drv","system":"x86_64-linux"}'
        ;;
      *"#checks.x86_64-linux"*)
        echo '{"attr":"smoke","drvPath":"/nix/store/cccc-smoke.drv","system":"x86_64-linux"}'
        ;;
      *"#devShells.x86_64-linux"*)
        echo '{"attr":"default","drvPath":"/nix/store/dddd-shell.drv","system":"x86_64-linux"}'
        ;;
      *"#packages.aarch64-linux"*)
        echo '{"attr":"hello","drvPath":"/nix/store/eeee-hello-aarch64.drv","system":"aarch64-linux"}'
        ;;
      *"#nixosConfigurations"*)
        # argunix walks nixosConfigurations once with
        # `--apply '(x: x.config.system.build.toplevel)'` so each entry
        # surfaces as the system's toplevel derivation. Sanity-check the
        # apply expression is what we expect, then emit one toplevel job.
        case "$apply" in
          *"config.system.build.toplevel"*) ;;
          *) echo "fake nix-eval-jobs: nixosConfigurations called without expected --apply (got: $apply)" >&2
             exit 2 ;;
        esac
        echo '{"attr":"laptop","drvPath":"/nix/store/ffff-nixos-laptop.drv","system":"x86_64-linux"}'
        ;;
      *"#homeConfigurations"*)
        # Same shape, different `--apply`.
        case "$apply" in
          *"activationPackage"*) ;;
          *) echo "fake nix-eval-jobs: homeConfigurations called without expected --apply (got: $apply)" >&2
             exit 2 ;;
        esac
        echo '{"attr":"alice","drvPath":"/nix/store/gggg-home-alice.drv","system":"x86_64-linux"}'
        ;;
      *)
        # No output for fragments argunix asked about that aren't in this
        # fixture. Mirror nix-eval-jobs' "no such output" behaviour by
        # exiting non-zero with empty stderr; argunix treats that as zero
        # jobs (see runner.rs).
        exit 1
        ;;
    esac
  '';

  fixtureFlake = runCommand "argunix-eval-fixture-flake" { } ''
    mkdir -p $out
    cat > $out/flake.nix <<'EOF'
    # A placeholder flake. Our fake nix-eval-jobs ignores the contents and
    # just looks at the fragment, so this only needs to exist.
    {
      description = "argunix eval-smoke fixture";
      outputs = { self }: { };
    }
    EOF
  '';
in
runCommand "argunix-eval-smoke"
  {
    nativeBuildInputs = [
      argunix
      fakeNixEvalJobs
    ];
    meta.description = "argunix eval spawns nix-eval-jobs, parses output, aggregates per-system jobs";
  }
  ''
    set -euo pipefail

    workdir=$(mktemp -d)
    cd "$workdir"

    argunix eval \
      --src ${fixtureFlake} \
      --systems x86_64-linux,aarch64-linux \
      --timeout-seconds 30 \
      > out.json 2> stderr.log

    echo "--- daemon stderr ---"
    cat stderr.log
    echo "--- jobs json ---"
    cat out.json

    # Seven jobs:
    #   2 packages.x86_64 + 1 checks.x86_64 + 1 devShells.x86_64
    # + 1 packages.aarch64
    # + 1 nixosConfigurations.laptop + 1 homeConfigurations.alice.
    # devShells/checks for aarch64 contribute 0 (fake exits 1, runner
    # treats empty-stderr-non-zero as no jobs). nixosConfigurations
    # and homeConfigurations are walked once each (no per-system
    # fan-out) with the expected --apply.
    count=$(grep -c '"attr_path"' out.json || true)
    echo "found $count jobs"
    test "$count" -eq 7

    # Spot-check a couple of full attr paths.
    grep -q '"packages.x86_64-linux.hello"' out.json
    grep -q '"checks.x86_64-linux.smoke"' out.json
    grep -q '"packages.aarch64-linux.hello"' out.json
    grep -q '/nix/store/aaaa-hello.drv' out.json
    grep -q '/nix/store/eeee-hello-aarch64.drv' out.json

    # nixosConfigurations + homeConfigurations: assert the toplevel /
    # activationPackage drv landed and the attr path has no system
    # segment (system comes from the drv, not the path).
    grep -q '"nixosConfigurations.laptop"' out.json
    grep -q '/nix/store/ffff-nixos-laptop.drv' out.json
    grep -q '"homeConfigurations.alice"' out.json
    grep -q '/nix/store/gggg-home-alice.drv' out.json

    touch $out
  ''
