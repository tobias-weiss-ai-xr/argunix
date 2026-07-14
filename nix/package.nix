{
  lib,
  naersk,
  runCommand,
  tailwindcss_4,
}:

let
  workspaceRoot = ./..;

  # The build source is exactly the Cargo workspace: the two manifests
  # plus every member crate's directory — with the member list read
  # from `Cargo.toml` itself. So a new crate, or a new non-Rust asset a
  # crate embeds (`include_str!`-ed data like `slogans.txt`, an Askama
  # template, a `.sql` migration), needs no change here: `Cargo.toml`
  # is the single source of truth, not an ever-growing extension
  # whitelist.
  #
  # Scoping to crate directories (rather than filtering the whole tree
  # by file extension) also keeps the build cache honest — editing
  # `docs/`, `nix/`, `flake.nix` or the README never enters this
  # fileset, so it never rebuilds the Rust crates. Untracked files are
  # already excluded upstream: a flake's source is its git tree.
  workspaceToml = fromTOML (builtins.readFile (workspaceRoot + "/Cargo.toml"));
  inherit (workspaceToml.workspace) members;

  # naersk only infers a version from a top-level `[package]` section;
  # a virtual workspace manifest like ours has none, so it would fall
  # back to "unknown" (and `builtins.trace` the whole config). Read the
  # `[workspace.package]` version — the one the member crates inherit
  # via `version.workspace = true` — and pass it explicitly.
  inherit (workspaceToml.workspace.package) version;

  src = lib.fileset.toSource {
    root = workspaceRoot;
    fileset = lib.fileset.unions (
      [
        (workspaceRoot + "/Cargo.toml")
        (workspaceRoot + "/Cargo.lock")
      ]
      ++ map (member: workspaceRoot + "/${member}") members
    );
  };

  rust = naersk.buildPackage {
    name = "argunix";
    inherit src version;
    # Askama compiles templates into the binary; nothing on disk at
    # runtime depends on the template directory. Static assets are
    # added by the wrapper derivation below.
  };

  # `cargo test --workspace` as its own derivation. We don't enable
  # `doCheck` on the binary build above because that would slow every
  # `nix build .#argunix` by however long the test suite takes; running
  # tests as a separate `nix flake check` entry keeps the build/test
  # split honest. Surfaced via passthru so flake.nix can wire it into
  # `checks.<system>` without duplicating the fileset filter.
  tests = naersk.buildPackage {
    name = "argunix-cargo-tests";
    inherit src version;
    mode = "test";
    release = false;
    cargoTestOptions = x: x ++ [ "--workspace" ];
  };

  static =
    runCommand "argunix-static"
      {
        nativeBuildInputs = [ tailwindcss_4 ];
      }
      ''
        mkdir -p $out
        # Copies everything checked into `argunix-web/static/` —
        # including the vendored, self-hosted Raleway woff2 fonts under
        # `static/fonts/` (GDPR: never hot-link Google's font CDN). They
        # are committed assets, like `htmx.min.js`, so the dev server
        # (`argunix-web-dev`, which serves the source tree directly) and
        # this build serve byte-identical files.
        cp -r ${src}/argunix-web/static/. $out/
        chmod -R u+w $out
        # Strip the placeholder ui.css that's checked in (read-only
        # straight from /nix/store after `cp`) and the input source —
        # tailwind regenerates ui.css below; input.css isn't served.
        rm -f $out/ui.css $out/input.css
        # Tailwind v4 picks up `@source` directives from input.css, so
        # no extra `--content` flag here.
        tailwindcss \
          -i ${src}/argunix-web/static/input.css \
          -o $out/ui.css \
          --minify
      '';
in
runCommand "argunix"
  {
    inherit (rust) version;
    passthru = { inherit rust static tests; };
    meta.mainProgram = "argunix";
  }
  ''
    mkdir -p $out
    cp -r ${rust}/. $out/
    mkdir -p $out/share/argunix
    cp -r ${static} $out/share/argunix/static
  ''
