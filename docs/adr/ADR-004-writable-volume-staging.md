# ADR-004: Writable Finder volume via local write-back staging

- Status: accepted (implementation v0.4)
- Date: 2026-08-26

## Context

MTP objects cannot be mutated in place: the phone owns its storage, the
protocol offers create/delete/upload-stream/partial-read only. A writable
Finder volume therefore needs a translation layer that absorbs POSIX writes
locally and replays them as MTP operations.

The original simple-mtpfs solved this with /tmp copies pushed back on
`release()`. Its weaknesses were lost data on push failure and full-file
copies on every open even for read-only use.

## Decision

**Stage per open-for-write file, flush on COMMIT.**

1. `write(id, off, data)` → ensure a local staging copy exists
   (pull current object once through ranged `GetPartialObject` reads),
   apply `pwrite` locally, mark dirty. Reply UNSTABLE.
2. Kernel sends `COMMIT` when the file closes → `commit(id)` flushes:
   `hdelete(old)` + `hupload(new)` + remember the new device handle in
   `flushed_dev`, so the kernel's existing filehandle keeps working.
3. `setattr(size)` truncates inside the stage; `read`/`getattr`/`readdir`
   serve staged state so clients see their own writes immediately.
4. `create`/`create_exclusive` register *virtual* ids (bit 62) that gain a
   real device handle after the first flush; `remove` of an unflushed file
   just discards its stage; `rename` of an unflushed file is pure metadata.

Guard-rails:

- `--read-only` forces `Capabilities::ReadOnly`; default follows device
  storage writability.
- Staged temp files live under `/tmp/pereprava-nfs-<pid>/` and are removed
  on flush/remove; process exit leaves nothing behind but tmp files.
- Nested creation inside an unflushed directory returns NFS3ERR_INVAL
  (documented limitation until directory staging exists).

## Consequences

- Writes become "close-to-sync": data lands on the phone at close, matching
  how people use file managers.
- A crash between delete-old and upload-new loses the old copy — same
  trade-off as simple-mtpfs; mitigated later by upload-first+rename once we
  adopt server-side rename of freshly uploaded objects.
- Memory bounded by disk staging, not RAM.
