{
  runCommand,
  writeText,
  writers,
  medusa,
}:
let
  webhookSecret = writeText "medusa-test-webhook-secret" "wh-secret";
  token = writeText "medusa-test-github-token" "tok-value";

  config = writers.writeYAML "medusa.yaml" {
    external_url = "https://medusa.example.com";
    forges.github-myorg = {
      kind = "github";
      api_url = "https://api.github.com";
      webhook_secret_path = "${webhookSecret}";
      token_path = "${token}";
    };
    repos = [
      {
        slug = "myorg/myrepo";
        forge = "github-myorg";
      }
    ];
  };
in
runCommand "medusa-config-smoke"
  {
    nativeBuildInputs = [ medusa ];
    meta.description = "M1: load YAML, open sqlite, run migrations, print 'ready'";
  }
  ''
    set -euo pipefail

    workdir=$(mktemp -d)
    cd "$workdir"

    stdout=$(medusa run --config ${config} 2> stderr.log)
    echo "--- daemon stderr ---"
    cat stderr.log
    echo "--- daemon stdout ---"
    echo "$stdout"
    echo "--- assertions ---"
    test "$stdout" = "ready"
    test -f db.sqlite

    # Re-running on the same DB should still succeed (idempotent migration,
    # idempotent boot recovery).
    medusa run --config ${config} > /dev/null

    touch $out
  ''
