#!/bin/sh
# install.sh — Veda CLI installer
#
# Usage:
#   curl -fL https://raw.githubusercontent.com/jooekong/veda/main/install.sh | sh
#   curl -fL ... | sh -s -- --with-fuse        # also install veda-fuse
#
# Env overrides:
#   VEDA_VERSION       Version to install (default below)
#   VEDA_INSTALL_DIR   Where to put binaries (default: $HOME/.local/bin)

set -eu

# ====== config ======
GITHUB_REPO="jooekong/veda"
DEFAULT_VERSION="0.1.5"
DEFAULT_SERVER="http://10.79.51.161:3000"

# ====== state ======
WITH_FUSE=0
VERSION="${VEDA_VERSION:-$DEFAULT_VERSION}"
DEST="${VEDA_INSTALL_DIR:-$HOME/.local/bin}"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT INT TERM

# ====== helpers ======
log()  { printf '%s\n' "$*" >&2; }
err()  { log "error: $*"; exit 1; }
warn() { log "warn: $*"; }

detect_platform() {
    os="${1:-$(uname -s)}"
    arch="${2:-$(uname -m)}"
    case "$os-$arch" in
        Darwin-x86_64) echo "x86_64-apple-darwin" ;;
        Darwin-arm64)  echo "aarch64-apple-darwin" ;;
        Linux-x86_64)  echo "x86_64-unknown-linux-gnu" ;;
        Linux-aarch64) echo "aarch64-unknown-linux-gnu" ;;
        *) err "unsupported platform: $os-$arch" ;;
    esac
}

download_asset() {
    name="$1"
    url="https://github.com/$GITHUB_REPO/releases/download/$VERSION/$name"
    curl --fail --show-error --silent --location \
         "$url" -o "$TMP/$name" \
        || err "download failed: $name (check version $VERSION exists in releases)"
}

verify_sha256() {
    name="$1"
    if command -v sha256sum >/dev/null 2>&1; then
        (cd "$TMP" && sha256sum -c "$name.sha256" >/dev/null) \
            || err "checksum mismatch on $name"
    else
        # macOS uses shasum
        (cd "$TMP" && shasum -a 256 -c "$name.sha256" >/dev/null) \
            || err "checksum mismatch on $name"
    fi
}

install_binary() {
    bin_name="$1"   # veda or veda-fuse
    target="$2"
    asset="$bin_name-$target"
    log "downloading $asset..."
    download_asset "$asset"
    download_asset "$asset.sha256"
    verify_sha256 "$asset"
    mkdir -p "$DEST"
    mv "$TMP/$asset" "$DEST/$bin_name"
    chmod +x "$DEST/$bin_name"
    log "installed $bin_name → $DEST/$bin_name"
}

preflight_fuse() {
    os="${1:-$(uname -s)}"
    case "$os" in
        Darwin)
            if [ ! -e /Library/Filesystems/macfuse.fs ]; then
                log ""
                log "veda CLI is installed."
                log "But macFUSE is not installed; skipping veda-fuse."
                log "To install macFUSE:"
                log "  brew install --cask macfuse"
                log "Then authorize the system extension in:"
                log "  System Settings → Privacy & Security"
                log "Re-run install.sh --with-fuse afterwards."
                exit 0
            fi
            ;;
        Linux)
            if ! ldconfig -p 2>/dev/null | grep -q libfuse3; then
                distro_id="unknown"
                if [ -r /etc/os-release ]; then
                    # shellcheck disable=SC1091
                    . /etc/os-release
                    distro_id="${ID:-unknown}"
                fi
                case "$distro_id" in
                    debian|ubuntu)
                        cmd="sudo apt-get install -y libfuse3-3 fuse3" ;;
                    fedora|rhel|centos|rocky|alma)
                        cmd="sudo dnf install -y fuse3" ;;
                    arch|manjaro)
                        cmd="sudo pacman -S --noconfirm fuse3" ;;
                    *)
                        log ""
                        log "veda CLI is installed."
                        log "veda-fuse needs libfuse3 but distro $distro_id has no auto-install recipe."
                        log "Install libfuse3 manually, then re-run install.sh --with-fuse."
                        exit 0
                        ;;
                esac
                log ""
                log "veda CLI is installed."
                log "veda-fuse needs libfuse3. Recommended: $cmd"
                printf "Run it now? [y/N] " >&2
                read -r ans
                case "$ans" in
                    y|Y|yes|YES)
                        eval "$cmd" || { warn "FUSE install failed; CLI is ready"; exit 0; }
                        ;;
                    *)
                        log "Skipped. CLI is ready."
                        exit 0
                        ;;
                esac
            fi
            ;;
    esac
}

install_skill() {
    if [ -d "$HOME/.claude" ]; then
        mkdir -p "$HOME/.claude/skills/veda"
        url="https://github.com/$GITHUB_REPO/releases/download/$VERSION/skill.md"
        if curl --fail --silent --location \
                "$url" -o "$HOME/.claude/skills/veda/SKILL.md"; then
            log "skill installed → ~/.claude/skills/veda/SKILL.md"
        else
            warn "could not fetch skill.md; skipping (CLI is fine)"
        fi
    fi
}

print_next_steps() {
    log ""
    log "✓ veda is installed at $DEST/veda"
    case ":$PATH:" in
        *":$DEST:"*) ;;
        *)
            log ""
            log "⚠ $DEST is not in your PATH. Add this to your shell rc:"
            log "    export PATH=\"$DEST:\$PATH\""
            ;;
    esac
    log ""
    log "Next: run 'veda init' for guided setup (~30 seconds), or"
    log "      'veda init --server $DEFAULT_SERVER --login --email ... --password ...' to"
    log "      attach an existing account non-interactively."
    log ""
    log "After setup, 'veda status' confirms config + server reachability."
    log ""
    if [ -d "$HOME/.claude" ]; then
        log "Claude Code: skill auto-installed; just ask Claude to use veda."
    else
        log "Other agents: have them read https://raw.githubusercontent.com/$GITHUB_REPO/main/skill.md"
    fi
}

main() {
    while [ $# -gt 0 ]; do
        case "$1" in
            --with-fuse) WITH_FUSE=1 ;;
            -h|--help)
                cat >&2 <<EOF
Usage: install.sh [--with-fuse]

Env overrides:
  VEDA_VERSION       Version to install (default: $DEFAULT_VERSION)
  VEDA_INSTALL_DIR   Where to put binaries (default: \$HOME/.local/bin)
EOF
                exit 0
                ;;
            *) err "unknown flag: $1 (try --help)" ;;
        esac
        shift
    done

    target=$(detect_platform)
    install_binary "veda" "$target"

    if [ "$WITH_FUSE" -eq 1 ]; then
        # Probe whether veda-fuse-$target exists on this release. Avoids
        # hardcoding which platforms ship a prebuilt fuse binary; if the CI
        # matrix changes (or a future release adds darwin fuse), the probe
        # picks it up automatically.
        fuse_url="https://github.com/$GITHUB_REPO/releases/download/$VERSION/veda-fuse-$target"
        if ! curl -fsIL -o /dev/null --max-time 10 "$fuse_url"; then
            log ""
            log "veda CLI is installed."
            log "veda-fuse-$target is not in release $VERSION."
            log "(Currently the release only ships a prebuilt veda-fuse for"
            log " x86_64-linux. darwin needs macFUSE SDK and aarch64 needs"
            log " a cross-compile sysroot, both blocked on the CI runner.)"
            log "To get veda-fuse on $target, compile from source:"
            log "  git clone https://github.com/$GITHUB_REPO.git"
            log "  cd veda && cargo build --release -p veda-fuse"
            log "  cp target/release/veda-fuse $DEST/"
            exit 0
        fi
        preflight_fuse
        install_binary "veda-fuse" "$target"
    fi

    install_skill
    print_next_steps
}

# allow sourcing for tests (set VEDA_INSTALL_TEST_MODE=1 to skip main)
if [ "${VEDA_INSTALL_TEST_MODE:-0}" != "1" ]; then
    main "$@"
fi
