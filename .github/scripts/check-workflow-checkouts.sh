#!/usr/bin/env bash
#
# check-workflow-checkouts.sh - every job that runs a script from this
# repository must also check the tree out.
#
# A job's workspace is empty until it checks out, so a step that runs
# `.github/scripts/whatever.sh` in a job that never checked out dies on a
# missing file (exit 127) - not on the logic the script was written for.
# That is how v0.9.0's release failed: the promote job called the
# image-tag guard, the guard had been extracted out of inline shell into
# .github/scripts/, and the job had no checkout to find it in. The release
# stopped one step before the promotion the guard exists to protect, and
# nothing in CI said so - the guard is only exercised by a real release.
#
# So the shape of the workflow is checked here, on every pull request:
# a job that mentions a repository script must also mention a checkout.
#
# Read-only, and it never writes: it reports and exits non-zero.
set -euo pipefail

cd "$(dirname "$0")/../.."

status=0
for workflow in .github/workflows/*.yml; do
  [ -e "$workflow" ] || continue
  if ! awk -v file="$workflow" '
    # A job header is a two-space-indented key with nothing after it, and
    # only after the `jobs:` mapping. Anything shallower ends the job it
    # was in - a two-space comment between two jobs belongs to neither, so
    # it must not be attributed to the one before it.
    function flush() {
      if (job != "" && uses_script && !has_checkout) {
        printf "::error::%s: the job %s runs a repository script but never checks the tree out - it would fail on a missing file before it reached the script.\n", file, job
        bad = 1
      }
    }
    /^jobs:[ ]*$/ { in_jobs = 1; next }
    !in_jobs { next }
    /^  [A-Za-z0-9_-]+:[ ]*$/ {
      flush()
      job = $1
      sub(/:$/, "", job)
      uses_script = 0
      has_checkout = 0
      next
    }
    /^  [^ ]/ {
      # Two spaces and a non-space: the previous job ended here.
      flush()
      job = ""
      next
    }
    {
      if (job == "") next
      if ($0 ~ /\.github\/scripts\//) uses_script = 1
      if ($0 ~ /actions\/checkout@/) has_checkout = 1
    }
    END {
      flush()
      exit bad
    }
  ' "$workflow"; then
    status=1
  fi
done

if [ "$status" -ne 0 ]; then
  echo "::error::at least one job runs a repository script without a checkout."
  exit 1
fi
echo "every job that runs a repository script checks the tree out first."
