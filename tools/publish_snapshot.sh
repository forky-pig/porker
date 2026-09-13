#!/bin/bash
# Publish a Porker release to the public repo (porker-vmm/porker) as ONE squashed snapshot commit.
#
#   tools/publish_snapshot.sh porker-v0.1.1 [/path/to/public/checkout]
#
# The public repo has no shared history with this one (it was created as a single squashed
# commit), so releases are whole-tree snapshots: the tagged tree is exported, the paths in
# tools/publish_strip.txt are removed, the result replaces the public working tree, and one
# commit + one tag (v<version>) are created there for you to review and push. Nothing is pushed.
#
# Preconditions: the tag exists here; the public checkout is on `main` with a clean tree.
set -euo pipefail

TAG=${1:?usage: $0 <porker-vX.Y.Z tag> [public checkout]}
PUBLIC=${2:-${PORKER_PUBLIC:-/home/user/code/vm_hypercomputer/porker}}
REPO=$(cd "$(dirname "$0")/.." && pwd)
STRIP=$REPO/tools/publish_strip.txt
VERSION=${TAG#porker-v}

cd "$REPO"
git rev-parse -q --verify "refs/tags/$TAG" >/dev/null || { echo "error: tag $TAG does not exist here" >&2; exit 1; }
[ -d "$PUBLIC/.git" ] || { echo "error: $PUBLIC is not a git checkout" >&2; exit 1; }
[ "$(git -C "$PUBLIC" rev-parse --abbrev-ref HEAD)" = main ] || { echo "error: $PUBLIC is not on main" >&2; exit 1; }
if ! git -C "$PUBLIC" diff --quiet || ! git -C "$PUBLIC" diff --cached --quiet; then
    echo "error: $PUBLIC has uncommitted changes; commit or stash them first" >&2; exit 1
fi
if git -C "$PUBLIC" rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
    echo "error: $PUBLIC already has tag $TAG" >&2; exit 1
fi

FORK_BASE=$(git show "$TAG:src/firecracker/src/main.rs" | sed -nE 's/^const PORKER_FORK_BASE: &str = "([0-9a-f]+)";/\1/p')
FC_VERSION=$(git show "$TAG:src/firecracker/Cargo.toml" | sed -nE 's/^version = "([^"]+)"/\1/p' | head -1)

TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
git archive --format=tar "$TAG" | tar -x -C "$TMP"

# apply the strip list
shopt -s globstar nullglob dotglob
removed=0
while IFS= read -r pat; do
    pat=${pat%%#*}; pat=$(echo "$pat" | xargs 2>/dev/null || true); [ -z "$pat" ] && continue
    for p in "$TMP"/$pat; do
        [ -e "$p" ] || continue
        rm -rf "$p"; removed=$((removed+1))
    done
done < "$STRIP"

# replace the public tree (everything except its .git)
( cd "$PUBLIC" && find . -mindepth 1 -maxdepth 1 ! -name .git -exec rm -rf {} + )
( cd "$TMP" && tar -c . ) | tar -x -C "$PUBLIC"

cd "$PUBLIC"
git add -A
if git diff --cached --quiet; then echo "nothing to publish: public tree already matches $TAG"; exit 0; fi

git commit -q -F - <<EOF
Porker $VERSION: sync to upstream Firecracker $FC_VERSION ($FORK_BASE)

Snapshot of the internal tree at tag $TAG, minus the paths listed in
tools/publish_strip.txt. See CHANGELOG.md ("Porker $VERSION") for what
changed and docs/UPSTREAM.md for how upstream is tracked.
EOF
git tag -a "$TAG" -m "Porker $VERSION (upstream Firecracker $FC_VERSION @ $FORK_BASE)"

echo "== published $TAG -> $PUBLIC as $(git rev-parse --short HEAD), tag $TAG ($removed strip-list matches removed)"
git show --stat --format='%h %s' HEAD | tail -1
cat <<EOF

Review, then push:
  git -C $PUBLIC log -1 --stat
  git -C $PUBLIC diff HEAD~1 --stat
  git -C $PUBLIC push origin main $TAG
EOF
