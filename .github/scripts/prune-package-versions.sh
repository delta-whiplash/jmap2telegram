#!/usr/bin/env bash
#
# Prune old versions of the jmap2telegram container package on GHCR.
#
# Two rules, both deliberately conservative - a wrong delete here breaks
# `docker pull` for someone:
#
#   1. Orphan manifests. A multi-arch build pushes each per-arch manifest
#      (and its attestation) untagged, then points a tagged index at them.
#      When that index is later pruned, or re-tagged away by a re-release,
#      its children are left behind, referenced by nothing. Those go.
#   2. Excess per-commit tags. ci.yml publishes every merge as
#      `sha-<commit>`; only the newest KEEP unreleased ones are kept.
#
# Nothing reachable from a tag is ever deleted, and nothing created in the
# last hour is either: an in-flight build's children look unreferenced until
# its index lands.
#
# Environment:
#   OWNER    package owner (user or org)
#   REPO     "owner/name" of the repository the package belongs to
#   PKG      package name
#   KEEP     per-commit tags to keep (default 20)
#   DRY_RUN  "true" logs what would be deleted and deletes nothing
#
set -euo pipefail

: "${OWNER:?OWNER is required}"
: "${REPO:?REPO is required}"
: "${PKG:?PKG is required}"
KEEP="${KEEP:-20}"
DRY_RUN="${DRY_RUN:-false}"

REG="ghcr.io/${REPO}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

list_versions() {
  gh api --paginate "/users/${OWNER}/packages/container/${PKG}/versions?per_page=100" \
    --jq '.[] | [.id, .name, .created_at, ((.metadata.container.tags // []) | join(" "))] | @tsv'
}

# --- inventory ---------------------------------------------------------
if ! list_versions > "${WORK}/versions.tsv" 2> "${WORK}/versions.err"; then
  if grep -q 'HTTP 404' "${WORK}/versions.err"; then
    echo "no '${PKG}' package yet - nothing to prune"
    exit 0
  fi
  cat "${WORK}/versions.err" >&2
  exit 1
fi

total="$(wc -l < "${WORK}/versions.tsv")"
tagged="$(awk -F'\t' '$4 != ""' "${WORK}/versions.tsv" | wc -l)"
echo "inventory: ${total} versions, ${tagged} tagged, $((total - tagged)) untagged"

# `name` is the manifest digest; the tags list is empty for the per-arch and
# attestation children an index points at.

# --- protected digests -------------------------------------------------
# Everything reachable from a tagged version. Tagged versions are never
# pruned, so nothing they point at may be either. Walk the graph, because an
# index can itself point at another index.
awk -F'\t' '$4 != "" {print $2}' "${WORK}/versions.tsv" | sort -u > "${WORK}/protected.txt"
cp "${WORK}/protected.txt" "${WORK}/frontier.txt"
while [ -s "${WORK}/frontier.txt" ]; do
  : > "${WORK}/next.txt"
  while read -r digest; do
    [ -z "$digest" ] && continue
    : > "${WORK}/children.txt"
    docker buildx imagetools inspect --raw "${REG}@${digest}" 2>/dev/null \
      | jq -r '(.manifests // [])[].digest' 2>/dev/null > "${WORK}/children.txt" || true
    while read -r child; do
      [ -z "$child" ] && continue
      if ! grep -qx "$child" "${WORK}/protected.txt"; then
        echo "$child" >> "${WORK}/protected.txt"
        echo "$child" >> "${WORK}/next.txt"
      fi
    done < "${WORK}/children.txt"
  done < "${WORK}/frontier.txt"
  mv "${WORK}/next.txt" "${WORK}/frontier.txt"
done
echo "protected: $(wc -l < "${WORK}/protected.txt") digests reachable from a tag"

# Commits carrying a v* tag. Their per-commit image survives even if a
# re-promotion ever gave the index a different digest than the sha-* build.
gh api --paginate "/repos/${REPO}/tags?per_page=100" --jq '.[].commit.sha' \
  | sort -u > "${WORK}/released.txt"

CUTOFF="$(date -u -d '1 hour ago' +%Y-%m-%dT%H:%M:%SZ)"
: > "${WORK}/delete.tsv"

# --- rule 1: unreferenced untagged manifests ---------------------------
while IFS=$'\t' read -r id digest created tags; do
  [ -z "${id:-}" ] && continue
  [ -n "$tags" ] && continue
  if [[ "$created" > "$CUTOFF" ]]; then
    echo "keep  ${digest} - untagged but recent, may belong to an in-flight build"
    continue
  fi
  grep -qx "$digest" "${WORK}/protected.txt" && continue
  echo "prune ${digest} - orphan manifest (${created})"
  echo "$id" >> "${WORK}/delete.tsv"
done < "${WORK}/versions.tsv"

# --- rule 2: the oldest per-commit tags --------------------------------
# Only versions whose tags are *all* `sha-*` are considered, so a released
# image - whose version also carries vX.Y.Z / X.Y / X / latest - is never
# eligible. (Promotion re-tags the very same index digest, so this is what
# protects a released per-commit image, not the checks below.)
list_versions \
  | awk -F'\t' '$4 == "" { next } { n = split($4, tags, " "); for (i = 1; i <= n; i++) if (tags[i] !~ /^sha-/) next; print $3 "\t" $1 "\t" $4 }' \
  | sort -r > "${WORK}/commits.tsv"

kept=0
while IFS=$'\t' read -r created id tag; do
  [ -z "${id:-}" ] && continue
  # Rule 1 owns untagged versions; make sure rule 2 can never reach one even
  # if the selector above ever regresses.
  case "$tag" in sha-*) ;; *) continue ;; esac
  if grep -qx "${tag#sha-}" "${WORK}/released.txt"; then
    echo "keep  ${tag} - released"
    continue
  fi
  kept=$((kept + 1))
  [ "$kept" -le "$KEEP" ] && continue
  echo "prune ${tag} - older than the newest ${KEEP} unreleased per-commit tags"
  echo "$id" >> "${WORK}/delete.tsv"
done < "${WORK}/commits.tsv"
echo "per-commit tags: kept the newest ${KEEP} unreleased, plus every released one"

# --- act ---------------------------------------------------------------
count="$(wc -l < "${WORK}/delete.tsv")"
if [ "$count" -eq 0 ]; then
  echo "nothing to prune"
  exit 0
fi
if [ "$DRY_RUN" = "true" ]; then
  echo "DRY_RUN: would delete ${count} version(s), deleting nothing"
  exit 0
fi
while read -r id; do
  [ -z "$id" ] && continue
  gh api -X DELETE "/users/${OWNER}/packages/container/${PKG}/versions/${id}" > /dev/null
done < "${WORK}/delete.tsv"
echo "deleted ${count} version(s)"
