# Synchronization

For installation and everyday commands, see the [README](../README.md).
This guide covers conflicts, deletion decisions and transfer limitations.

## Selection and transfer

For Compose projects, Ulak synchronizes the paths referenced by the server's
resolved Compose model, including sibling directories. `.gitignore` applies;
`[sync] include` adds required ignored paths, and `[sync] protect` excludes
server-owned data from uploads, downloads and deletion. Absolute paths and
paths under `~` in Compose belong to the server.

Small changes travel over a persistent SSH connection. Larger files and new
directories use rsync. On Linux servers with Python 3.9 or later, server changes
are watched; otherwise they return on the next full comparison, about once a
minute. Restarts, reconnects and missed events also trigger a full comparison.

## Conflicts and missing history

Ulak does not merge or create conflict files. If both machines changed a file
differently since their last agreed content, Ulak keeps both copies, warns
about the conflict and skips the file until the copies match.

To resolve a conflict:

1. Find the warning in `ulak sync` output or in `service/service.log` under
   Ulak's state directory (`$XDG_STATE_HOME/ulak`, by default `~/.local/state/ulak`).
2. Find the server copy at the path named in the warning, inside the remote
   workspace shown by `ulak status`.
3. Make both copies contain the result you want, then run `ulak sync`.

Without agreed content on record for that server, uploads follow
`[sync] no_history`. This can happen after the state directory was wiped, on a
fresh CI runner, or on a machine without that server's history:

- `send` (default) replaces the server copy with the local copy and warns for
  each replaced file.
- `hold` keeps both copies and reports a conflict.

Deletion follows separate rules; see [Deletion safety](#deletion-safety).

## Deletion safety

Ulak treats deletion as a separate operation from synchronization.

- It never runs `rsync --delete`.
- It never deletes a local file during a pull.
- A remote file is eligible for deletion only if Ulak recorded that it sent
  the file and the file is now absent locally. If the server's copy differs
  from the version both machines last agreed on, the server's edit wins: the
  deletion is not applied, the server's copy comes back to your checkout, and
  a warning names the file.
- With no agreed version on record for that file, deletion follows the record
  of the send alone, even if the server changed the file. The exception is a
  send that Ulak can tell never reached the server: the server's file then stays.
- If the server changed the structure of that file's path, so that a directory
  on the way became a symlink or a file, or the file itself is no longer a
  regular file or symlink, Ulak does not delete through it. It leaves the file
  on the server, stops tracking it and names it in a warning.
- If the server's copy cannot be read right now, because something is writing
  it or the SSH user may not read it, Ulak keeps it this time, names it with
  the reason and tries again on the next sync. If the server owns that file,
  add it to `[sync] protect`.
- Either way, every other deletion goes ahead and the sync continues.
- A pull does not overwrite a local change that has not reached the server.
- Ulak does not delete an unclaimed server-created file merely because no local
  copy exists.
- Protected paths never enter the deletion ledger, and a path protected after
  Ulak sent it is never deleted.
- More than 25 remote deletions require confirmation by default. Change the
  limit with `sync.max_delete` or `--max-delete`.

Each client and checkout gets its own remote workspace. Ulak locks the
workspace during changes and refuses if two clients claim the same workspace.
SSH host-key checking is never disabled.

## Previewing a sync

`ulak sync --dry-run` previews uploads and remote deletions without applying
them. It does not preview downloads.

The upload list comes from rsync's size-and-time check; the real sync compares
contents, so a file listed for upload may be held as a conflict. A locally
deleted file that changed on the server is listed separately: the real sync
keeps the server copy and brings it back.

Resolving the Compose model still requires a server connection and may upload
temporary Compose and environment files.

## Transfer and cache limits

- A file that goes through rsync is compared before the transfer, not as it
  is written. A program on the server that rewrites that file in between can
  lose its write. A batch can fall back to rsync because it includes a large
  file, a new directory or too many files, or because the persistent SSH
  connection is unavailable. Do not edit locally the files a running container
  rewrites.
- Ulak caches file digests on both machines and trusts an entry only while
  the file's device, inode, size, mode and timestamps are unchanged. That is
  stricter than rsync's quick check, which compares size and modification
  time only. On a network filesystem whose clock runs behind the machine that
  hashes its files, an entry can be trusted too early and miss a change.
  Either cache is safe to delete and costs one full read on the next sync.
  Here it is `workspaces/<id>/fingerprints/` in the state directory. On the
  server it is `.baseline/` beside the `workspace` directory that
  `ulak status` shows, whose parent directory is named `<id>`.
