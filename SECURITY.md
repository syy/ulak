# Security policy

## Reporting a vulnerability

Report privately through GitHub's
[Report a vulnerability](https://github.com/syy/ulak/security/advisories/new)
form. Do not open a public issue.

Include `ulak --version`, the server's Docker and rsync versions, and the
shortest command sequence that reproduces the problem.

You will get a first reply within a week. Only the latest release is supported;
a fix ships in the next version, and disclosure is coordinated with you before
it does.

## Scope

Ulak builds shell commands for a remote host and moves files between two
machines. These count as vulnerabilities:

- a path or flag value that reaches a remote shell as syntax instead of as an
  argument;
- a deletion Ulak did not record sending, a deletion of a `[sync] protect`
  path, or a deletion during a pull;
- a secret that reaches the audit trail, a log line, an error message, or the
  remote process argv;
- a command that reaches the wrong machine's filesystem;
- one checkout or client reading or retiring another's workspace or state;
- anything that weakens SSH host-key checking.

These do not:

- a server or SSH account with more privilege than you meant to grant;
- Docker's own behaviour, such as published ports bypassing a host firewall;
- last-write-wins when the same file changes on both machines;
- an attacker who already has write access to your checkout or to
  `~/.local/state/ulak`.
