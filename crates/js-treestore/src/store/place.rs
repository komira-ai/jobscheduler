//! Placing a sealed stage as a tree: the rename, the record and the table entry.

use std::collections::BTreeSet;
use std::path::Path;

use super::{Entry, Inner, Placing, Reservation, RunId, TreeHandle, TreeWanted, sync_dir};
use crate::error::{Error, Step};
use crate::record::Record;
use crate::sealer::Sealer;
use crate::source::LayerSource;

impl<S: Sealer, F: LayerSource> Inner<S, F> {
    /// Places the sealed stage as the tree, records it and enters it in the table, one
    /// placement of a diff id at a time. A tree already in place is accepted only when this
    /// store holds it as current (another extraction of the same layer won); anything else
    /// there is not ours to take for the new content.
    pub(super) fn commit(
        &self,
        want: &TreeWanted,
        stage: &Path,
        sealed_bytes: u64,
        reservation: &mut Reservation<'_, S, F>,
        run: Option<&RunId>,
    ) -> Result<Option<TreeHandle>, Error> {
        let diff_id = &want.diff_id;
        let mut state = self.lock();
        while state.placing.contains(diff_id) {
            state.waiting += 1;
            state = self.done.wait(state).expect("tree store state poisoned");
            state.waiting -= 1;
        }
        state.placing.insert(diff_id.clone());
        drop(state);
        let mut placing = Placing {
            inner: self,
            diff_id: diff_id.clone(),
            unentered: false,
        };
        self.place(want, stage, sealed_bytes, reservation, run, &mut placing)
    }

    /// The steps between the rename and the table entry. A failure other than a simulated
    /// death leaves nothing in place.
    fn record_placed(&self, want: &TreeWanted, sealed_bytes: u64) -> Result<(), Error> {
        let diff_id = &want.diff_id;
        sync_dir(&self.dir.join("trees"))?;
        self.reached(Step::Renamed)?;
        let record = Record {
            diff_id: diff_id.as_str().to_owned(),
            blob: want.blob.digest.to_string(),
            extractor_version: self.cfg.extractor_version,
            bytes: sealed_bytes,
        };
        self.write_record(diff_id, &record)?;
        self.reached(Step::Recorded)
    }

    fn place(
        &self,
        want: &TreeWanted,
        stage: &Path,
        sealed_bytes: u64,
        reservation: &mut Reservation<'_, S, F>,
        run: Option<&RunId>,
        placing: &mut Placing<'_, S, F>,
    ) -> Result<Option<TreeHandle>, Error> {
        let diff_id = &want.diff_id;
        let version = self.cfg.extractor_version;
        let target = self.tree_path(diff_id);
        let moved = match std::fs::rename(stage, &target) {
            // A sealed directory may refuse to move (macOS refuses to rename an immutable one).
            Err(_) if stage.exists() & !target.exists() => self.move_unsealed(stage, &target),
            other => other.map_err(|source| Error::Io {
                what: "place tree",
                source,
            }),
        };
        match moved {
            Ok(()) => {
                placing.unentered = true;
                match self.record_placed(want, sealed_bytes) {
                    Ok(()) => {}
                    Err(Error::Crashed(step)) => return Err(Error::Crashed(step)),
                    Err(error) => {
                        // Out of the way, with its record, so that a retry can place it.
                        return Err(match self.trash_tree(diff_id) {
                            Ok(()) => error,
                            Err(cleanup) => Error::Cleanup {
                                cause: Box::new(error),
                                cleanup: Box::new(cleanup),
                            },
                        });
                    }
                }
                let mut state = self.lock();
                placing.unentered = false;
                state.reserved -= reservation.take();
                state.next_seq += 1;
                let seq = state.next_seq;
                let last_use_ms = self.now();
                state.trees.insert(
                    diff_id.clone(),
                    Entry {
                        bytes: sealed_bytes,
                        version,
                        pins: BTreeSet::new(),
                        seq,
                        last_use_ms,
                        doomed: false,
                    },
                );
                run.map(|run| self.pin_locked(&mut state, diff_id, run))
                    .transpose()
            }
            Err(_) if target.exists() => {
                let mut state = self.lock();
                let ours = state
                    .trees
                    .get(diff_id)
                    .is_some_and(|e| (e.version == version) & !e.doomed);
                if !ours {
                    return Err(Error::Conflict(diff_id.as_str().to_owned()));
                }
                let pinned = run
                    .map(|run| self.pin_locked(&mut state, diff_id, run))
                    .transpose();
                drop(state);
                self.discard(stage);
                pinned
            }
            Err(error) => Err(error),
        }
    }

    /// Moves a sealed stage that would not move into place `target`: unsealed, moved, and
    /// sealed again where it now stands, as [`Sealer::seal`] accepts a tree in place. If
    /// the unsealing or the second move fails, the stage is sealed again where it is (the
    /// error says so if that fails too); if the seal in place fails, the tree is moved back
    /// to the stage and that error is returned, so nothing unsealed is ever left in `trees/`.
    fn move_unsealed(&self, stage: &Path, target: &Path) -> Result<(), Error> {
        self.sealer
            .unseal_for_delete(stage)
            .map_err(Error::Seal)
            .and_then(|()| {
                std::fs::rename(stage, target).map_err(|source| Error::Io {
                    what: "place tree",
                    source,
                })
            })
            .map_err(|error| self.seal_again(stage, error))?;
        self.sealer.seal(target).map(drop).map_err(|why| {
            let _ = std::fs::rename(target, stage);
            Error::Seal(why)
        })
    }
}
