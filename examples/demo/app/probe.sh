#!/bin/sh
# Reports whether each local-reference mechanism really exists inside
# the container. No guessing, only evidence.

show() {
  label="$1"
  path="$2"
  if [ -f "$path" ]; then
    printf '  %-28s OK    %s\n' "$label" "$(cat "$path")"
  elif [ -d "$path" ]; then
    printf '  %-28s BROKEN mounted as a directory, not a file (%s)\n' "$label" "$path"
  else
    parent=$(dirname "$path")
    if [ -d "$parent" ]; then
      printf '  %-28s BROKEN %s exists but is EMPTY -> the daemon created an empty directory\n' "$label" "$parent"
    else
      printf '  %-28s MISSING   %s\n' "$label" "$path"
    fi
  fi
}

echo "===================================================================="
echo " ulak-workspace probe   |  container host: $(hostname)  |  arch: $(uname -m)"
echo "===================================================================="

if [ -n "$GREETING" ]; then
  printf '  %-28s OK    %s\n' "1. env_file (./.env)" "$GREETING"
else
  printf '  %-28s BROKEN GREETING variable is empty\n' "1. env_file (./.env)"
fi

show "2. build context (./app)"   /baked/marker.txt
show "3. bind mount (./bind)"     /mnt/bind/marker.txt
show "4. configs file (./configs)" /mnt/config/app.conf

# 5. Writable mount: appends one line per run. Ulak brings this file
#    back to the checkout unless the mount is protected or ignored.
echo "--------------------------------------------------------------------"
if [ -d /data ]; then
  echo "run $(date -u +%H:%M:%S)" >> /data/runs.log 2>/dev/null || true
  printf '  %-28s %s lines\n' "5. writable (./data)" "$(wc -l < /data/runs.log 2>/dev/null || echo 0)"
  sed 's/^/       /' /data/runs.log 2>/dev/null || true
else
  printf '  %-28s MISSING\n' "5. writable (./data)"
fi

echo "===================================================================="

# Report once, then stay up. A running portless service is a shape the
# stack needs on purpose: it keeps the stack alive while the
# port-publishing service is stopped, which is exactly the state in
# which a tunnel is open with no container behind it — the third tunnel
# state `ulak status` reports, and the one the e2e suite stops `web` to
# produce. (busybox sleep knows no "infinity"; i32::MAX seconds is 68
# years, which is the same statement.)
exec sleep 2147483647
