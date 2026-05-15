#!/bin/bash
# release.sh — bump workspace version, tag both remotes, push.
# Usage: ./scripts/release.sh <github-version> <gitlab-version>
#   e.g. ./scripts/release.sh 0.1.6 0.0.12-test
# Cargo.toml + binary version follow <github-version>; <gitlab-version>
# is just an external label on the GitLab "test" stream (it stamps
# DEFAULT_VERSION_GITLAB in install.sh and tags the same commit).

set -euo pipefail

GH_VERSION="${1:?usage: $0 <github-version> <gitlab-version>  (e.g. 0.1.6 0.0.12-test)}"
GL_VERSION="${2:?usage: $0 <github-version> <gitlab-version>  (e.g. 0.1.6 0.0.12-test)}"

# Match strictly the top-level workspace.package version line so dep
# version = "1" lines don't get rewritten. -i.bak for BSD/GNU sed compat.
sed -i.bak -E "s/^version = \"[0-9]+\\.[0-9]+\\.[0-9]+\"\$/version = \"$GH_VERSION\"/" Cargo.toml
rm -f Cargo.toml.bak

# install.sh DEFAULT_VERSION_{GITHUB,GITLAB} must track each release stream's
# tag — anyone curling the script without VEDA_VERSION otherwise gets
# whatever was hardcoded at the time of the last manual edit.
sed -i.bak -E "s/^DEFAULT_VERSION_GITHUB=\"[^\"]+\"\$/DEFAULT_VERSION_GITHUB=\"$GH_VERSION\"/" install.sh
sed -i.bak -E "s/^DEFAULT_VERSION_GITLAB=\"[^\"]+\"\$/DEFAULT_VERSION_GITLAB=\"$GL_VERSION\"/" install.sh
rm -f install.sh.bak

# Refresh Cargo.lock with the new version pinned.
cargo update --workspace >/dev/null 2>&1 || true

# Sanity: build CLI to confirm version baked in.
actual=$(cargo run --release --quiet -p veda-cli --bin veda -- --version 2>/dev/null | awk '{print $2}')
if [ "$actual" != "$GH_VERSION" ]; then
    echo "version mismatch: Cargo.toml says $GH_VERSION but binary says $actual"
    exit 1
fi

# Safety: make sure ddxq/main hasn't diverged before we push.
git fetch ddxq main
if ! git merge-base --is-ancestor ddxq/main HEAD; then
    echo "ddxq/main has commits not in local HEAD — rebase or merge first; aborting"
    exit 1
fi

git add Cargo.toml Cargo.lock install.sh
git commit -m "chore: bump version to $GH_VERSION ($GL_VERSION on gitlab)"
git tag "$GH_VERSION"
git tag "$GL_VERSION"
git push origin main
git push origin "$GH_VERSION"
git push ddxq main
git push ddxq "$GL_VERSION"

echo "✓ tagged $GH_VERSION (github) + $GL_VERSION (gitlab); both CIs will build"
