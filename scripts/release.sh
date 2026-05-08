#!/bin/bash
# release.sh — bump workspace version, tag, push.
# Usage: ./scripts/release.sh 0.1.3

set -euo pipefail

VERSION="${1:?usage: $0 <version>  (e.g. 0.1.3)}"

# Match strictly the top-level workspace.package version line so dep
# version = "1" lines don't get rewritten. -i.bak for BSD/GNU sed compat.
sed -i.bak -E "s/^version = \"[0-9]+\\.[0-9]+\\.[0-9]+\"\$/version = \"$VERSION\"/" Cargo.toml
rm -f Cargo.toml.bak

# install.sh DEFAULT_VERSION must track the workspace tag — anyone curling
# the script without VEDA_VERSION otherwise gets whatever was hardcoded at
# the time of the last manual edit (was stuck at 0.1.0 through five releases).
sed -i.bak -E "s/^DEFAULT_VERSION=\"[0-9]+\\.[0-9]+\\.[0-9]+\"\$/DEFAULT_VERSION=\"$VERSION\"/" install.sh
rm -f install.sh.bak

# Refresh Cargo.lock with the new version pinned.
cargo update --workspace >/dev/null 2>&1 || true

# Sanity: build CLI to confirm version baked in.
actual=$(cargo run --release --quiet -p veda-cli --bin veda -- --version 2>/dev/null | awk '{print $2}')
if [ "$actual" != "$VERSION" ]; then
    echo "version mismatch: Cargo.toml says $VERSION but binary says $actual"
    exit 1
fi

git add Cargo.toml Cargo.lock
git commit -m "chore: bump version to $VERSION"
git tag "$VERSION"
git push origin main
git push origin "$VERSION"

echo "✓ tagged $VERSION; CI will build the release"
