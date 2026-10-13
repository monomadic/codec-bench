//! A suite directory → markdown. Numbers only; no pass/fail. Bit-exactness is against the
//! reference backend's per-frame MD5s when both runs recorded them (`--hash`).

use anyhow::Result;
use harness_core::{Outcome, RunResult, Scenario};
use std::collections::BTreeMap;
use std::path::Path;

pub fn render(dir: &Path, reference: &str) -> Result<String> {
    let results = crate::runner::results_of(dir)?;
    let meta = std::fs::read_to_string(dir.join("meta.json")).unwrap_or_default();
    let mut backends: Vec<String> = results.iter().map(|r| r.backend.clone()).collect();
    backends.sort();
    backends.dedup();
    // Reference first, then the rest in name order.
    backends.sort_by_key(|b| (b != reference, b.clone()));

    let mut md = String::new();
    md.push_str(&format!("# codec-bench report\n\n`{}`\n\n", dir.display()));
    if !meta.is_empty() {
        md.push_str("<details><summary>run metadata</summary>\n\n```json\n");
        md.push_str(meta.trim());
        md.push_str("\n```\n</details>\n\n");
    }

    for sc in Scenario::ALL {
        let rows: BTreeMap<String, Vec<&RunResult>> = results.iter().filter(|r| r.scenario == sc).fold(BTreeMap::new(), |mut m, r| {
            m.entry(short(&r.file)).or_default().push(r);
            m
        });
        if rows.is_empty() {
            continue;
        }
        md.push_str(&format!("## {}\n\n", sc.name()));
        match sc {
            Scenario::Decode => {
                md.push_str(&format!("Per frame: wall ms / CPU core-ms (process + children). Exact: every frame's MD5 equals `{reference}`'s.\n\n"));
                md.push_str(&header(&backends, &["ms/frame", "cpu ms/frame", "exact"]));
                for (file, rs) in &rows {
                    let r_ref = rs.iter().find(|r| r.backend == reference);
                    let cells: Vec<String> = backends
                        .iter()
                        .map(|b| match rs.iter().find(|r| &r.backend == b) {
                            Some(r) if r.outcome == Outcome::Ok => {
                                let f = r.frame_ms.as_ref().map(|s| s.mean).unwrap_or(0.0);
                                let cpu = r.cpu_ms.map(|c| c / r.frames.max(1) as f64);
                                let exact = match r_ref {
                                    Some(x) if x.backend == r.backend => "ref".to_string(),
                                    Some(x) => exactness(&x.frame_md5, &r.frame_md5),
                                    None => "—".into(),
                                };
                                format!("{f:.2} / {} / {exact}", cpu.map(|c| format!("{c:.1}")).unwrap_or("—".into()))
                            }
                            Some(r) => outcome_cell(&r.outcome),
                            None => "—".into(),
                        })
                        .collect();
                    md.push_str(&format!("| {file} | {} |\n", cells.join(" | ")));
                }
            }
            Scenario::Seek => {
                md.push_str(&format!("Exact random seeks: p50 / p95 ms. Same: the seeked frames' MD5s equal `{reference}`'s for the same targets. The `ffmpeg` CLI backend respawns a process per seek, so its numbers are a ceiling, not a baseline (build with `--features libav` for in-process numbers).\n\n"));
                md.push_str(&header(&backends, &["p50", "p95", "same"]));
                for (file, rs) in &rows {
                    let r_ref = rs.iter().find(|r| r.backend == reference);
                    let cells: Vec<String> = backends
                        .iter()
                        .map(|b| match rs.iter().find(|r| &r.backend == b) {
                            Some(r) if r.outcome == Outcome::Ok => {
                                let s = r.seek_ms.clone().unwrap_or_default();
                                let same = match r_ref {
                                    Some(x) if x.backend == r.backend => "ref".to_string(),
                                    Some(x) => seek_sameness(x, r),
                                    None => "—".into(),
                                };
                                format!("{:.1} / {:.1} / {same}", s.p50, s.p95)
                            }
                            Some(r) => outcome_cell(&r.outcome),
                            None => "—".into(),
                        })
                        .collect();
                    md.push_str(&format!("| {file} | {} |\n", cells.join(" | ")));
                }
            }
            Scenario::Robust => {
                md.push_str("How each backend failed (or didn't) on each file.\n\n");
                md.push_str(&header(&backends, &["outcome"]));
                for (file, rs) in &rows {
                    let cells: Vec<String> = backends
                        .iter()
                        .map(|b| match rs.iter().find(|r| &r.backend == b) {
                            Some(r) if r.outcome == Outcome::Ok => format!("ok ({} frames)", r.frames),
                            Some(r) => outcome_cell(&r.outcome),
                            None => "—".into(),
                        })
                        .collect();
                    md.push_str(&format!("| {file} | {} |\n", cells.join(" | ")));
                }
            }
        }
        md.push('\n');
    }
    Ok(md)
}

fn header(backends: &[String], cols: &[&str]) -> String {
    let heads: Vec<String> = backends.iter().map(|b| format!("{b} ({})", cols.join(" / "))).collect();
    format!("| file | {} |\n|---|{}\n", heads.join(" | "), "---|".repeat(backends.len()))
}

fn short(path: &str) -> String {
    Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string())
}

fn outcome_cell(o: &Outcome) -> String {
    match o {
        Outcome::Ok => "ok".into(),
        Outcome::Unsupported { .. } => "unsupported".into(),
        Outcome::Error { message } => format!("error: {}", trunc(message)),
        Outcome::Crash { status } => format!("**crash**: {}", trunc(status)),
        Outcome::Timeout { after_s } => format!("**timeout** {after_s:.0}s"),
    }
}

fn trunc(s: &str) -> String {
    let s = s.replace('|', "\\|").replace('\n', " ");
    if s.chars().count() > 60 { format!("{}…", s.chars().take(60).collect::<String>()) } else { s }
}

/// "exact (n)" when every hash matches, else the first differing frame.
fn exactness(reference: &[String], other: &[String]) -> String {
    if reference.is_empty() || other.is_empty() {
        return "no hashes".into();
    }
    match reference.iter().zip(other).position(|(a, b)| a != b) {
        Some(i) => format!("differs @{i}"),
        None if reference.len() == other.len() => format!("exact ({})", other.len()),
        None => format!("prefix exact ({} vs {})", other.len(), reference.len()),
    }
}

fn seek_sameness(reference: &RunResult, other: &RunResult) -> String {
    let (mut same, mut total) = (0, 0);
    for s in &other.seeks {
        if let Some(r) = reference.seeks.iter().find(|x| x.index == s.index) {
            if r.md5.is_some() && s.md5.is_some() {
                total += 1;
                if r.md5 == s.md5 {
                    same += 1;
                }
            }
        }
    }
    if total == 0 { "—".into() } else { format!("{same}/{total}") }
}
