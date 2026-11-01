//! The markdown tables and the JSON of a report.

use std::collections::BTreeSet;
use std::io::Write;

use crate::error::Error;
use crate::types::{
    Comparison, CvComparison, GroupReport, Interval, MAX_PLAN_RUNS, MetricReport, Permutation,
    Report, RunRecord, WARMUP,
};

const NA: &str = "NA";

fn num(v: f64) -> String {
    format!("{v:.4}")
}

fn opt(v: Option<f64>) -> String {
    v.map_or_else(|| NA.to_string(), num)
}

fn interval(i: Option<Interval>) -> String {
    i.map_or_else(
        || NA.to_string(),
        |i| format!("[{}, {}]", num(i.lo), num(i.hi)),
    )
}

fn percent(v: f64) -> String {
    format!("{:+.2}%", v * 100.0)
}

fn percent_cell(v: Option<f64>) -> String {
    v.map_or_else(|| NA.to_string(), percent)
}

fn percent_interval(i: Option<Interval>) -> String {
    i.map_or_else(
        || NA.to_string(),
        |i| format!("[{}, {}]", percent(i.lo), percent(i.hi)),
    )
}

fn permutation(p: Option<Permutation>) -> String {
    p.map_or_else(
        || NA.to_string(),
        |p| format!("{:.4} ({}, {} splits)", p.p, p.method.as_str(), p.splits),
    )
}

fn yes_no(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

fn runs_cell(n: Option<usize>) -> String {
    n.map_or_else(|| format!(">{MAX_PLAN_RUNS}"), |n| n.to_string())
}

fn arm_row(g: &GroupReport, m: &MetricReport) -> String {
    let stats = match &m.summary {
        Some(s) => [
            num(s.mean),
            interval(s.mean_ci),
            num(s.median),
            opt(s.std),
            num(s.min),
            num(s.max),
            opt(s.cv),
            interval(s.cv_ci),
        ],
        None => std::array::from_fn(|_| NA.to_string()),
    };
    format!(
        "| {} | {} | {} | {} | {} | {} |\n",
        g.setting,
        g.arm,
        m.n,
        m.n_missing,
        g.n_warmup,
        stats.join(" | ")
    )
}

fn comparison_row(c: &Comparison) -> String {
    format!(
        "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
        c.setting,
        c.metric,
        c.base,
        c.arm,
        c.n_base,
        c.n_arm,
        opt(c.diff),
        interval(c.diff_ci),
        percent_cell(c.rel_change),
        percent_interval(c.rel_ci),
        permutation(c.permutation),
        c.verdict.as_str(),
        yes_no(c.underpowered),
    )
}

fn cv_comparison_row(c: &CvComparison) -> String {
    format!(
        "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
        c.setting,
        c.metric,
        c.base,
        c.arm,
        c.n_base,
        c.n_arm,
        opt(c.cv_base),
        opt(c.cv_arm),
        opt(c.ratio),
        interval(c.ratio_ci),
        c.verdict.as_str(),
        yes_no(c.underpowered),
    )
}

fn run_row(r: &RunRecord) -> String {
    let values: Vec<String> = r
        .metrics
        .iter()
        .map(|(name, v)| format!("{name}={}", opt(*v)))
        .collect();
    format!(
        "| {} | {} | {} | {} | {} | {} | {} |\n",
        r.setting,
        r.arm,
        r.run_index,
        r.label,
        r.seed,
        r.commit,
        values.join("; ")
    )
}

/// The report as markdown: a table per metric, the comparisons, the run counts and, last,
/// every run (warm-ups labelled).
#[must_use]
pub fn to_markdown(report: &Report) -> String {
    let mut s = String::new();
    let names: BTreeSet<&str> = report
        .groups
        .iter()
        .flat_map(|g| g.metrics.iter().map(|m| m.metric.as_str()))
        .collect();
    for name in &names {
        s.push_str(&format!("### {name}\n\n"));
        s.push_str("| setting | arm | n | missing | warm-ups | mean | 95% CI of mean | median | std | min | max | std/mean | 95% CI of std/mean |\n");
        s.push_str("|---|---|---|---|---|---|---|---|---|---|---|---|---|\n");
        for g in &report.groups {
            for m in g.metrics.iter().filter(|m| m.metric == *name) {
                s.push_str(&arm_row(g, m));
            }
        }
        s.push('\n');
    }
    if !report.comparisons.is_empty() {
        s.push_str("### Comparisons (Welch 95% interval of the difference of means)\n\n");
        s.push_str("| setting | metric | base | arm | n base | n arm | difference | 95% CI | change | 95% CI of change | permutation p | verdict | underpowered |\n");
        s.push_str("|---|---|---|---|---|---|---|---|---|---|---|---|---|\n");
        for c in &report.comparisons {
            s.push_str(&comparison_row(c));
        }
        s.push('\n');
    }
    if !report.cv_comparisons.is_empty() {
        s.push_str("### Comparisons of std/mean\n\n");
        s.push_str("| setting | metric | base | arm | n base | n arm | std/mean base | std/mean arm | ratio | 95% CI of ratio | verdict | underpowered |\n");
        s.push_str("|---|---|---|---|---|---|---|---|---|---|---|---|\n");
        for c in &report.cv_comparisons {
            s.push_str(&cv_comparison_row(c));
        }
        s.push('\n');
    }
    let plans: Vec<String> = report
        .groups
        .iter()
        .flat_map(|g| {
            g.metrics.iter().flat_map(move |m| {
                m.plans.iter().map(move |p| {
                    format!(
                        "| {} | {} | {} | {} | {} | {} | {} |\n",
                        g.setting,
                        g.arm,
                        m.metric,
                        opt(m.summary.as_ref().and_then(|x| x.cv)),
                        percent(p.effect),
                        runs_cell(p.min_runs_ci),
                        runs_cell(p.min_runs_power80),
                    )
                })
            })
        })
        .collect();
    if !plans.is_empty() {
        s.push_str("### Runs per arm needed for an effect\n\n");
        s.push_str("| setting | arm | metric | std/mean | effect | runs for a 95% half-width at most the effect | runs for about 80% power |\n");
        s.push_str("|---|---|---|---|---|---|---|\n");
        for row in plans {
            s.push_str(&row);
        }
        s.push('\n');
    }
    s.push_str(&format!(
        "### Runs\n\nEvery run is listed. Runs labelled `{WARMUP}` are left out of the statistics; NA is a value the run did not produce.\n\n"
    ));
    s.push_str(
        "| setting | arm | run | label | seed | commit | values |\n|---|---|---|---|---|---|---|\n",
    );
    for g in &report.groups {
        for r in &g.runs {
            s.push_str(&run_row(r));
        }
    }
    s
}

/// Writes the report as pretty JSON.
///
/// # Errors
/// [`Error::Json`] when the writer fails.
pub fn write_json(report: &Report, out: &mut dyn Write) -> Result<(), Error> {
    serde_json::to_writer_pretty(out, report)?;
    Ok(())
}
