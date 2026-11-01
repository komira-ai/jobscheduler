# Files the previous release wrote

`written_by_previous_release.toml` and `rewritten_by_previous_release.toml` were written by
v0.1.120's own `js-lease`, from a checkout of the tag (`git worktree add <dir> v0.1.120`):

- `written_by_previous_release.toml`: `scenario.rs` included in a test of that checkout's
  `crates/js-lease/tests/lease` and run there (`whole_hosts_written`).
- `rewritten_by_previous_release.toml`: `regenerate_previous.rs.txt` added as a module of that
  checkout's `crates/js-lease/tests/lease` (with `OUT/` set to a directory holding
  `written_by_this_release.toml`) and run with
  `cargo test -p js-lease --test lease gen_rewritten`. It restarts that release over this
  release's file as its server does (`read_bookings`, `HostBook::seat`, `LeaseTable::open`).

`written_by_this_release.toml` is what this release writes; `cross_release.rs` pins it, so
a change to it fails there and the two files above must be made again from it.
`previous_store.rs` is lines 24 to 126 of v0.1.120's `crates/js-lease/src/store.rs`.
