//! The agent's warnings in its own log: every `tracing` event at `WARN` or `ERROR` that the
//! agent's code makes is one line on standard error, which the unit's journal keeps, as
//! `js-agent: warning: <message> <field>=<value> ...` (`error:` for an `ERROR` event).
//!
//! Without a subscriber installed, `tracing` drops every event, and the warnings the agent's
//! code makes where it has no event of the lifecycle to say it through went nowhere: among
//! them the check of a run's input sources after its job failed, whose result then says
//! `input-unreadable` while the host's log said nothing of which run or which source. The
//! binary installs this subscriber once, at the start of [`crate::main`]; nothing at `INFO`
//! or below is written, so the journal holds what the agent already prints and these
//! warnings.

use std::fmt;
use std::io;

use tracing::{Event, Level, Subscriber};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields, MakeWriter};
use tracing_subscriber::registry::LookupSpan;

/// The most verbose level written.
const LEVEL: Level = Level::WARN;

/// The format of a line: the program's name, the level in a word, the event's message and
/// fields, as the agent's other lines are.
struct Line;

impl<S, N> FormatEvent<S, N> for Line
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let word = match *event.metadata().level() {
            Level::ERROR => "error",
            _ => "warning",
        };
        write!(writer, "js-agent: {word}: ")?;
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

/// The subscriber that writes the agent's warnings to what `make` makes, one line each.
fn subscriber<W>(make: W) -> impl Subscriber + Send + Sync
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    tracing_subscriber::fmt()
        .with_writer(make)
        .with_ansi(false)
        .with_max_level(LEVEL)
        .event_format(Line)
        .finish()
}

/// Installs, for every thread of the process, the subscriber that writes to `make`; `false`
/// when the process already has one (it is kept: only the first install counts).
fn install_with<W>(make: W) -> bool
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    tracing::subscriber::set_global_default(subscriber(make)).is_ok()
}

/// Installs the subscriber that writes the agent's warnings to standard error, for every
/// thread of the process; `false` when the process already has one.
pub(crate) fn install() -> bool {
    install_with(io::stderr)
}

#[cfg(test)]
#[path = "warnings_tests.rs"]
mod tests;
