# Files the previous release wrote

`written_by_previous_release.toml` and `rewritten_by_previous_release.toml` were written by
v0.1.120's own `js-lease`, from a checkout of the tag (`git worktree add <dir> v0.1.120`):

- `written_by_previous_release.toml`: the steps of `scenario.rs` included in a test of that
  checkout's `crates/js-lease/tests/lease` and run there (`whole_hosts_written`), with the
  second acquire given a 30 s time to live and renewed ten seconds in, which that release
  had and this one does not. `cross_release.rs` compares this release's output of the
  scenario with it but for each lease's `expires_at_ms`.
- `rewritten_by_previous_release.toml`: `regenerate_previous.rs.txt` added as a module of that
  checkout's `crates/js-lease/tests/lease` (with `OUT/` set to a directory holding
  `written_by_this_release.toml`) and run with
  `cargo test -p js-lease --test lease gen_rewritten`. It restarts that release over this
  release's file as its server does (`read_bookings`, `HostBook::seat`, `LeaseTable::open`).

The `expires_at_ms` rows of `rewritten_by_previous_release.toml` were moved to `4600000`, the
`ends_at_ms` of every lease, by hand, because `written_by_this_release.toml` now writes them
so; the previous reader's own rewrite of the file (`previous::write`) is still checked
against it byte for byte in `cross_release.rs`.

`written_by_this_release.toml` is what this release writes; `cross_release.rs` pins it, so
a change to it fails there and the two files above must be made again from it.
`previous_store.rs` is lines 24 to 126 of v0.1.120's `crates/js-lease/src/store.rs`.
