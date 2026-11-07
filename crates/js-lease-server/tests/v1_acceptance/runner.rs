//! The runner of the cases, in place of libtest's: the cases run one at a time, in order,
//! each to its end, and a case that panics fails. The crun runtime's seams are function
//! pointers that read the current case's node, so one case at a time is what makes each
//! case's node its own.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::process::ExitCode;

/// A case: its name and its body.
pub type Case = (String, Box<dyn Fn()>);

/// What the arguments select.
#[derive(Debug, Default, PartialEq, Eq)]
struct Selection {
    filters: Vec<String>,
    skips: Vec<String>,
    exact: bool,
    list: bool,
    ignored_only: bool,
}

impl Selection {
    /// Reads libtest's arguments (the program name left out).
    fn parse(args: impl IntoIterator<Item = String>) -> Self {
        let mut chosen = Self::default();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--exact" => chosen.exact = true,
                "--list" => chosen.list = true,
                "--ignored" => chosen.ignored_only = true,
                "--skip" => chosen.skips.extend(args.next()),
                // Flags that take a value: the value is not a filter.
                "--test-threads" | "--format" | "--color" | "-Z" | "--logfile" => {
                    let _ = args.next();
                }
                flag if flag.starts_with('-') => {}
                filter => chosen.filters.push(filter.to_owned()),
            }
        }
        chosen
    }

    /// Whether the case `name` runs.
    fn runs(&self, name: &str) -> bool {
        let matches = |pattern: &String| match self.exact {
            true => name == pattern,
            false => name.contains(pattern.as_str()),
        };
        !self.ignored_only
            && (self.filters.is_empty() || self.filters.iter().any(matches))
            && !self.skips.iter().any(|skip| name.contains(skip.as_str()))
    }
}

/// Runs the cases the process's arguments select, and says how each ended, as libtest does.
pub fn run(cases: &[Case]) -> ExitCode {
    let chosen = Selection::parse(std::env::args().skip(1));
    let selected: Vec<&Case> = cases.iter().filter(|(name, _)| chosen.runs(name)).collect();
    if chosen.list {
        for (name, _) in &selected {
            println!("{name}: test");
        }
        return ExitCode::SUCCESS;
    }
    println!("\nrunning {} tests", selected.len());
    let mut failed = Vec::new();
    for (name, body) in &selected {
        println!("test {name} ...");
        match catch_unwind(AssertUnwindSafe(body)) {
            Ok(()) => println!("test {name} ... ok"),
            Err(_) => {
                println!("test {name} ... FAILED");
                failed.push(name.as_str());
            }
        }
    }
    let passed = selected.len() - failed.len();
    if failed.is_empty() {
        println!("\ntest result: ok. {passed} passed; 0 failed\n");
        ExitCode::SUCCESS
    } else {
        println!("\nfailures:");
        for name in &failed {
            println!("    {name}");
        }
        println!(
            "\ntest result: FAILED. {passed} passed; {} failed\n",
            failed.len()
        );
        ExitCode::from(101)
    }
}
