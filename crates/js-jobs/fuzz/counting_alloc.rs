//! The allocator of the decode targets, with a limit on the live bytes of one decode.
//!
//! The allocator is `cap::Cap` over the system allocator. [`bounded`] sets its limit to the
//! bytes live when it is called plus the declared bound, and an allocation past the limit
//! fails; the default allocation-error handler then aborts the process, which the fuzzer
//! reports as a crash. The limit is lifted again when the call returns.

#[global_allocator]
static ALLOC: cap::Cap<std::alloc::System> = cap::Cap::new(std::alloc::System, usize::MAX);

/// Runs `decode` with at most `bound` live bytes above those live now; past that the
/// process aborts.
pub fn bounded<T>(bound: usize, decode: impl FnOnce() -> T) -> T {
    ALLOC
        .set_limit(ALLOC.allocated() + bound)
        .expect("the limit is above what is live");
    let out = decode();
    ALLOC
        .set_limit(usize::MAX)
        .expect("the limit is above what is live");
    out
}
