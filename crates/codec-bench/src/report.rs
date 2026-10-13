//! A suite directory → markdown. Numbers only; no pass/fail. Bit-exactness is against the
//! reference backend's per-frame MD5s when both runs recorded them (`--hash`).

use anyhow::Result;
use harness_core::dump::Luma;
use harness_core::{Outcome, RunResult, Scenario};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

pub fn render(dir: &Path, reference: &str) -> Result<String> {
    let results = crate::runner::results_of(dir)?;
    let meta = std::fs::read_to_string(dir.join("meta.json")).unwrap_or_default();
    let mut backends: Vec<String> = results.iter().map(|r| r.backend.clone()).collect();
    backends.sort();
    backends.dedup();
    // Reference first, then the rest in name order.
    backends.sort_by_key(|b| (b != reference, b.clone()));

    let mut dumps: DumpCache = HashMap::new();
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
                md.push_str(&format!("Per frame: wall ms / CPU core-ms (process + children). Exact: every frame's MD5 equals `{reference}`'s. PSNR: the lowest luma PSNR over the dumped frames against `{reference}`'s frame at the same index (`suite --psnr`); ∞ when every dumped frame is identical.\n\n"));
                md.push_str(&header(&backends, &["ms/frame", "cpu ms/frame", "exact", "min PSNR"]));
                for (file, rs) in &rows {
                    let r_ref = rs.iter().find(|r| r.backend == reference);
                    let truth_dump = r_ref.and_then(|r| r.dump_dir.clone()).map(PathBuf::from);
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
                                let psnr = match (&truth_dump, &r.dump_dir) {
                                    (Some(t), Some(d)) if r.backend != reference => min_psnr(&mut dumps, t, Path::new(d), None),
                                    _ => "—".into(),
                                };
                                format!("{f:.2} / {} / {exact} / {psnr}", cpu.map(|c| format!("{c:.1}")).unwrap_or("—".into()))
                            }
                            Some(r) => outcome_cell(&r.outcome),
                            None => "—".into(),
                        })
                        .collect();
                    md.push_str(&format!("| {file} | {} |\n", cells.join(" | ")));
                }
            }
            Scenario::Seek => {
                md.push_str(&format!("Exact random seeks: p50 / p95 ms. True: the seeked frame's MD5 equals the frame at that index in `{reference}`'s *sequential* decode of the same file (ground truth, needs a decode run with `--hash` that covers the index); otherwise falls back to `{reference}`'s own seek result for that target, marked `~`. The `ffmpeg` CLI backend respawns a process per seek, so its numbers are a ceiling, not a baseline (build with `--features libav` for in-process numbers).\n\n"));
                md.push_str(&header(&backends, &["p50", "p95", "true", "min PSNR"]));
                for (file, rs) in &rows {
                    let r_ref = rs.iter().find(|r| r.backend == reference);
                    let truth = results.iter().find(|r| r.backend == reference && r.scenario == Scenario::Decode && short(&r.file) == *file && !r.frame_md5.is_empty());
                    let truth_dump = results.iter().find(|r| r.backend == reference && r.scenario == Scenario::Decode && short(&r.file) == *file && r.dump_dir.is_some()).and_then(|r| r.dump_dir.clone()).map(PathBuf::from);
                    let cells: Vec<String> = backends
                        .iter()
                        .map(|b| match rs.iter().find(|r| &r.backend == b) {
                            Some(r) if r.outcome == Outcome::Ok => {
                                let s = r.seek_ms.clone().unwrap_or_default();
                                let same = match (truth, r_ref) {
                                    (Some(t), _) if r.seeks.iter().all(|s| (s.index as usize) < t.frame_md5.len()) => seek_truth(t, r),
                                    (_, Some(x)) if x.backend == r.backend => "ref".to_string(),
                                    (_, Some(x)) => format!("~{}", seek_sameness(x, r)),
                                    _ => "—".into(),
                                };
                                let psnr = match (&truth_dump, &r.dump_dir) {
                                    (Some(t), Some(d)) => {
                                        let idx: Vec<u64> = r.seeks.iter().map(|s| s.index).collect();
                                        min_psnr(&mut dumps, t, Path::new(d), Some(&idx))
                                    }
                                    _ => "—".into(),
                                };
                                format!("{:.1} / {:.1} / {same} / {psnr}", s.p50, s.p95)
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

type DumpCache = HashMap<PathBuf, Option<std::sync::Arc<Luma>>>;

fn load(cache: &mut DumpCache, path: PathBuf) -> Option<std::sync::Arc<Luma>> {
    cache.entry(path.clone()).or_insert_with(|| Luma::read(&path).ok().map(std::sync::Arc::new)).clone()
}

/// The lowest luma PSNR of `dir`'s dumped frames against `truth`'s frame of the same index.
/// `indices`: compare only these (seeks); `None`: every dump in `dir` that `truth` also has.
fn min_psnr(cache: &mut DumpCache, truth: &Path, dir: &Path, indices: Option<&[u64]>) -> String {
    let idx: Vec<u64> = match indices {
        Some(i) => i.to_vec(),
        None => std::fs::read_dir(dir)
            .map(|rd| rd.flatten().filter_map(|e| e.path().file_stem()?.to_str()?.parse().ok()).collect())
            .unwrap_or_default(),
    };
    let (mut min, mut n, mut inf) = (f64::INFINITY, 0, 0);
    for i in idx {
        let (Some(a), Some(b)) = (load(cache, truth.join(format!("{i}.y"))), load(cache, dir.join(format!("{i}.y")))) else { continue };
        let Some(db) = b.psnr(&a) else { continue };
        n += 1;
        if db.is_infinite() {
            inf += 1;
        } else {
            min = min.min(db);
        }
    }
    if n == 0 {
        "—".into()
    } else if inf == n {
        format!("∞ ({n})")
    } else {
        format!("{min:.1} dB ({n})")
    }
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

/// Seeked frames against the reference's sequential decode: "n/m", listing the wrong indices.
fn seek_truth(decode: &RunResult, other: &RunResult) -> String {
    let mut wrong = Vec::new();
    let mut total = 0;
    for s in &other.seeks {
        let (Some(m), Some(t)) = (&s.md5, decode.frame_md5.get(s.index as usize)) else { continue };
        total += 1;
        if m != t {
            wrong.push(s.index.to_string());
        }
    }
    if total == 0 {
        "—".into()
    } else if wrong.is_empty() {
        format!("{total}/{total}")
    } else {
        format!("{}/{total} (wrong: {})", total - wrong.len(), wrong.join(","))
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
