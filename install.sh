#!/bin/sh
# install.sh — Veda CLI installer
#
# Usage:
#   curl -fL <install-url> | sh                       # install latest (auto-resolved)
#   curl -fL ... | sh -s -- --with-fuse               # also install veda-fuse
#
# Env overrides:
#   VEDA_VERSION       Pin a specific version (default: auto — fetched from source)
#   VEDA_INSTALL_DIR   Where to put binaries (default: $HOME/.local/bin)
#   VEDA_SOURCE        gitlab (default) | github — pick artifact host

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

DEFAULT_SERVER="https://veda.dbpaas.dingdongxiaoqu.com"

# ====== state ======
WITH_FUSE=0
SOURCE="${VEDA_SOURCE:-gitlab}"
VERSION="${VEDA_VERSION:-}"   # empty → fetch_latest_version() resolves it
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

# Resolve the latest non-prerelease version from the chosen source.
# Output goes to stdout (a single version string); failures call err()
# and exit, so callers can capture without worrying about empty values.
#
# Why each source has its own path:
#   - GitLab: deploy-token's read_package_registry scope cannot
#     enumerate (no /repository/tags, no /packages list). CI uploads
#     a `latest/LATEST_VERSION` text file in publish:all (.gitlab-ci.yml)
#     which we fetch as a single artifact.
#   - GitHub: public `/releases/latest` returns the most recent
#     non-prerelease tag. Unauthed; rate-limited to 60 req/h per IP
#     which is fine for installs.
fetch_latest_version() {
    case "$SOURCE" in
        gitlab)
            url="https://${GITLAB_HOST}/api/v4/projects/${GITLAB_PROJECT_ID}/packages/generic/veda/latest/LATEST_VERSION"
            body=$(curl_with_auth --fail --silent --location "$url" 2>/dev/null) \
                || err "fetch failed: $url
       (latest pointer not yet published, or token expired)
       Workaround: VEDA_VERSION=<x.y.z> curl … | sh"
            # Strip whitespace — the upload could end with a trailing
            # newline depending on how the CI step printed it.
            v=$(printf '%s' "$body" | tr -d '[:space:]')
            ;;
        github)
            url="https://api.github.com/repos/${GITHUB_REPO}/releases/latest"
            body=$(curl --fail --silent --location "$url" 2>/dev/null) \
                || err "fetch failed: $url (network? rate-limited?)"
            # Pull "tag_name":"x.y.z" from the JSON without jq.
            v=$(printf '%s' "$body" \
                | grep -m1 '"tag_name"' \
                | sed -E 's/.*"tag_name"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/')
            ;;
        *)
            err "unknown VEDA_SOURCE: $SOURCE (expected gitlab or github)"
            ;;
    esac
    if [ -z "$v" ]; then
        err "fetched version from $SOURCE but parsed empty value (server returned unexpected format?)"
    fi
    printf '%s' "$v"
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
                    # RHEL-family forks common on internal Chinese
                    # clouds: HCE = Huawei Cloud EulerOS, openEuler =
                    # Huawei upstream, Kylin = Galaxylink/麒麟 V10,
                    # Anolis = Alibaba Cloud Linux, TencentOS = TencentOS
                    # Server. All ship `fuse3` (provides libfuse3.so.3)
                    # in the default yum repo. yum is invoked rather
                    # than dnf because HCE 2.x and Kylin V10 still
                    # default to yum.
                    hce|openeuler|kylin|anolis|tencentos)
                        cmd="sudo yum install -y fuse3" ;;
                    arch|manjaro)
                        cmd="sudo pacman -S --noconfirm fuse3" ;;
                    *)
                        log ""
                        log "veda CLI is installed."
                        log "veda-fuse needs the libfuse3 runtime shared library, but distro"
                        log "'$distro_id' has no auto-install recipe in this script."
                        log "Install it manually (package name varies):"
                        log "  RHEL family:  sudo yum install -y fuse3"
                        log "  Debian/Ubuntu: sudo apt-get install -y libfuse3-3 fuse3"
                        log "Then re-run install.sh --with-fuse."
                        exit 0
                        ;;
                esac
                log ""
                log "veda CLI is installed."
                log "veda-fuse needs libfuse3. Recommended: $cmd"
                # `curl | sh` makes stdin the script body itself —
                # `read` there consumes a script line instead of user
                # input, which silently runs the next branch with a
                # garbage answer. Route the prompt + read through
                # /dev/tty.
                #
                # `[ -r /dev/tty ]` is a false-positive trap: it
                # checks device-node readability (the device file
                # always exists with read perms on the user), not
                # whether *this* process has a controlling terminal.
                # In the no-tty case (systemd, container without
                # `-T`, detached daemon) the open still fails with
                # ENXIO. Probe by actually trying to open in a
                # subshell — if that fd-3 dance succeeds, the real
                # open below will too; the subshell-scoped fd is
                # discarded so we don't leak an extra descriptor.
                # `2>/dev/null` on the subshell swallows the
                # `Device not configured` message the shell would
                # otherwise print to stderr.
                if (exec 3</dev/tty) 2>/dev/null; then
                    printf "Run it now? [y/N] " >/dev/tty
                    read -r ans </dev/tty
                else
                    log "Non-interactive shell (no controlling tty): skipping FUSE deps install."
                    log "Run '$cmd' manually if you want veda-fuse."
                    exit 0
                fi
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
    if [ ! -d "$HOME/.claude" ]; then
        return
    fi
    mkdir -p "$HOME/.claude/skills/veda"
    # Primary skill — always install (mirrors `skill.md` from the
    # repo as `SKILL.md` so Claude Code picks it up by the
    # capitalised convention the loader expects).
    url=$(asset_url "skill.md")
    if curl_with_auth --fail --silent --location \
            "$url" -o "$HOME/.claude/skills/veda/SKILL.md"; then
        log "skill installed → ~/.claude/skills/veda/SKILL.md"
    else
        warn "could not fetch skill.md; skipping (CLI is fine)"
        return
    fi
    # FUSE companion — only useful when veda-fuse is also installed.
    # Best-effort: missing artifact is fine (older releases predate
    # the split); the main skill.md links to it but degrades cleanly.
    if [ "$WITH_FUSE" -eq 1 ]; then
        url=$(asset_url "skill-fuse.md")
        if curl_with_auth --fail --silent --location \
                "$url" -o "$HOME/.claude/skills/veda/skill-fuse.md"; then
            log "fuse skill installed → ~/.claude/skills/veda/skill-fuse.md"
        else
            warn "could not fetch skill-fuse.md (older release?); CLI is still fine"
        fi
    fi
}

# Write the server URL into the freshly-installed binary's config so the
# user (or agent) doesn't have to `veda config set` as step zero. Uses
# the absolute path because `curl | sh` runs in a shell whose PATH has
# not been re-evaluated since we dropped the binary into $DEST.
#
# Skip when the user has already pointed `server_url` at something
# non-default — re-running install.sh to upgrade the CLI shouldn't
# clobber a custom server URL. `http://localhost:3000` is the
# CliConfig::default and we treat it as "unset"; anything else,
# including DEFAULT_SERVER itself, is the user's choice.
configure_server_url() {
    cfg_path="${XDG_CONFIG_HOME:-$HOME/.config}/veda/config.toml"
    if [ -r "$cfg_path" ]; then
        existing_url=$(grep -E '^server_url' "$cfg_path" 2>/dev/null \
            | sed -E 's/^server_url[[:space:]]*=[[:space:]]*"([^"]*)".*/\1/' \
            | head -1)
        case "$existing_url" in
            ""|http://localhost:3000) ;;
            *)
                log "✓ server URL kept as $existing_url (config already set)"
                return
                ;;
        esac
    fi
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
    log ""
    log "Next: run 'veda init' — zero-input anonymous onboard, instantly usable."
    log "      Add --email for a named account, or upgrade later via 'veda init --upgrade --email …'."
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
    # PATH check goes LAST and gets banner treatment so the user can't
    # miss it. Many distros (HCE / RHEL family) ship a root profile
    # that doesn't include ~/.local/bin, leaving a fresh install
    # invisible to the shell — the first `veda init` then fails with
    # "command not found" and the user blames the installer, not PATH.
    # Banner uses ASCII box drawing so it's visible in any terminal.
    # ${PATH:-} guards against `set -u` crashing if PATH is somehow
    # unset (theoretical — even sh init scripts set PATH — but
    # cheap insurance).
    case ":${PATH:-}:" in
        *":$DEST:"*) ;;
        *)
            log ""
            log "┌───────────────────────────────────────────────────────────────┐"
            log "│  ⚠ ACTION NEEDED — $DEST is not in your PATH"
            log "│"
            log "│  Run this in your current shell so 'veda' resolves now:"
            log "│"
            log "│      export PATH=\"$DEST:\$PATH\""
            log "│"
            log "│  And append the same line to ~/.bashrc (or ~/.zshrc) so it"
            log "│  persists across sessions. Without this, 'veda init' below"
            log "│  will fail with 'command not found'."
            log "└───────────────────────────────────────────────────────────────┘"
            ;;
    esac
}

main() {
    while [ $# -gt 0 ]; do
        case "$1" in
            --with-fuse)   WITH_FUSE=1 ;;
            # --source / --from-* take precedence over the VEDA_SOURCE
            # env var. PR2 removed the user-visible flag in favor of
            # env-only, but the `VEDA_SOURCE=… curl … | sh` footgun
            # (env doesn't cross the pipe to sh) bit alpha testers
            # immediately. Bringing the flag back as the primary lever
            # while keeping env as a fallback covers both invocation
            # styles cleanly. `--from-github` / `--from-gitlab` are
            # kept as aliases for muscle memory.
            --source)
                shift
                [ $# -gt 0 ] || err "--source needs a value (github|gitlab)"
                [ -n "$1" ] || err "--source value cannot be empty"
                SOURCE="$1" ;;
            --source=*)
                SOURCE="${1#--source=}"
                [ -n "$SOURCE" ] || err "--source= value cannot be empty (use --source github|gitlab)"
                ;;
            --from-github) SOURCE=github ;;
            --from-gitlab) SOURCE=gitlab ;;
            -h|--help)
                cat >&2 <<EOF
Usage: install.sh [--with-fuse] [--source github|gitlab]

Source defaults to gitlab (internal $GITLAB_HOST). Pass --source github
to install from public GitHub releases, or set VEDA_SOURCE=github (the
env var is only honored when the flag is absent, and remember it does
not cross a \`curl … | sh\` pipe — prefer the flag).

The script auto-resolves the latest version from the chosen source.

Supported platforms: macOS Intel, macOS Apple Silicon, Linux x86_64.

Env overrides (lower priority than the matching CLI flag):
  VEDA_VERSION       Pin a specific version (default: auto-resolved)
  VEDA_INSTALL_DIR   Where to put binaries (default: \$HOME/.local/bin)
  VEDA_SOURCE        gitlab (default) | github

Examples:
  install.sh --with-fuse                         # default gitlab stream
  install.sh --source github --with-fuse         # public GitHub
  curl -fL <url> | sh -s -- --source github      # curl-pipe with flag
EOF
                exit 0
                ;;
            *) err "unknown flag: $1 (try --help)" ;;
        esac
        shift
    done

    case "$SOURCE" in
        gitlab|github) ;;
        *) err "source must be gitlab or github (got '$SOURCE'); pick via --source <name> or VEDA_SOURCE env" ;;
    esac

    if [ -z "$VERSION" ]; then
        log "resolving latest version from $SOURCE..."
        VERSION=$(fetch_latest_version)
        log "→ $VERSION"
    fi

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
