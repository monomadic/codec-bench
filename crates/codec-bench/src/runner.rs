//! The suite driver. Each (backend, file, scenario) runs as `codec-bench run …` in its own
//! subprocess: a crash or hang is a data point, CPU time and peak RSS come from the kernel's
//! child accounting, and backends share no process state.

use crate::scenarios::Opts;
use anyhow::{Context, Result};
use harness_core::{Outcome, RunResult, Scenario};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub file: Vec<Entry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Entry {
    /// Relative to the manifest's directory.
    pub path: String,
    /// fixtures | conformance | library | malformed
    pub set: String,
    #[serde(default)]
    pub codec: String,
    #[serde(default)]
    pub notes: String,
    /// Scenarios this file takes part in; empty means all.
    #[serde(default)]
    pub scenarios: Vec<String>,
}

#[derive(Serialize)]
struct Meta<'a> {
    started: String,
    host: String,
    os: &'static str,
    arch: &'static str,
    git: Option<String>,
    ffmpeg: Option<String>,
    backends: &'a [String],
    scenarios: Vec<&'static str>,
    sets: &'a [String],
    timeout_s: f64,
    max_frames: u64,
    seeks: u32,
    seed: u64,
}

pub fn suite(manifest: &Path, sets: &[String], backends: &[String], scenarios: &[Scenario], out: &Path, timeout: f64, opts: &Opts) -> Result<()> {
    let text = std::fs::read_to_string(manifest).with_context(|| format!("read {}", manifest.display()))?;
    let m: Manifest = toml::from_str(&text).with_context(|| format!("parse {}", manifest.display()))?;
    let base = manifest.parent().unwrap_or(Path::new("."));
    let entries: Vec<&Entry> = m.file.iter().filter(|e| sets.is_empty() || sets.contains(&e.set)).collect();
    std::fs::create_dir_all(out)?;

    let meta = Meta {
        started: chrono::Local::now().to_rfc3339(),
        host: hostname(),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        git: git_describe(),
        ffmpeg: ffmpeg_version(),
        backends,
        scenarios: scenarios.iter().map(|s| s.name()).collect(),
        sets,
        timeout_s: timeout,
        max_frames: opts.max_frames,
        seeks: opts.seeks,
        seed: opts.seed,
    };
    std::fs::write(out.join("meta.json"), serde_json::to_string_pretty(&meta)?)?;
    let mut results = std::fs::File::create(out.join("results.jsonl"))?;
    let mut log = std::fs::File::create(out.join("stderr.log"))?;
    let exe = std::env::current_exe()?;

    let total = entries.len() * backends.len() * scenarios.len();
    let mut n = 0;
    let mut missing = 0;
    for e in &entries {
        let file = base.join(&e.path);
        if !file.exists() {
            missing += 1;
            eprintln!("skip (missing): {}", file.display());
            continue;
        }
        for sc in scenarios {
            if !e.scenarios.is_empty() && !e.scenarios.iter().any(|s| s == sc.name()) {
                continue;
            }
            for b in backends {
                n += 1;
                eprint!("[{n}/{total}] {b:<10} {:<7} {} … ", sc.name(), e.path);
                let r = run_one(&exe, b, *sc, &file, timeout, opts, &mut log)?;
                eprintln!("{}", describe(&r));
                writeln!(results, "{}", serde_json::to_string(&r)?)?;
            }
        }
    }
    if missing > 0 {
        eprintln!("{missing} corpus file(s) missing: run scripts/make-fixtures.sh or scripts/fetch-conformance.sh");
    }
    eprintln!("results: {}", out.join("results.jsonl").display());
    Ok(())
}

fn describe(r: &RunResult) -> String {
    match &r.outcome {
        Outcome::Ok => {
            let cpu = r.cpu_ms.map(|c| format!(", cpu {:.0} ms", c)).unwrap_or_default();
            match (r.frame_ms.as_ref(), r.seek_ms.as_ref()) {
                (Some(f), _) => format!("ok: {} frames, {:.2} ms/frame{cpu}", r.frames, f.mean),
                (_, Some(s)) => format!("ok: {} seeks, p50 {:.1} ms, p95 {:.1} ms{cpu}", s.count, s.p50, s.p95),
                _ => format!("ok{cpu}"),
            }
        }
        Outcome::Unsupported { reason } => format!("unsupported: {reason}"),
        Outcome::Error { message } => format!("error: {message}"),
        Outcome::Crash { status } => format!("CRASH: {status}"),
        Outcome::Timeout { after_s } => format!("TIMEOUT after {after_s:.0} s"),
    }
}

fn run_one(exe: &Path, backend: &str, sc: Scenario, file: &Path, timeout: f64, opts: &Opts, log: &mut std::fs::File) -> Result<RunResult> {
    let mut cmd = Command::new(exe);
    cmd.args(["run", "--backend", backend, "--scenario", sc.name()]).arg(file);
    cmd.args(["--max-frames", &opts.max_frames.to_string(), "--seeks", &opts.seeks.to_string(), "--seed", &opts.seed.to_string()]);
    if opts.hash {
        cmd.arg("--hash");
    }
    if !opts.targets.is_empty() {
        cmd.arg("--targets").arg(opts.targets.iter().map(|t| t.to_string()).collect::<Vec<_>>().join(","));
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());

    let before = children_usage();
    let start = Instant::now();
    let mut child = cmd.spawn().context("spawn codec-bench run")?;
    let mut stdout = child.stdout.take().context("stdout")?;
    let mut stderr = child.stderr.take().context("stderr")?;
    // Drain the pipes on threads so a chatty child can't block on a full pipe.
    let out_t = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut stdout, &mut s);
        s
    });
    let err_t = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut stderr, &mut s);
        s
    });
    let deadline = Duration::from_secs_f64(timeout);
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break Some(st);
        }
        if start.elapsed() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let after = children_usage();
    let out = out_t.join().unwrap_or_default();
    let err = err_t.join().unwrap_or_default();
    if !err.trim().is_empty() {
        let _ = writeln!(log, "--- {backend} {} {}\n{}", sc.name(), file.display(), err.trim_end());
    }

    let mut r = match status {
        None => {
            let mut r = RunResult::new(backend, file, sc);
            r.outcome = Outcome::Timeout { after_s: timeout };
            r
        }
        Some(st) if st.success() => match out.lines().last().and_then(|l| serde_json::from_str::<RunResult>(l).ok()) {
            Some(r) => r,
            None => {
                let mut r = RunResult::new(backend, file, sc);
                r.outcome = Outcome::Error { message: format!("no result on stdout: {}", err.lines().last().unwrap_or("").trim()) };
                r
            }
        },
        Some(st) => {
            let mut r = RunResult::new(backend, file, sc);
            r.outcome = Outcome::Crash { status: format!("{st}; {}", err.lines().last().unwrap_or("").trim()) };
            r
        }
    };
    if let (Some(b), Some(a)) = (before, after) {
        r.cpu_ms = Some((a.cpu_s - b.cpu_s).max(0.0) * 1000.0);
        // ru_maxrss over children is a running maximum, not per child: it is the largest child
        // so far. Reported as is; compare it only between runs in the same session with care.
        r.peak_rss_bytes = Some(a.maxrss_bytes);
    }
    r.host = Some(hostname());
    Ok(r)
}

struct Usage {
    cpu_s: f64,
    maxrss_bytes: u64,
}

/// Cumulative CPU time and peak RSS of waited-for children (and their waited-for children, so
/// the ffmpeg processes of the CLI backend count once that backend has reaped them).
#[cfg(unix)]
fn children_usage() -> Option<Usage> {
    // SAFETY: getrusage writes a plain struct we own; RUSAGE_CHILDREN is a valid selector.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut ru) };
    if rc != 0 {
        return None;
    }
    let tv = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    let maxrss = ru.ru_maxrss as u64 * if cfg!(target_os = "macos") { 1 } else { 1024 };
    Some(Usage { cpu_s: tv(ru.ru_utime) + tv(ru.ru_stime), maxrss_bytes: maxrss })
}

#[cfg(not(unix))]
fn children_usage() -> Option<Usage> {
    None
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| Command::new("hostname").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()))
        .unwrap_or_default()
}

fn git_describe() -> Option<String> {
    let out = Command::new("git").args(["describe", "--always", "--dirty"]).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn ffmpeg_version() -> Option<String> {
    #[cfg(feature = "ffmpeg")]
    {
        backend_ffmpeg::version()
    }
    #[cfg(not(feature = "ffmpeg"))]
    {
        None
    }
}

#[allow(dead_code)]
pub fn results_of(dir: &Path) -> Result<Vec<RunResult>> {
    let p: PathBuf = dir.join("results.jsonl");
    let text = std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
    text.lines().filter(|l| !l.trim().is_empty()).map(|l| serde_json::from_str(l).map_err(Into::into)).collect()
}
