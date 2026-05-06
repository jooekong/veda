#!/bin/sh
# install.sh — Veda CLI installer
#
# Usage:
#   curl -fL https://git.ddxq.mobi/middleware/dbpaas/veda/-/raw/main/install.sh | sh
#   curl -fL ... | sh -s -- --with-fuse        # also install veda-fuse
#
# Env overrides:
#   VEDA_VERSION       Version to install (default below)
#   VEDA_INSTALL_DIR   Where to put binaries (default: $HOME/.local/bin)

set -eu

# ====== config (filled in by maintainer) ======
DEPLOY_TOKEN="aYm_fPpSwfdi7psCAGdQ"        # GitLab deploy token, scope=read_package_registry
PROJECT_ID="9462"
GITLAB_BASE="https://git.ddxq.mobi"
DEFAULT_VERSION="0.1.0"
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
    url="$GITLAB_BASE/api/v4/projects/$PROJECT_ID/packages/generic/veda/$VERSION/$name"
    curl --fail --show-error --silent --location \
         --header "Deploy-Token: $DEPLOY_TOKEN" \
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
        url="$GITLAB_BASE/api/v4/projects/$PROJECT_ID/packages/generic/veda/$VERSION/skill.md"
        if curl --fail --silent --location \
                --header "Deploy-Token: $DEPLOY_TOKEN" \
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
    log "Next steps:"
    log "  1. veda config set server_url $DEFAULT_SERVER"
    log "  2. veda account create --name <you> --email <email> --password <password>"
    log "  3. veda config set api_key <key from above>"
    log "  4. veda workspace create my-workspace && veda use my-workspace"
    log ""
    if [ -d "$HOME/.claude" ]; then
        log "Claude Code: skill auto-installed; just ask Claude to use veda."
    else
        log "Other agents: have them read $GITLAB_BASE/middleware/dbpaas/veda/-/raw/main/skill.md"
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

    [ "$DEPLOY_TOKEN" = "REPLACE_ME_DEPLOY_TOKEN" ] \
        && err "DEPLOY_TOKEN not configured in install.sh"

    target=$(detect_platform)
    install_binary "veda" "$target"

    if [ "$WITH_FUSE" -eq 1 ]; then
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
