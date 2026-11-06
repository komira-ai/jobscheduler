-- kbf-migration: expand
-- The writer generation (one row): the writer lock's holder increments it as it takes the
-- lock, and the holder's store checks it, held shared, in every commit, so a writer that
-- lost the lock commits nothing once another has taken it. A release that does not fence
-- never reads it.
CREATE TABLE writer_generation (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    generation bigint NOT NULL);
INSERT INTO writer_generation (generation) VALUES (0);
