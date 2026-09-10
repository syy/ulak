#!/usr/bin/env bash
#
# Checks whether a fresh server is ready for ulak.
#
# Usage (from your own machine, copying nothing to the server):
#     ssh <host> 'bash -s' < scripts/check-server.sh
#
# Or on the server itself:
#     bash check-server.sh
#
# Exit code 0 = ready, 1 = something is missing.

# No `set -e` here on purpose: some of these checks are meant to fail, and
# the point of the script is to see all of them in one run.

if [ -t 1 ]; then
  R=$'\033[31m'; G=$'\033[32m'; Y=$'\033[33m'; D=$'\033[2m'; B=$'\033[1m'; N=$'\033[0m'
else
  R=''; G=''; Y=''; D=''; B=''; N=''
fi

FAIL=0
WARN=0

# The status word is padded to 8 columns so every label starts in the same
# place; `MISSING` is the longest one and sets that width.
ok()   { printf '  %sOK      %s %-22s %s\n' "$G" "$N" "$1" "$2"; }
bad()  { printf '  %sMISSING %s %-22s %s\n' "$R" "$N" "$1" "$2"; FAIL=$((FAIL+1)); }
warn() { printf '  %sWARN    %s %-22s %s\n' "$Y" "$N" "$1" "$2"; WARN=$((WARN+1)); }
head_() { printf '\n%s%s%s\n' "$B" "$1" "$N"; }

printf '%s╔══════════════════════════════════════════════════════╗%s\n' "$B" "$N"
printf '%s║  ulak server readiness check                      ║%s\n' "$B" "$N"
printf '%s╚══════════════════════════════════════════════════════╝%s\n' "$B" "$N"

# ─── system info ─────────────────────────────────────────────────────────
head_ "SYSTEM"
if [ -r /etc/os-release ]; then
  . /etc/os-release
  printf '  %s%-22s%s %s\n' "$D" "distro" "$N" "${PRETTY_NAME:-unknown}"
fi
printf '  %s%-22s%s %s / %s\n' "$D" "kernel / arch" "$N" "$(uname -r)" "$(uname -m)"
printf '  %s%-22s%s %s\n' "$D" "user" "$N" "$(id -un) (uid $(id -u))"

# ─── required: docker ────────────────────────────────────────────────────
head_ "REQUIRED"

if command -v docker >/dev/null 2>&1; then
  ok "docker" "$(docker --version 2>&1 | head -1)"

  # Can the daemon be reached without sudo? ulak never calls sudo.
  if docker info >/dev/null 2>&1; then
    ok "docker daemon" "reachable without sudo"
  else
    if sudo -n docker info >/dev/null 2>&1; then
      bad "docker daemon" "sudo only -> run 'usermod -aG docker $(id -un)', then log in again"
    else
      bad "docker daemon" "unreachable (the service may be down: systemctl status docker)"
    fi
  fi
else
  bad "docker" "not installed"
fi

# The compose v2 plugin (no hyphen). The old docker-compose v1 is not enough.
if docker compose version >/dev/null 2>&1; then
  ok "docker compose" "$(docker compose version 2>&1 | head -1)"
elif command -v docker-compose >/dev/null 2>&1; then
  bad "docker compose" "only the OLD 'docker-compose' (v1) -> install docker-compose-plugin"
else
  bad "docker compose" "missing -> apt-get install docker-compose-plugin"
fi

# Keep the standalone preflight floor aligned with sync::parse_rsync_version
# and sync::local_rsync. Merely finding rsync used to mark old versions READY.
rsync_supported() {
  local version_pattern='^rsync[[:space:]]+version[[:space:]]+v?([0-9]+)\.([0-9]+)(\.|[[:space:]]|$)'
  [[ "$1" =~ $version_pattern ]] || return 1
  local major="${BASH_REMATCH[1]}" minor="${BASH_REMATCH[2]}"
  (( 10#$major > 3 || (10#$major == 3 && 10#$minor >= 2) ))
}

if command -v rsync >/dev/null 2>&1; then
  rsync_version=$(rsync --version 2>&1)
  rsync_status=$?
  rsync_first_line=${rsync_version%%$'\n'*}
  if [ "$rsync_status" -eq 0 ] && rsync_supported "$rsync_first_line"; then
    ok "rsync" "$rsync_first_line"
  else
    bad "rsync" "need rsync >= 3.2; found: $rsync_first_line -> install or upgrade rsync with your package manager"
  fi
else
  bad "rsync" "not installed -> install rsync >= 3.2 with your package manager"
fi

# ─── resources ───────────────────────────────────────────────────────────
head_ "RESOURCES"

if command -v nproc >/dev/null 2>&1; then
  printf '  %s%-22s%s %s cores\n' "$D" "cpu" "$N" "$(nproc)"
fi

if [ -r /proc/meminfo ]; then
  mem_kb=$(awk '/^MemTotal:/{print $2}' /proc/meminfo)
  mem_gb=$((mem_kb / 1024 / 1024))
  if [ "$mem_gb" -lt 2 ]; then
    warn "memory" "${mem_gb} GB — enough for small stacks, probably not for more"
  else
    ok "memory" "${mem_gb} GB"
  fi

  swap_kb=$(awk '/^SwapTotal:/{print $2}' /proc/meminfo)
  if [ "${swap_kb:-0}" -eq 0 ] && [ "$mem_gb" -lt 8 ]; then
    warn "swap" "none — containers die of OOM once memory fills up"
  fi
fi

# Disk: look at whichever partition docker's data directory lives on.
docker_root=$(docker info --format '{{.DockerRootDir}}' 2>/dev/null)
[ -d "${docker_root:-}" ] || docker_root=/var/lib/docker
[ -d "$docker_root" ] || docker_root=/
avail_gb=$(df -BG --output=avail "$docker_root" 2>/dev/null | tail -1 | tr -dc '0-9')
if [ -n "$avail_gb" ]; then
  if [ "$avail_gb" -lt 10 ]; then
    bad "disk ($docker_root)" "${avail_gb} GB free — too little to hold images"
  elif [ "$avail_gb" -lt 30 ]; then
    warn "disk ($docker_root)" "${avail_gb} GB free — tight for a mid-sized stack"
  else
    ok "disk ($docker_root)" "${avail_gb} GB free"
  fi
fi

# ─── optional ────────────────────────────────────────────────────────────
head_ "OPTIONAL (not needed by ulak)"

if command -v tailscale >/dev/null 2>&1; then
  ts_ip=$(tailscale ip -4 2>/dev/null | head -1)
  ok "tailscale" "${ts_ip:-installed but not connected}"
else
  printf '  %s--      %s %-22s %s\n' "$D" "$N" "tailscale" "absent (mind the ports if you reach this box on a public IP)"
fi

for c in make git; do
  if command -v "$c" >/dev/null 2>&1; then
    ok "$c" "$($c --version 2>&1 | head -1 | cut -c1-40)"
  else
    printf '  %s--      %s %-22s %s\n' "$D" "$N" "$c" "absent (only needed to build on the server itself)"
  fi
done

# ─── security ────────────────────────────────────────────────────────────
head_ "SECURITY"

sshd_out=$(sshd -T 2>/dev/null || sudo -n sshd -T 2>/dev/null)
if [ -n "$sshd_out" ]; then
  if printf '%s' "$sshd_out" | grep -qi '^passwordauthentication yes'; then
    warn "ssh password login" "ON — set 'PasswordAuthentication no' in sshd_config"
  else
    ok "ssh password login" "off"
  fi
else
  printf '  %s--      %s %-22s %s\n' "$D" "$N" "ssh config" "unreadable (needs privileges)"
fi

# Docker punching through ufw is the mistake people make most often here.
# It is not ulak's business to fix it, so only warn.
if command -v ufw >/dev/null 2>&1 && ufw status 2>/dev/null | grep -qi '^Status: active'; then
  warn "ufw + docker" "ufw is ACTIVE but docker BYPASSES it — never publish on 0.0.0.0!"
  printf '      %suse ports: [\"127.0.0.1:8080:80\"] — the ulak tunnel brings it home as localhost:8080%s\n' "$D" "$N"
fi

# Ports listening on every interface, not just loopback.
if command -v ss >/dev/null 2>&1; then
  pub=$(ss -Hltn 2>/dev/null | awk '{print $4}' | grep -E '^(0\.0\.0\.0|\[::\]|\*):' | head -5)
  if [ -n "$pub" ]; then
    warn "exposed ports" "these listen on 0.0.0.0:"
    printf '%s\n' "$pub" | sed "s/^/      ${D}/;s/$/${N}/"
  else
    ok "exposed ports" "nothing listening on 0.0.0.0"
  fi
fi

# ─── summary ─────────────────────────────────────────────────────────────
printf '\n%s──────────────────────────────────────────────────────%s\n' "$B" "$N"
if [ "$FAIL" -gt 0 ]; then
  printf '%s%s hard requirement(s) missing — ulak will not run.%s  (%s warning(s))\n' "$R" "$FAIL" "$N" "$WARN"
  printf '%sInstall: docker-ce + docker-compose-plugin from docker'"'"'s official apt repo, plus rsync%s\n' "$D" "$N"
  exit 1
fi

if [ "$WARN" -gt 0 ]; then
  printf '%sREADY%s — every hard requirement is met. (%s warning(s) — the WARN lines above say what to do)\n' "$G" "$N" "$WARN"
else
  printf '%sREADY%s — every hard requirement is met, nothing to warn about.\n' "$G" "$N"
fi
printf '\nNext step (locally):\n'
printf '  %scd <your-project> && ulak init <this-host> && ulak doctor%s\n' "$D" "$N"
exit 0
