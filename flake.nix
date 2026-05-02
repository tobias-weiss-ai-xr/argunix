{
  description = "medusa: a declarative Nix-only CI";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    flake-parts.url = "github:hercules-ci/flake-parts";
    flake-parts.inputs.nixpkgs-lib.follows = "nixpkgs";

    naersk.url = "github:nix-community/naersk";
    naersk.inputs.nixpkgs.follows = "nixpkgs";

    treefmt-nix.url = "github:numtide/treefmt-nix";
    treefmt-nix.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    inputs:
    inputs.flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];

      flake.overlays =
        let
          medusa = import ./nix/overlay.nix;
          naersk = final: _prev: {
            naersk = final.callPackage inputs.naersk { };
          };
        in
        {
          inherit medusa naersk;
          default = inputs.nixpkgs.lib.composeManyExtensions [
            naersk
            medusa
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
          };
        in
        {
          _module.args.pkgs = import inputs.nixpkgs {
            inherit system;
            overlays = [ inputs.self.overlays.default ];
          };

          packages = {
            default = pkgs.medusa;
            inherit (pkgs) medusa;
          };

          formatter = treefmtEval.config.build.wrapper;

          devShells.default = pkgs.mkShell {
            packages = [
              pkgs.cargo
              pkgs.rustc
              pkgs.clippy
              pkgs.rustfmt
              pkgs.rust-analyzer
              pkgs.pkg-config
              pkgs.openssl
              pkgs.nix-eval-jobs
              pkgs.sqlx-cli
              treefmtEval.config.build.wrapper
            ];
          };

          checks = {
            formatting = treefmtEval.config.build.check inputs.self;
            inherit (pkgs) medusa;
            config-smoke = pkgs.callPackage ./nix/tests/config-smoke.nix { };
            eval-smoke = pkgs.callPackage ./nix/tests/eval-smoke.nix { };
            build-smoke = pkgs.callPackage ./nix/tests/build-smoke.nix { };
          };
        };
    };
}
