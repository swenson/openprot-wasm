#!/bin/bash
# Licensed under the Apache-2.0 license
#
# Publish the site built in www/ to the gh-pages branch, which GitHub Pages
# serves.
#
# Commits the contents of www/, with the build output that .gitignore keeps
# out of the other branches, to the local gh-pages branch. The commit's parent
# is the published branch, origin/gh-pages, if there is one. The working tree
# and the current branch are not touched.
#
# Usage:
#   tools/publish-pages.sh          # commit to the local gh-pages branch
#   tools/publish-pages.sh --push   # and push it to origin
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR/.."

push=0
case "${1:-}" in
    "") ;;
    --push) push=1 ;;
    *)
        echo "usage: $0 [--push]" >&2
        exit 1
        ;;
esac

for f in www/scenarios.json www/build-info.json www/pkg/openprot_wasm_bg.wasm; do
    if [ ! -f "$f" ]; then
        echo "ERROR: $f is missing; run ./build.sh first" >&2
        exit 1
    fi
done

# What the firmware was built from, for the commit message. A published site
# must be reproducible from the revision it names.
firmware="$(python3 - <<'PYEOF'
import json, sys

manifest = json.load(open("www/scenarios.json"))
openprot = manifest["openprot"]
if openprot["modified"]:
    sys.exit("ERROR: the firmware in www/ was built from an OpenPRoT checkout with local changes")
print(f"OpenPRoT {openprot['rev'][:12]}, {len(manifest['scenarios'])} scenarios")
PYEOF
)"
source_rev="$(git rev-parse --short=12 HEAD)"
if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
    source_rev="$source_rev with local changes"
fi

git fetch --quiet origin '+refs/heads/gh-pages:refs/remotes/origin/gh-pages' 2>/dev/null || true
parent="$(git rev-parse --quiet --verify refs/remotes/origin/gh-pages || true)"

# Build the tree in a temporary index, so that the checkout's index is left
# alone. --force adds the build output that .gitignore lists.
index="$(mktemp)"
trap 'rm -f "$index"' EXIT
rm -f "$index"
export GIT_INDEX_FILE="$index"
git --work-tree=www add --all --force .
# Tells GitHub Pages to serve the files as they are, without Jekyll.
empty="$(git hash-object -w --stdin </dev/null)"
git update-index --add --cacheinfo "100644,$empty,.nojekyll"
tree="$(git write-tree)"
unset GIT_INDEX_FILE

if [ -n "$parent" ] && [ "$(git rev-parse "$parent^{tree}")" = "$tree" ]; then
    echo "gh-pages already has this site ($parent)"
    commit="$parent"
else
    commit="$(git commit-tree "$tree" ${parent:+-p "$parent"} \
        -m "Publish openprot-wasm $source_rev" -m "Firmware: $firmware.")"
    echo "Committed the site as $commit"
fi
git update-ref refs/heads/gh-pages "$commit"

if [ "$push" = 1 ]; then
    git push origin refs/heads/gh-pages:refs/heads/gh-pages
else
    echo "To publish: git push origin refs/heads/gh-pages:refs/heads/gh-pages"
fi
