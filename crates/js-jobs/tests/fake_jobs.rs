//! The job store suite on the in-memory fake: the correct fake passes every check, and a fake
//! with a deliberate defect fails the check that defect breaks. The `expected` text is the
//! start of the failing assertion's message, so each test also pins why it fails.

use js_jobs::suite;
use js_state::fake::{FakeBackend, Flaw};

fn correct() -> FakeBackend {
    FakeBackend::new(Flaw::None)
}

js_jobs::job_store_tests!(correct);

#[test]
fn the_whole_suite_runs_in_one_pass() {
    suite::run_job_store_suite(&correct());
}

// Catches a placement whose job and allocation are written in separate steps: a store that
// applies a transaction's operations one by one until a precondition fails leaves the job
// placed with no allocation of its own.
#[test]
#[should_panic(
    expected = "placement_and_allocation_commit_together: the job was placed without its allocation"
)]
fn a_partial_transaction_breaks_atomic_placement() {
    suite::placement_and_allocation_commit_together(&FakeBackend::new(Flaw::PartialTxn));
}

// Catches an end and its release written in separate steps: the job ends while its
// allocation stays booked.
#[test]
#[should_panic(
    expected = "ending_a_placed_job_moves_its_allocation_to_releasing_in_the_same_commit: the job ended without its allocation"
)]
fn a_partial_transaction_breaks_an_atomic_end() {
    suite::ending_a_placed_job_moves_its_allocation_to_releasing_in_the_same_commit(
        &FakeBackend::new(Flaw::PartialTxn),
    );
}

// Catches history that is acknowledged and not durable: a restart loses the freed row.
#[test]
#[should_panic(
    expected = "a_freed_allocation_row_is_kept_as_history: the history did not survive a restart"
)]
fn an_acknowledgement_before_the_sync_loses_history() {
    suite::a_freed_allocation_row_is_kept_as_history(&FakeBackend::new(Flaw::AckBeforeSync));
}

// Catches pages that resume at their token instead of past it: a job is listed twice.
#[test]
#[should_panic(
    expected = "list_jobs_pages_in_key_order_and_filters_by_state: the pages do not concatenate"
)]
fn a_scan_that_resumes_at_its_token_repeats_a_job() {
    suite::list_jobs_pages_in_key_order_and_filters_by_state(&FakeBackend::new(
        Flaw::ScanResumesAtToken,
    ));
}
