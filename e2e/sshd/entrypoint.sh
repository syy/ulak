#!/bin/sh
# Start sshd, then hand over to the stock dind entrypoint (dockerd).
set -e

# Restart-proof: stale pid files from a previous run block both daemons.
rm -f /var/run/docker.pid /run/docker.pid /var/run/sshd.pid /run/sshd.pid

ssh-keygen -A
mkdir -p /home/dev/.ssh
chown dev:dev /home/dev/.ssh
chmod 700 /home/dev/.ssh
/usr/sbin/sshd

# dockerd-entrypoint.sh comes from the dind base image. DOCKER_TLS_CERTDIR=""
# (set by the harness) skips TLS cert generation — we only need the unix
# socket, which lands root:docker so user `dev` can use it.
exec dockerd-entrypoint.sh "$@"
