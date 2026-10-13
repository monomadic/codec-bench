//! `codec-bench`: one binary, four jobs.
//!
//! - `run`     one backend, one file, one scenario, in this process; prints a `RunResult` JSON line.
//! - `suite`   every (backend × file × scenario) of a corpus, each `run` in its own subprocess,
//!             with CPU time, peak RSS, crash and timeout accounting; writes `results.jsonl`.
//! - `report`  turns a suite directory into a markdown comparison.
//! - `compare` two backends on one file in one process: per-frame luma PSNR and bit-exactness.

mod backends;
mod compare;
mod report;
mod runner;
mod scenarios;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use harness_core::Scenario;
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args, Clone)]
struct ScenarioOpts {
    /// Decode at most this many frames (decode / robust scenarios). 0 = all.
    #[arg(long, default_value_t = 0)]
    max_frames: u64,
    /// Number of random exact seeks (seek scenario).
    #[arg(long, default_value_t = 12)]
    seeks: u32,
    /// Seed for the seek targets; the same seed gives every backend the same targets.
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// Record a per-frame MD5 (the report needs it for bit-exact comparison).
    #[arg(long)]
    hash: bool,
    /// Seek to these frame indices (comma-separated) instead of random targets.
    #[arg(long, value_delimiter = ',')]
    targets: Vec<u64>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List the backends compiled into this binary.
    Backends,
    /// Run one backend on one file for one scenario, in this process.
    Run {
        #[arg(long)]
        backend: String,
        #[arg(long, value_parser = parse_scenario)]
        scenario: Scenario,
        file: PathBuf,
        #[command(flatten)]
        opts: ScenarioOpts,
    },
    /// Run a corpus across backends and scenarios, one subprocess per run.
    Suite {
        /// The corpus manifest (TOML); paths inside are relative to it.
        #[arg(long, default_value = "corpus/manifest.toml")]
        corpus: PathBuf,
        /// Only files in these sets (comma-separated: fixtures, conformance, library, malformed).
        #[arg(long, value_delimiter = ',')]
        set: Vec<String>,
        /// Backends to run (comma-separated). Default: all compiled in.
        #[arg(long, value_delimiter = ',')]
        backends: Vec<String>,
        /// Scenarios to run (comma-separated: decode, seek, robust). Default: all.
        #[arg(long, value_delimiter = ',', value_parser = parse_scenario)]
        scenarios: Vec<Scenario>,
        /// Output directory. Default: runs/<timestamp>.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Kill a run after this many seconds.
        #[arg(long, default_value_t = 300.0)]
        timeout: f64,
        #[command(flatten)]
        opts: ScenarioOpts,
    },
    /// Summarise a suite directory as markdown (to stdout and <dir>/report.md).
    Report {
        dir: PathBuf,
        /// The backend others are compared against for bit-exactness.
        #[arg(long, default_value = "ffmpeg")]
        reference: String,
    },
    /// Decode one file with two backends in this process and compare frame by frame.
    Compare {
        #[arg(long)]
        a: String,
        #[arg(long)]
        b: String,
        file: PathBuf,
        #[arg(long, default_value_t = 0)]
        max_frames: u64,
    },
}

fn parse_scenario(s: &str) -> std::result::Result<Scenario, String> {
    Scenario::parse(s).ok_or_else(|| format!("unknown scenario {s:?} (decode, seek, robust)"))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Backends => {
            for b in backends::all() {
                println!("{}", b.name);
            }
            Ok(())
        }
        Cmd::Run { backend, scenario, file, opts } => {
            let desc = backends::find(&backend).with_context(|| format!("backend {backend:?} is not compiled in"))?;
            let result = scenarios::run(desc, &file, scenario, &opts.into());
            println!("{}", serde_json::to_string(&result)?);
            Ok(())
        }
        Cmd::Suite { corpus, set, backends: names, scenarios: scs, out, timeout, opts } => {
            let chosen: Vec<_> = if names.is_empty() {
                backends::all().iter().map(|b| b.name.to_string()).collect()
            } else {
                for n in &names {
                    if backends::find(n).is_none() {
                        bail!("backend {n:?} is not compiled in (see `codec-bench backends`)");
                    }
                }
                names
            };
            let scs = if scs.is_empty() { Scenario::ALL.to_vec() } else { scs };
            let out = out.unwrap_or_else(|| PathBuf::from("runs").join(chrono::Local::now().format("%Y%m%d-%H%M%S").to_string()));
            runner::suite(&corpus, &set, &chosen, &scs, &out, timeout, &opts.into())
        }
        Cmd::Report { dir, reference } => {
            let md = report::render(&dir, &reference)?;
            std::fs::write(dir.join("report.md"), &md)?;
            print!("{md}");
            Ok(())
        }
        Cmd::Compare { a, b, file, max_frames } => {
            let da = backends::find(&a).with_context(|| format!("backend {a:?}"))?;
            let db = backends::find(&b).with_context(|| format!("backend {b:?}"))?;
            compare::run(da, db, &file, max_frames)
        }
    }
}

impl From<ScenarioOpts> for scenarios::Opts {
    fn from(o: ScenarioOpts) -> Self {
        scenarios::Opts { max_frames: o.max_frames, seeks: o.seeks, seed: o.seed, hash: o.hash, targets: o.targets }
    }
}
