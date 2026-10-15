#!/usr/bin/env bash
#
# release-guards.sh - the decisions release.yml makes, extracted into one file
# so they can be exercised on every pull request (ci.yml's
# `release-guards-check`) instead of only during a release.
#
# That is not a matter of taste: logic that only an actual release runs is
# logic nobody tests, and both bugs found in this workflow so far - an
# annotated tag read as a moved tag, and a `gh release create` that failed on
# a retry instead of doing nothing - were found in production, by cutting real
# releases.
#
# Four decisions, all read-only except `tag` (which creates the tag unless
# DRY_RUN=true):
#
#   version    Cargo.toml's version must be the version being released.
#   tag        Ensure $TAG exists at $SHA, or refuse to move it.
#   image-tag  Refuse to re-point v$VERSION at a digest other than $SRC's.
#   chart      Decide publish/skip/refuse for an already-published chart
#              version, by comparing content (tarball digests are useless
#              here: `helm package` stamps mtimes into the archive).
#
# Exit status is the decision: 0 means "go ahead" - which covers "nothing to
# do" as well as "do it" - and 1 means "refuse, and the message says why".
#
# Inputs are environment variables, matching how the workflow steps are
# written: VERSION, TAG, SHA, REPO_SLUG, REPO, OWNER, SRC, LOCAL_TGZ, GH_TOKEN,
# and DRY_RUN (only `tag` writes).
set -euo pipefail

# A temp dir that outlives the function that creates it: the cleanup trap is
# set once, at the top level, because a `local` variable is already out of
# scope by the time an EXIT trap runs - under `set -u` that turned "identical
# content, nothing to push" into a failed step, which is precisely the retry
# path this script exists to protect.
guard_tmp=""
# The trap must not change the exit status. Returning the `[ -n ]` test's own
# status would turn every path into a failure; returning 0 unconditionally
# would hide a refusal. So: capture the status, clean up, exit with it.
cleanup() {
  local status=$?
  if [ -n "$guard_tmp" ]; then
    rm -rf "$guard_tmp"
  fi
  exit "$status"
}
trap cleanup EXIT

usage() {
  echo "usage: $(basename "$0") {version|tag|image-tag|chart}" >&2
  exit 2
}

fail() {
  # `::error::` is the workflow command that annotates the run; plain text
  # would still fail the step but say less about where.
  echo "::error::$1"
  exit 1
}

cmd_version() {
  : "${VERSION:?VERSION is required}"
  # First `version = "..."` line: Cargo.toml's [package] table, which cargo
  # requires to come before the tables that may quote other crates' versions.
  # `tr -d '\r'` because a checkout with CRLF line endings (a developer's
  # autocrlf, say) would otherwise fail the comparison while printing the same
  # version twice.
  local crate_version
  crate_version="$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1 | tr -d '\r')"
  if [ "$crate_version" != "$VERSION" ]; then
    fail "Cargo.toml says version = \"${crate_version}\" but this is release ${VERSION}. Set the crate version first, merge that, and release from the resulting commit."
  fi
  echo "Cargo.toml version ${crate_version} matches the release version."
}

cmd_tag() {
  : "${TAG:?TAG is required}" "${SHA:?SHA is required}" "${REPO_SLUG:?REPO_SLUG is required}" "${GH_TOKEN:?GH_TOKEN is required}"
  local url="https://api.github.com/repos/${REPO_SLUG}"
  local ref existing_sha
  ref=$(curl -s \
    -H "Authorization: Bearer $GH_TOKEN" -H "Accept: application/vnd.github+json" \
    "${url}/git/refs/tags/${TAG}")
  existing_sha=$(echo "$ref" | jq -r '.object.sha // empty')
  # An *annotated* tag's ref points at the tag object, not at the commit it
  # names, so follow it before comparing: without this, re-running a release
  # whose tag was cut with `git tag -a` looks exactly like a tag that was
  # moved, and a legitimate retry is refused. Lightweight tags need no
  # dereference.
  if [ "$(echo "$ref" | jq -r '.object.type // empty')" = "tag" ]; then
    existing_sha=$(curl -s \
      -H "Authorization: Bearer $GH_TOKEN" -H "Accept: application/vnd.github+json" \
      "${url}/git/tags/${existing_sha}" \
      | jq -r '.object.sha // empty')
  fi

  if [ -z "$existing_sha" ]; then
    if [ "${DRY_RUN:-false}" = "true" ]; then
      echo "would create tag ${TAG} at ${SHA}."
      return 0
    fi
    curl -sS -f -X POST \
      -H "Authorization: Bearer $GH_TOKEN" -H "Accept: application/vnd.github+json" \
      "${url}/git/refs" \
      -d "{\"ref\":\"refs/tags/${TAG}\",\"sha\":\"${SHA}\"}"
    echo "Created tag ${TAG} at ${SHA}."
  elif [ "$existing_sha" = "$SHA" ]; then
    echo "Tag ${TAG} already points at ${SHA}."
  else
    fail "${TAG} already points at ${existing_sha}, not at ${SHA}. Release tags are immutable: bump the version and release that, rather than moving a published tag."
  fi
}

cmd_image_tag() {
  : "${REPO:?REPO is required}" "${VERSION:?VERSION is required}" "${SRC:?SRC is required}"
  # `imagetools create` happily overwrites an existing tag, which would leave
  # vX.Y.Z pointing at a digest other than the one it published - so refuse
  # before touching it. Re-running a release for the same digest stays a
  # no-op, which keeps retrying a failed run safe.
  local published built
  published="$(docker buildx imagetools inspect "${REPO}:v${VERSION}" \
    --format '{{.Manifest.Digest}}' 2>/dev/null || true)"
  if [ -z "$published" ]; then
    echo "v${VERSION} is not published yet - nothing to refuse."
    return 0
  fi
  built="$(docker buildx imagetools inspect "$SRC" --format '{{.Manifest.Digest}}')"
  if [ "$published" = "$built" ]; then
    echo "v${VERSION} already points at ${built}; promoting it again is a no-op."
    return 0
  fi
  fail "v${VERSION} already exists at ${published}, but this build is ${built}. Release tags are immutable: bump the version and release that, rather than republishing a published tag."
}

cmd_chart() {
  : "${VERSION:?VERSION is required}" "${OWNER:?OWNER is required}" "${LOCAL_TGZ:?LOCAL_TGZ is required}"
  local oci_chart="oci://ghcr.io/${OWNER}/charts/jmap2telegram"
  local oci_ref="ghcr.io/${OWNER}/charts/jmap2telegram:${VERSION}"

  # The caller gates the push on this output; default to publishing, and only
  # downgrade it once the comparison has decided otherwise.
  if [ -n "${GITHUB_OUTPUT:-}" ]; then
    echo "publish=true" >> "$GITHUB_OUTPUT"
  fi

  if ! docker buildx imagetools inspect "$oci_ref" > /dev/null 2>&1; then
    echo "chart ${VERSION} is not published yet - publishing it."
    return 0
  fi

  # Published, so it must be identical. Every step past this point is allowed
  # to fail but not to fall through to the push: `set -e` plus the explicit
  # diff keeps a transient failure from turning into a silent overwrite.
  guard_tmp="$(mktemp -d)"
  local tmp="$guard_tmp"
  helm pull "$oci_chart" --version "$VERSION" --destination "$tmp" > /dev/null
  mkdir -p "${tmp}/published" "${tmp}/local"
  tar -xzf "${tmp}/jmap2telegram-${VERSION}.tgz" -C "${tmp}/published"
  tar -xzf "$LOCAL_TGZ" -C "${tmp}/local"
  if diff -r --brief "${tmp}/local/jmap2telegram" "${tmp}/published/jmap2telegram"; then
    echo "chart ${VERSION} is already published with identical content - nothing to push."
    if [ -n "${GITHUB_OUTPUT:-}" ]; then
      echo "publish=false" >> "$GITHUB_OUTPUT"
    fi
    return 0
  fi
  fail "chart ${VERSION} is already published with different content. Chart versions are immutable: bump the version and release that."
}

case "${1:-}" in
  version) cmd_version ;;
  tag) cmd_tag ;;
  image-tag) cmd_image_tag ;;
  chart) cmd_chart ;;
  *) usage ;;
esac
