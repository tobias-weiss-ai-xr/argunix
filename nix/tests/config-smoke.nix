{ runCommand, medusa }:
runCommand "medusa-m1-smoke"
  {
    nativeBuildInputs = [ medusa ];
    meta.description = "M1: load YAML, open sqlite, run migrations, print 'ready'";
  }
  ''
    set -euo pipefail

    workdir=$(mktemp -d)
    cd "$workdir"

    mkdir creds
    echo wh > creds/wh
    echo tok > creds/tok

    cat > medusa.yaml <<EOF
    external_url: https://medusa.example.com
    forges:
      github-myorg:
        kind: github
        api_url: https://api.github.com
        webhook_secret_path: $workdir/creds/wh
        token_path: $workdir/creds/tok
    repos:
      - slug: myorg/myrepo
        forge: github-myorg
    EOF

    stdout=$(medusa --config medusa.yaml 2> stderr.log)
    echo "--- daemon stderr ---"
    cat stderr.log
    echo "--- daemon stdout ---"
    echo "$stdout"
    echo "--- assertions ---"
    test "$stdout" = "ready"
    test -f db.sqlite

    # Re-running on the same DB should still succeed (idempotent migration,
    # idempotent boot recovery).
    medusa --config medusa.yaml > /dev/null

    touch $out
  ''
