# Bodies of the previous release

The literal JSON the agent wire of release `v0.1.135` sends, as the golden tests of that
tag (`crates/js-agent-wire/tests/golden.rs` at `v0.1.135`) assert it: a hello, a heartbeat
that carries a held run, a result and a taint, and a reply that carries a start, a stop, a
cancel, a result acknowledgement and a taint acknowledgement. They are files, not values
built from the current types, so a change to those types cannot move them.
`tests/previous_release.rs` decodes them with the current types, and decodes the current
types' bodies with a copy of that release's structs.
