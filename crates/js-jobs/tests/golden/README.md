# Golden rows

Rows as an older build wrote them, which this build must still read
(`a_row_without_the_newest_tag_reads_with_defaults` in `src/suite/rows.rs`). Once a release
has written rows, a golden file is never rewritten: a change to the encoding that cannot read
one is not additive. (Before the first release the time encoding changed from one `u64` of
nanoseconds to whole seconds and nanoseconds, and this file was written again for it; no
build ever released the earlier encoding.)

## `job-before-network.bin`

A job row of format 1 written before the network policy, the declared ports and the files
were fields (tags 17, 18 and 19), so it has none of them, and no retention end (tag 30). It
reads with the defaults: network `none`, no ports, no files, no retention end.

| tag | field | value |
|---|---|---|
| 1 | id | `golden-1` |
| 2 | image | `team/app:1@sha256:` and 64 `c` |
| 3 | command | `["run"]` |
| 4 | args | empty |
| 5 | env | empty |
| 7 | cpu millis | 1000 |
| 8 | memory bytes | 2^30 |
| 9 | accelerators | 0 |
| 11 | disk bytes | 0 |
| 15 | capabilities | empty |
| 16 | wall limit | 600 s |
| 20 | labels | empty |
| 21 | priority | 3 |
| 22 | state | 0 (queued) |
| 23 | submitted at | 5 s on the store's clock (seconds 5, nanoseconds 0) |
| 28 | cancel requested | 0 |
