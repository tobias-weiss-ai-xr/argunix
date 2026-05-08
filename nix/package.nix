{
  lib,
  naersk,
  runCommand,
  tailwindcss_4,
}:

let
  src = lib.fileset.toSource {
    root = ./..;
    fileset = lib.fileset.fileFilter (
      file:
      file.hasExt "rs"
      || file.hasExt "sql"
      || file.hasExt "css"
      || file.hasExt "html"
      || file.hasExt "svg"
      || file.hasExt "js"
      || file.hasExt "LICENSE"
      || file.name == "Cargo.toml"
      || file.name == "Cargo.lock"
    ) ./..;
  };

  rust = naersk.buildPackage {
    name = "argunix";
    inherit src;
    # Askama compiles templates into the binary; nothing on disk at
    # runtime depends on the template directory. Static assets are
    # added by the wrapper derivation below.
  };

  static =
    runCommand "argunix-static"
      {
        nativeBuildInputs = [ tailwindcss_4 ];
      }
      ''
        mkdir -p $out
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
    passthru = { inherit rust static; };
    meta.mainProgram = "argunix";
  }
  ''
    mkdir -p $out
    cp -r ${rust}/. $out/
    mkdir -p $out/share/argunix
    cp -r ${static} $out/share/argunix/static
  ''
