//! The job list's pages at their edges: an empty table, pages with nothing to show, the page
//! cap, limits at and around the table's size, and rows that change between pages.

use std::num::NonZeroUsize;

use js_state::conformance::Backend;
use js_state::{Cond, Store, Txn};

use super::{Rig, host, rig, share};
use crate::codec::{FORMAT, job_tag, set_field};
use crate::{JobId, JobPage, JobState, MAX_PAGE};

fn limit(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).expect("a limit above zero")
}

/// Every page of a list with `state` as the filter and `limit` as the page size.
fn walk<S: Store>(rig: &Rig<S>, state: Option<JobState>, limit: NonZeroUsize) -> Vec<JobPage> {
    let mut page = rig
        .jobs
        .list_jobs(state, None, limit)
        .expect("the first page");
    let mut pages = Vec::new();
    while let Some(token) = page.resume.clone() {
        pages.push(page);
        page = rig
            .jobs
            .list_jobs(state, Some(&token), limit)
            .expect("the next page");
    }
    pages.push(page);
    pages
}

fn ids(pages: &[JobPage]) -> Vec<JobId> {
    pages
        .iter()
        .flat_map(|page| page.jobs.iter().map(|job| job.id.clone()))
        .collect()
}

/// An empty table is one empty page; a page with no job that passes the filter, or only rows
/// a later build wrote, still leads on to the next page; a limit of one walks every job; a
/// limit equal to the table holds it all in one page, and one less needs a second.
pub fn list_jobs_walks_past_pages_with_nothing_to_show<B: Backend>(backend: &B) {
    let name = "list_jobs_walks_past_pages_with_nothing_to_show";
    let rig = rig(backend);
    let empty = rig.jobs.list_jobs(None, None, limit(10)).expect("a page");
    assert_eq!(
        (empty.jobs.len(), empty.skipped.len(), empty.resume),
        (0, 0, None),
        "{name}: an empty table is not one empty page"
    );

    // j00..j29, the middle ten cancelled; then ten rows a later build wrote; then z00.
    let mut queued = Vec::new();
    for n in 0..30 {
        let id = rig.queue(&format!("j{n:02}"));
        if (10..20).contains(&n) {
            rig.jobs.request_cancel(&id).expect("cancels");
        } else {
            queued.push(id);
        }
    }
    let newer: Vec<String> = (0..10).map(|n| format!("job/n{n:02}")).collect();
    for key in &newer {
        rig.put_raw(key, vec![FORMAT + 1]);
    }
    queued.push(rig.queue("z00"));

    let pages = walk(&rig, Some(JobState::Queued), limit(10));
    let shown: Vec<usize> = pages.iter().map(|page| page.jobs.len()).collect();
    assert_eq!(
        shown,
        [10, 0, 10, 0, 1],
        "{name}: the filtered pages are not ten, none, ten, none (skipped) and one"
    );
    assert_eq!(
        ids(&pages),
        queued,
        "{name}: the filtered walk is not the queued jobs"
    );
    let skipped: Vec<String> = pages.iter().flat_map(|page| page.skipped.clone()).collect();
    assert_eq!(skipped, newer, "{name}: the skipped rows are not named");

    let one_by_one = walk(&rig, None, limit(1));
    assert_eq!(
        (one_by_one.len(), ids(&one_by_one).len()),
        (41, 31),
        "{name}: a limit of one does not walk every row"
    );
    let all = rig.jobs.list_jobs(None, None, limit(41)).expect("a page");
    assert_eq!(
        (all.jobs.len(), all.resume),
        (31, None),
        "{name}: a limit of the table's size needs a second page"
    );
    let short = rig.jobs.list_jobs(None, None, limit(40)).expect("a page");
    assert_eq!(
        short.resume.as_ref().map(|token| token.key().to_owned()),
        Some("job/n09".to_owned()),
        "{name}: a limit one below the table's size has no second page"
    );
}

/// A page never holds more than [`MAX_PAGE`] rows, whatever limit is asked for.
pub fn list_jobs_is_capped_at_max_page<B: Backend>(backend: &B) {
    let name = "list_jobs_is_capped_at_max_page";
    let rig = rig(backend);
    rig.queue("cap-0000");
    let template = rig.raw("job/cap-0000").expect("written").value;
    let batch: Vec<Txn> = (1..=MAX_PAGE.get())
        .map(|n| {
            let id = format!("cap-{n:04}");
            let row = set_field(&template, job_tag::ID, id.as_bytes()).expect("the row parses");
            Txn::new().put(format!("job/{id}"), row, Cond::Absent)
        })
        .collect();
    for result in rig.store.apply_batch(batch).expect("reachable") {
        result.expect("a fresh row commits");
    }
    let first = rig
        .jobs
        .list_jobs(None, None, NonZeroUsize::MAX)
        .expect("a page");
    assert_eq!(
        (first.jobs.len(), first.resume.is_some()),
        (MAX_PAGE.get(), true),
        "{name}: a page held more than the cap, or ended the list"
    );
    let token = first.resume.expect("a token");
    let rest = rig
        .jobs
        .list_jobs(None, Some(&token), NonZeroUsize::MAX)
        .expect("a page");
    assert_eq!(
        (rest.jobs.len(), rest.resume),
        (1, None),
        "{name}: the page after the cap is not the one row left"
    );
}

/// Rows that change between pages, as the store's scan promises: a job recorded behind the
/// token is not returned, one ahead of it is, one deleted ahead of it is not, one changed ahead
/// of it shows its new state, and no job is returned twice.
pub fn list_jobs_follows_rows_that_change_between_pages<B: Backend>(backend: &B) {
    let name = "list_jobs_follows_rows_that_change_between_pages";
    let rig = rig(backend);
    for n in (2..=20).step_by(2) {
        rig.queue(&format!("c{n:02}"));
    }
    let first = rig.jobs.list_jobs(None, None, limit(5)).expect("a page");
    let token = first.resume.clone().expect("more than one page");
    assert_eq!(
        token.key(),
        "job/c10",
        "{name}: the first page ends elsewhere"
    );

    rig.queue("c01");
    rig.queue("c11");
    rig.delete_raw("job/c12");
    let changed = super::job_id("c14");
    rig.jobs
        .place(&changed, host("host-a"), share())
        .expect("placed");

    let mut pages = vec![first];
    let mut page = rig
        .jobs
        .list_jobs(None, Some(&token), limit(5))
        .expect("a page");
    while let Some(next) = page.resume.clone() {
        pages.push(page);
        page = rig
            .jobs
            .list_jobs(None, Some(&next), limit(5))
            .expect("a page");
    }
    pages.push(page);
    let listed: Vec<String> = ids(&pages).iter().map(|id| id.to_string()).collect();
    assert_eq!(
        listed,
        [
            "c02", "c04", "c06", "c08", "c10", "c11", "c14", "c16", "c18", "c20"
        ],
        "{name}: the walk does not follow the rows that changed"
    );
    let state = pages
        .iter()
        .flat_map(|page| page.jobs.iter())
        .find(|job| job.id == changed)
        .map(|job| job.state);
    assert_eq!(
        state,
        Some(JobState::Placed),
        "{name}: a row changed ahead of the token shows its old state"
    );
}
