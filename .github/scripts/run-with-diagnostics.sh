#!/usr/bin/env bash
set -uo pipefail

log_file=".cshell-ci-${GITHUB_JOB:-job}.log"
summary_file="${GITHUB_STEP_SUMMARY:-/dev/null}"
set +e
"$@" 2>&1 | tee "$log_file"
status=${PIPESTATUS[0]}
set -e

if (( status != 0 )); then
  {
    echo "### Failed command"
    echo
    echo "\`\`\`text"
    printf '%q ' "$@"
    echo
    tail -n 160 "$log_file"
    echo "\`\`\`"
  } >> "$summary_file"

  node - "$log_file" <<'NODE'
const fs = require("fs");
const path = process.argv[2];
const plain = fs.readFileSync(path, "utf8")
  .replace(/\x1b\[[0-9;]*m/g, "");
const lines = plain.split(/\r?\n/).slice(-50);
const message = lines.join("\n")
  .replaceAll("%", "%25")
  .replaceAll("\r", "%0D")
  .replaceAll("\n", "%0A");
console.log(`::error title=CShell CI failure::${message}`);
NODE
fi

exit "$status"
