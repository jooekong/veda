#!/bin/bash
# release-darwin.sh — build & upload darwin binaries to a GitLab release.
# Run on Joe's mac after pushing a tag and the linux CI release is done.
#
# Prereqs (one-time):
#   brew install --cask macfuse                              # for veda-fuse compile
#   rustup target add x86_64-apple-darwin aarch64-apple-darwin
#   export GITLAB_TOKEN=<your PAT, scope=api>                # personal access token
#
# Usage:
#   ./scripts/release-darwin.sh 0.1.0

set -euo pipefail

VERSION="${1:?version required, e.g. 0.1.0}"
TOKEN="${GITLAB_TOKEN:?need GITLAB_TOKEN env var (PAT with api scope)}"

PROJECT_ID="9462"
GITLAB_BASE="https://git.ddxq.mobi"
REGISTRY="$GITLAB_BASE/api/v4/projects/$PROJECT_ID/packages/generic/veda/$VERSION"

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

for TARGET in aarch64-apple-darwin x86_64-apple-darwin; do
    for BIN in veda veda-fuse; do
        echo ">>> building $BIN for $TARGET"
        if [ "$BIN" = "veda-fuse" ]; then
            cargo build --release --target "$TARGET" -p veda-fuse --bin veda-fuse
        else
            cargo build --release --target "$TARGET" --bin veda
        fi
        cp "target/$TARGET/release/$BIN" "$WORK/$BIN-$TARGET"
        (cd "$WORK" && shasum -a 256 "$BIN-$TARGET" > "$BIN-$TARGET.sha256")

        for f in "$BIN-$TARGET" "$BIN-$TARGET.sha256"; do
            echo "    uploading $f"
            curl --fail --header "PRIVATE-TOKEN: $TOKEN" \
                 --upload-file "$WORK/$f" \
                 "$REGISTRY/$f" >/dev/null
        done
    done
done

echo
echo ">>> appending darwin assets to release $VERSION"
for ASSET in veda-aarch64-apple-darwin \
             veda-x86_64-apple-darwin \
             veda-fuse-aarch64-apple-darwin \
             veda-fuse-x86_64-apple-darwin; do
    curl --fail --request POST \
         --header "PRIVATE-TOKEN: $TOKEN" \
         --header "Content-Type: application/json" \
         --data "{\"name\":\"$ASSET\",\"url\":\"$REGISTRY/$ASSET\",\"link_type\":\"package\"}" \
         "$GITLAB_BASE/api/v4/projects/$PROJECT_ID/releases/$VERSION/assets/links" \
        >/dev/null
    echo "    linked $ASSET"
done

echo
echo "✓ darwin release for $VERSION complete"
echo "  https://git.ddxq.mobi/middleware/dbpaas/veda/-/releases/$VERSION"
