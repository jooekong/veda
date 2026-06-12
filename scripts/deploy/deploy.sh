#!/usr/bin/env bash
# Deploy veda-server to one or more hosts, sequentially.
#
# Per host: push binary -> keep a .bak of the current one -> swap (atomic
# rename) -> systemctl restart -> poll /v1/ready until 200 -> next host.
#
# Swap MUST happen before any stop/restart: the socket unit stays active the
# whole time, and socket activation starts the service the moment a queued
# connection arrives while it is inactive — if the binary were swapped after
# the stop, systemd would silently boot the OLD binary and the subsequent
# start would no-op, passing the ready gate on the wrong version.
#
# Smoothness comes from socket activation (veda-server.socket): the listening
# socket stays open in systemd across the restart window, so new connections
# queue in the kernel backlog (~2-4s added latency) instead of being refused.
# No LB coordination needed. With a future multi-node + ELB setup, set
# drain_secs in the server config and this same script does a rolling deploy.
#
# Prereqs per host: veda-server.socket + veda-server.service installed and
# enabled (see templates in this directory), binary at $REMOTE_BIN.
#
# Usage:
#   deploy.sh <path-to-binary> <host1> [host2 ...]
# Env overrides:
#   SSH_USER (root) | REMOTE_BIN (/data/veda/bin/veda-server)
#   PORT (3000)     | READY_TIMEOUT_SECS (90) | OBSERVE_SECS (15)

set -euo pipefail

SSH_USER="${SSH_USER:-root}"
REMOTE_BIN="${REMOTE_BIN:-/data/veda/bin/veda-server}"
PORT="${PORT:-3000}"
READY_TIMEOUT_SECS="${READY_TIMEOUT_SECS:-90}"
OBSERVE_SECS="${OBSERVE_SECS:-15}"

if [[ $# -lt 2 ]]; then
    echo "usage: $0 <path-to-binary> <host1> [host2 ...]" >&2
    exit 1
fi

BINARY="$1"
shift
HOSTS=("$@")

[[ -f "$BINARY" ]] || { echo "binary not found: $BINARY" >&2; exit 1; }

wait_ready() {
    local host="$1" deadline=$((SECONDS + READY_TIMEOUT_SECS))
    until ssh "${SSH_USER}@${host}" "curl -fsS -o /dev/null http://127.0.0.1:${PORT}/v1/ready" 2>/dev/null; do
        if (( SECONDS >= deadline )); then
            return 1
        fi
        sleep 2
    done
}

for i in "${!HOSTS[@]}"; do
    host="${HOSTS[$i]}"
    echo "==> [$((i + 1))/${#HOSTS[@]}] ${host}"

    echo "  - push binary"
    scp -q "$BINARY" "${SSH_USER}@${host}:${REMOTE_BIN}.new"

    # Swap first (atomic rename; the running process keeps its old inode,
    # so no ETXTBSY), THEN restart — see header for why this order matters.
    # The socket stays open (held by systemd): connections queue from the
    # graceful stop until the new process accepts.
    echo "  - swap binary (old kept at .bak) + restart"
    ssh "${SSH_USER}@${host}" "chmod 755 ${REMOTE_BIN}.new && { [ ! -f ${REMOTE_BIN} ] || cp -p ${REMOTE_BIN} ${REMOTE_BIN}.bak; } && mv -f ${REMOTE_BIN}.new ${REMOTE_BIN} && systemctl restart veda-server"

    echo "  - wait /v1/ready (timeout ${READY_TIMEOUT_SECS}s)"
    if ! wait_ready "$host"; then
        echo "!! ${host} did not become ready — deploy halted, remaining hosts untouched: ${HOSTS[*]:$((i + 1))}" >&2
        echo "!! roll back with:" >&2
        echo "!!   ssh ${SSH_USER}@${host} 'mv -f ${REMOTE_BIN}.bak ${REMOTE_BIN} && systemctl restart veda-server'" >&2
        exit 1
    fi

    if (( i + 1 < ${#HOSTS[@]} )); then
        echo "  - ready; observing ${OBSERVE_SECS}s before next host (watch error rate / p99)"
        sleep "$OBSERVE_SECS"
    else
        echo "  - ready"
    fi
done

echo "==> all ${#HOSTS[@]} hosts deployed"
