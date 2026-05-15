#!/bin/sh
# install.sh — Veda CLI installer
#
# Usage:
#   curl -fL <install-url> | sh                       # default: install from internal GitLab
#   curl -fL ... | sh -s -- --with-fuse               # also install veda-fuse
#   curl -fL ... | sh -s -- --from-github             # install from public GitHub releases
#
# Env overrides:
#   VEDA_VERSION       Version to install
#   VEDA_INSTALL_DIR   Where to put binaries (default: $HOME/.local/bin)
#   VEDA_SOURCE        gitlab (default) | github

set -eu

# ====== config ======
GITHUB_REPO="jooekong/veda"
GITLAB_HOST="git.ddxq.mobi"
GITLAB_PROJECT_PATH="middleware/dbpaas/veda"
GITLAB_PROJECT_ID="9462"
# Read-only deploy token, scope=read_package_registry. Lets the script
# pull release artifacts without prompting the user for credentials.
# The token can only GET this project's packages; the GitLab instance
# is internal-only, so the blast radius if this script leaks is bounded
# to "someone on the internal network can fetch binaries they could
# already read by signing in to GitLab."
GITLAB_DEPLOY_TOKEN="j4s2baP6aEybzSrsxq76"

DEFAULT_VERSION_GITLAB="0.0.12-test"
DEFAULT_VERSION_GITHUB="0.1.6"
DEFAULT_SERVER="https://veda.dbpaas.dingdongxiaoqu.com"

# ====== state ======
WITH_FUSE=0
SOURCE="${VEDA_SOURCE:-gitlab}"
VERSION="${VEDA_VERSION:-}"
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
        *) err "unsupported platform: $os-$arch (supported: macOS Intel/Apple Silicon, Linux x86_64)" ;;
    esac
}

asset_url() {
    name="$1"
    case "$SOURCE" in
        gitlab)
            printf 'https://%s/api/v4/projects/%s/packages/generic/veda/%s/%s' \
                "$GITLAB_HOST" "$GITLAB_PROJECT_ID" "$VERSION" "$name"
            ;;
        github)
            printf 'https://github.com/%s/releases/download/%s/%s' \
                "$GITHUB_REPO" "$VERSION" "$name"
            ;;
        *)
            err "unknown VEDA_SOURCE: $SOURCE (expected gitlab or github)"
            ;;
    esac
}

curl_with_auth() {
    if [ "$SOURCE" = "gitlab" ]; then
        curl --header "Deploy-Token: $GITLAB_DEPLOY_TOKEN" "$@"
    else
        curl "$@"
    fi
}

download_asset() {
    name="$1"
    url=$(asset_url "$name")
    curl_with_auth --fail --show-error --silent --location \
         "$url" -o "$TMP/$name" \
        || err "download failed: $name (check version $VERSION exists on $SOURCE)"
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
        url=$(asset_url "skill.md")
        if curl_with_auth --fail --silent --location \
                "$url" -o "$HOME/.claude/skills/veda/SKILL.md"; then
            log "skill installed → ~/.claude/skills/veda/SKILL.md"
        else
            warn "could not fetch skill.md; skipping (CLI is fine)"
        fi
    fi
}

# Write the server URL into the freshly-installed binary's config so the
# user (or agent) doesn't have to `veda config set` as step zero. Uses
# the absolute path because `curl | sh` runs in a shell whose PATH has
# not been re-evaluated since we dropped the binary into $DEST.
configure_server_url() {
    if "$DEST/veda" config set server_url "$DEFAULT_SERVER" >/dev/null 2>&1; then
        log "✓ server URL set to $DEFAULT_SERVER"
    else
        warn "could not write server_url to config; run 'veda config set server_url $DEFAULT_SERVER' manually"
    fi
}

print_next_steps() {
    log ""
    log "✓ veda is installed at $DEST/veda"
    configure_server_url
    case ":$PATH:" in
        *":$DEST:"*) ;;
        *)
            log ""
            log "⚠ $DEST is not in your PATH. Add this to your shell rc:"
            log "    export PATH=\"$DEST:\$PATH\""
            ;;
    esac
    log ""
    log "Next: run 'veda init' — zero-input anonymous onboard, instantly usable."
    log "      Add --email for a named account, or upgrade later via 'veda claim'."
    log ""
    if [ -d "$HOME/.claude" ]; then
        log "Claude Code: skill auto-installed; just ask Claude to use veda."
    else
        case "$SOURCE" in
            gitlab) skill_doc="http://$GITLAB_HOST/$GITLAB_PROJECT_PATH/-/raw/main/skill.md" ;;
            github) skill_doc="https://raw.githubusercontent.com/$GITHUB_REPO/main/skill.md" ;;
        esac
        log "Other agents: have them read $skill_doc"
    fi
}

main() {
    while [ $# -gt 0 ]; do
        case "$1" in
            --with-fuse)   WITH_FUSE=1 ;;
            --from-github) SOURCE=github ;;
            --from-gitlab) SOURCE=gitlab ;;
            -h|--help)
                cat >&2 <<EOF
Usage: install.sh [--with-fuse] [--from-github|--from-gitlab]

Default source: gitlab (internal $GITLAB_HOST). Use --from-github to
install from public GitHub releases.

Supported platforms: macOS Intel, macOS Apple Silicon, Linux x86_64.

Env overrides:
  VEDA_VERSION       Version to install
                     (default: $DEFAULT_VERSION_GITLAB on gitlab, $DEFAULT_VERSION_GITHUB on github)
  VEDA_INSTALL_DIR   Where to put binaries (default: \$HOME/.local/bin)
  VEDA_SOURCE        gitlab (default) | github
EOF
                exit 0
                ;;
            *) err "unknown flag: $1 (try --help)" ;;
        esac
        shift
    done

    case "$SOURCE" in
        gitlab) : "${VERSION:=$DEFAULT_VERSION_GITLAB}" ;;
        github) : "${VERSION:=$DEFAULT_VERSION_GITHUB}" ;;
        *) err "VEDA_SOURCE must be gitlab or github, got: $SOURCE" ;;
    esac

    target=$(detect_platform)
    install_binary "veda" "$target"

    if [ "$WITH_FUSE" -eq 1 ]; then
        # Probe whether veda-fuse-$target exists on this release. Avoids
        # hardcoding which platforms ship a prebuilt fuse binary; if the CI
        # matrix changes (or a future release adds darwin-aarch64 fuse), the
        # probe picks it up automatically.
        fuse_url=$(asset_url "veda-fuse-$target")
        if ! curl_with_auth -fsIL -o /dev/null --max-time 10 "$fuse_url"; then
            log ""
            log "veda CLI is installed."
            log "veda-fuse-$target is not in release $VERSION on $SOURCE."
            log "(Currently only x86_64-linux and x86_64-darwin ship a prebuilt"
            log " veda-fuse. aarch64-darwin needs a cross-compile sysroot for"
            log " macFUSE which we haven't validated on CI.)"
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
