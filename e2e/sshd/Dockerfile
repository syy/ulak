# E2e fixture: a disposable "server" that looks exactly like a real ulak
# target — sshd + rsync + docker + compose v2 — reachable over SSH as a
# NON-ROOT user in the docker group (my-server is root, so this fixture
# covers the non-root path the test plan requires).
#
# Docker-in-Docker on purpose: the daemon must share the sshd filesystem,
# otherwise bind mounts under ~/.ulak/workspaces would resolve against the
# outer host and silently break — the exact bug ulak exists to fix.
# Requires: docker run --privileged.
FROM docker:29.6.2-dind

RUN apk add --no-cache openssh-server openssh-client rsync bash \
    docker-cli-compose

RUN addgroup -S docker 2>/dev/null || true \
    && adduser -D -s /bin/bash dev \
    && addgroup dev docker \
    # "!" would lock the account and block pubkey auth; "*" allows it while
    # still forbidding password login.
    && sed -i 's/^dev:!/dev:*/' /etc/shadow

COPY sshd_config /etc/ssh/sshd_config
COPY entrypoint.sh /usr/local/bin/e2e-entrypoint.sh
RUN chmod +x /usr/local/bin/e2e-entrypoint.sh

EXPOSE 22
ENTRYPOINT ["/usr/local/bin/e2e-entrypoint.sh"]
CMD []
