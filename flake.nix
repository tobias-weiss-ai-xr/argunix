{
  description = "argunix: a declarative Nix-only CI";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    naersk.url = "github:nix-community/naersk";
    naersk.flake = false;

    disko.url = "github:nix-community/disko";
    disko.flake = false;
  };

  outputs =
    inputs:
    let
      inherit (inputs.nixpkgs) lib;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];

      eachSystem =
        systems: f:
        builtins.foldl' (
          a: s: a // builtins.mapAttrs (k: v: (a.${k} or { }) // { ${s} = v; }) (f s)
        ) { } systems;
    in
    {
      overlays =
        let
          argunix = import ./nix/overlay.nix;
          naersk = import "${inputs.naersk}/overlay.nix";
        in
        {
          inherit argunix naersk;
          default = inputs.nixpkgs.lib.composeManyExtensions [
            naersk
            argunix
          ];
        };

      nixosModules = {
        default = ./nix/module.nix;
        argunix = ./nix/module.nix;
        argunix-builder = ./nix/builder-module.nix;
      };

      # Test deployment to argunix.nix-consulting.net.
      # Provision with `nixos-anywhere`; later updates via
      # `nixos-rebuild switch --target-host`.
      nixosConfigurations.argunix = inputs.nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        modules = [
          "${inputs.disko}/module.nix"
          inputs.self.nixosModules.default
          {
            nixpkgs.overlays = [ inputs.self.overlays.default ];
          }
          ./test-deployment/configuration.nix
        ];
      };
    }
    // eachSystem systems (
      system:
      let
        pkgs = import inputs.nixpkgs {
          inherit system;
          overlays = [ inputs.self.overlays.default ];
        };

        treefmt = pkgs.treefmt.withConfig {
          settings = {
            tree-root-file = "flake.nix";
            on-unmatched = "info";
            formatter = {
              nixfmt = {
                command = lib.getExe pkgs.nixfmt;
                includes = [ "*.nix" ];
              };
              statix = {
                command = lib.getExe pkgs.statix;
                options = [ "fix" ];
                no-positional-arg-support = true;
                includes = [ "*.nix" ];
              };
              deadnix = {
                command = lib.getExe pkgs.deadnix;
                options = [ "--edit" ];
                includes = [ "*.nix" ];
              };
              rustfmt = {
                command = lib.getExe pkgs.rustfmt;
                options = [
                  "--config"
                  "skip_children=true"
                  "--edition"
                  "2024"
                ];
                includes = [ "*.rs" ];
              };
              taplo = {
                command = lib.getExe pkgs.taplo;
                options = [ "format" ];
                includes = [ "*.toml" ];
              };
              prettier = {
                command = lib.getExe pkgs.prettier;
                options = [ "--write" ];
                includes = [
                  "*.css"
                  "*.html"
                  "*.js"
                  "*.json"
                  "*.md"
                  "*.yaml"
                  "*.yml"
                ];
                # Askama templates use Jinja-like `{% ... %}` tags that
                # prettier doesn't understand — it reflows them across
                # lines and breaks rendering. Exclude the template folder.
                excludes = [ "argunix-web/templates/*" ];
              };
              shellcheck = {
                command = lib.getExe pkgs.shellcheck;
                includes = [
                  "*.sh"
                  "*.bash"
                  "*.envrc"
                  "*.envrc.*"
                ];
              };
              shfmt = {
                command = lib.getExe pkgs.shfmt;
                options = [
                  "-w"
                  "-i"
                  "2"
                  "-s"
                ];
                includes = [
                  "*.sh"
                  "*.bash"
                  "*.envrc"
                  "*.envrc.*"
                ];
              };
            };
          };
        };
      in
      {
        packages = {
          default = pkgs.argunix;
          inherit (pkgs) argunix;
        };

        formatter = treefmt;

        devShells.default = pkgs.mkShell {
          packages = [
            pkgs.cargo
            pkgs.cargo-watch
            pkgs.clippy
            pkgs.nix-eval-jobs
            pkgs.openssl
            pkgs.pkg-config
            pkgs.rust-analyzer
            pkgs.rustc
            pkgs.rustfmt
            pkgs.sqlx-cli
            pkgs.tailwindcss_4
            treefmt
          ];
        };

        checks = {
          formatting = treefmt.check inputs.self;
          inherit (pkgs) argunix;
          cargo-tests = pkgs.argunix.passthru.tests;
          config-smoke = pkgs.callPackage ./nix/tests/config-smoke.nix { };
          eval-smoke = pkgs.callPackage ./nix/tests/eval-smoke.nix { };
          build-smoke = pkgs.callPackage ./nix/tests/build-smoke.nix { };
          webhook-smoke = pkgs.callPackage ./nix/tests/webhook-smoke.nix { };
          serve-pipeline-smoke = pkgs.callPackage ./nix/tests/serve-pipeline-smoke.nix { };
          forge-status-smoke = pkgs.callPackage ./nix/tests/forge-status-smoke.nix { };
        }
        // lib.optionalAttrs pkgs.stdenv.isx86_64 {
          # limiting these tests to run on x86 because we currently have no
          # non-VM arm runners that would be fast enough to make sense.
          module-smoke = pkgs.testers.runNixOSTest ./nix/tests/module-smoke.nix;
          builder-module-smoke = pkgs.testers.runNixOSTest ./nix/tests/builder-module-smoke.nix;
          builder-enrollment = pkgs.testers.runNixOSTest ./nix/tests/builder-enrollment.nix;
          builder-build-dispatch = pkgs.testers.runNixOSTest ./nix/tests/builder-build-dispatch.nix;
          builder-stream-large = pkgs.testers.runNixOSTest ./nix/tests/builder-stream-large.nix;
          builder-transfer-stall = pkgs.testers.runNixOSTest ./nix/tests/builder-transfer-stall.nix;
          builder-liveness-watchdog = pkgs.testers.runNixOSTest ./nix/tests/builder-liveness-watchdog.nix;
          builders-parallel = pkgs.testers.runNixOSTest ./nix/tests/builders-parallel.nix;
          cache-push = pkgs.testers.runNixOSTest ./nix/tests/cache-push.nix;
          crash-recovery = pkgs.testers.runNixOSTest ./nix/tests/crash-recovery.nix;
          synthetic-flake = pkgs.testers.runNixOSTest ./nix/tests/synthetic-flake.nix;
          registry = pkgs.testers.runNixOSTest ./nix/tests/registry.nix;
          registry-push = pkgs.testers.runNixOSTest ./nix/tests/registry-push.nix;
          registry-push-oci = pkgs.testers.runNixOSTest ./nix/tests/registry-push-oci.nix;
          multi-arch = pkgs.testers.runNixOSTest ./nix/tests/multi-arch.nix;
          live-log-stream = pkgs.testers.runNixOSTest ./nix/tests/live-log-stream.nix;
        };
      }
    );
}
