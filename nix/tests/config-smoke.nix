{
  runCommand,
  writeText,
  writers,
  argunix,
}:
let
  token = writeText "argunix-test-github-token" "tok-value";

  config = writers.writeYAML "argunix.yaml" {
    external_url = "https://argunix.example.com";
    forges.github-myorg = {
      kind = "github";
      web_url = "https://github.com";
      token_path = "${token}";
      repos = {
        "myorg/myrepo" = { };
      };
    };
  };
in
runCommand "argunix-config-smoke"
  {
    nativeBuildInputs = [ argunix ];
    meta.description = "load YAML, open sqlite, run migrations, print 'ready'";
  }
  ''
    set -euo pipefail

    workdir=$(mktemp -d)
    cd "$workdir"

    stdout=$(argunix run --config ${config} 2> stderr.log)
    echo "--- daemon stderr ---"
    cat stderr.log
    echo "--- daemon stdout ---"
    echo "$stdout"
    echo "--- assertions ---"
    test "$stdout" = "ready"
    test -f db.sqlite

    # Re-running on the same DB should still succeed (idempotent migration,
    # idempotent boot recovery).
    argunix run --config ${config} > /dev/null

    touch $out
  ''
