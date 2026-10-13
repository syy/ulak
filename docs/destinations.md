# Changing a stack's server

A stack declaration records the server it was brought up on, and that
declaration outranks a later `host` edit for Ulak's own commands typed without
`-f` or `-p` (`doctor`, `status`, `sync`, `clean`) or with a stack name:
otherwise a config change would move a running stack's background job to a
different daemon. When the two disagree, those commands say so and keep
addressing the declared server. The commands they print reach it too: Ulak's
by the stack's name, Docker's with `env ULAK_HOST=<declared>`. `ulak docker ...`
and any command given explicit `-f`/`-p` follow the configuration as usual.

If the declared server no longer exists,
`ulak clean --forget-destination <ssh-host>` retires the declaration from this
machine. It never contacts the server and deletes nothing on it. The checkout
keeps its record of what it sent there, so `env ULAK_HOST=<ssh-host> ulak clean`
can still remove those files properly if the host ever answers again.

To use a different server for a running project, stop the original stack
explicitly before starting it on the new destination:

```bash
env ULAK_HOST=old-server ulak docker compose down
env ULAK_HOST=new-server ulak docker compose up -d
```

Repeat any Compose flags used to start the stack, such as `-f compose.dev.yaml`
or `-p my-app`. This starts the project on the new server; it does not migrate
Docker volumes or other server-owned data.
