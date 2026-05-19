{
  description = "argunix: a declarative Nix-only CI";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    flake-parts.url = "github:hercules-ci/flake-parts";
    flake-parts.inputs.nixpkgs-lib.follows = "nixpkgs";

    naersk.url = "github:nix-community/naersk";
    naersk.inputs.nixpkgs.follows = "nixpkgs";

    treefmt-nix.url = "github:numtide/treefmt-nix";
    treefmt-nix.inputs.nixpkgs.follows = "nixpkgs";

    disko.url = "github:nix-community/disko";
    disko.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    inputs:
    inputs.flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        #"aarch64-linux"
      ];

      flake.overlays =
        let
          argunix = import ./nix/overlay.nix;
          naersk = final: _prev: {
            naersk = final.callPackage inputs.naersk { };
          };
        in
        {
          inherit argunix naersk;
          default = inputs.nixpkgs.lib.composeManyExtensions [
            naersk
            argunix
          ];
        };

      flake.nixosModules = {
        default = ./nix/module.nix;
        argunix = ./nix/module.nix;
        argunix-builder = ./nix/builder-module.nix;
      };

      # Test deployment to argunix.nix-consulting.net.
      # Provision with `nixos-anywhere`; later updates via
      # `nixos-rebuild switch --target-host`.
      flake.nixosConfigurations.argunix = inputs.nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        modules = [
          inputs.disko.nixosModules.disko
          inputs.self.nixosModules.default
          {
            nixpkgs.overlays = [ inputs.self.overlays.default ];
          }
          ./test-deployment/configuration.nix
        ];
      };

      perSystem =
        { system, pkgs, ... }:
        let
          treefmtEval = inputs.treefmt-nix.lib.evalModule pkgs {
            projectRootFile = "flake.nix";
            programs = {
              nixfmt.enable = true;
              deadnix.enable = true;
              statix.enable = true;
              rustfmt.enable = true;
              taplo.enable = true;
              prettier.enable = true;
              shellcheck.enable = true;
              shfmt.enable = true;
            };
            # Askama templates use Jinja-like `{% ... %}` tags that
            # prettier doesn't understand — it reflows them across lines
            # and breaks rendering. Exclude the template folder.
            settings.formatter.prettier.excludes = [
              "argunix-web/templates/*"
            ];
          };
        in
        {
          _module.args.pkgs = import inputs.nixpkgs {
            inherit system;
            overlays = [ inputs.self.overlays.default ];
          };

          packages = {
            default = pkgs.argunix;
            inherit (pkgs) argunix;
          };

          formatter = treefmtEval.config.build.wrapper;

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
              treefmtEval.config.build.wrapper
            ];
          };

          checks = {
            formatting = treefmtEval.config.build.check inputs.self;
            inherit (pkgs) argunix;
            cargo-tests = pkgs.argunix.passthru.tests;
            config-smoke = pkgs.callPackage ./nix/tests/config-smoke.nix { };
            eval-smoke = pkgs.callPackage ./nix/tests/eval-smoke.nix { };
            build-smoke = pkgs.callPackage ./nix/tests/build-smoke.nix { };
            webhook-smoke = pkgs.callPackage ./nix/tests/webhook-smoke.nix { };
            serve-pipeline-smoke = pkgs.callPackage ./nix/tests/serve-pipeline-smoke.nix { };
            forge-status-smoke = pkgs.callPackage ./nix/tests/forge-status-smoke.nix { };
            module-smoke = pkgs.testers.runNixOSTest ./nix/tests/module-smoke.nix;
            builder-module-smoke = pkgs.testers.runNixOSTest ./nix/tests/builder-module-smoke.nix;
            builder-enrollment = pkgs.testers.runNixOSTest ./nix/tests/builder-enrollment.nix;
            builder-build-dispatch = pkgs.testers.runNixOSTest ./nix/tests/builder-build-dispatch.nix;
            builder-stream-large = pkgs.testers.runNixOSTest ./nix/tests/builder-stream-large.nix;
            builders-parallel = pkgs.testers.runNixOSTest ./nix/tests/builders-parallel.nix;
            cache-push = pkgs.testers.runNixOSTest ./nix/tests/cache-push.nix;
            crash-recovery = pkgs.testers.runNixOSTest ./nix/tests/crash-recovery.nix;
            synthetic-flake = pkgs.testers.runNixOSTest ./nix/tests/synthetic-flake.nix;
            registry = pkgs.testers.runNixOSTest ./nix/tests/registry.nix;
            registry-push = pkgs.testers.runNixOSTest ./nix/tests/registry-push.nix;
            registry-push-oci = pkgs.testers.runNixOSTest ./nix/tests/registry-push-oci.nix;
            multi-arch = pkgs.testers.runNixOSTest ./nix/tests/multi-arch.nix;
          };
        };
    };
}
