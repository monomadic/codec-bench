//! What one `run` does. Everything here happens in the calling process; the runner wraps it in a
//! subprocess for isolation and accounting.

use harness_core::dump::Luma;
use harness_core::metrics::{Rng, Summary};
use harness_core::{BackendDesc, DecodeError, Frame, Result, RunResult, Scenario, SeekSample};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct Opts {
    pub max_frames: u64,
    pub seeks: u32,
    pub seed: u64,
    pub hash: bool,
    /// Explicit seek targets instead of random ones.
    pub targets: Vec<u64>,
    /// Write luma dumps here (`<index>.y`): every seeked frame; in decode, every `dump_every`-th
    /// frame plus the frames the seek scenario would target with the same seed.
    pub dump: Option<PathBuf>,
    pub dump_every: u64,
}

/// The targets the seek scenario uses for `total` frames and these options.
fn seek_targets(opts: &Opts, total: u64) -> Vec<u64> {
    if opts.targets.is_empty() {
        let mut rng = Rng::new(opts.seed);
        (0..opts.seeks).map(|_| rng.below(total)).collect()
    } else {
        opts.targets.clone()
    }
}

/// Dumping happens outside the timed section; a failure to write is reported, not fatal.
fn dump_frame(dir: &Path, index: u64, frame: &Frame) {
    if let Some(l) = Luma::of(frame) {
        if let Err(e) = l.write(&dir.join(format!("{index}.y"))) {
            eprintln!("dump {index}: {e}");
        }
    }
}

pub fn run(desc: BackendDesc, file: &Path, scenario: Scenario, opts: &Opts) -> RunResult {
    let mut r = RunResult::new(desc.name, file, scenario);
    r.host = std::env::var("HOSTNAME").ok();
    let t0 = Instant::now();
    let mut dec = match (desc.open)(file) {
        Ok(d) => d,
        Err(e) => return r.fail(&e),
    };
    r.open_ms = ms(t0);
    r.info = Some(dec.info().clone());
    r.dump_dir = opts.dump.as_ref().map(|d| d.display().to_string());
    let res = match scenario {
        Scenario::Decode => decode(&mut *dec, &mut r, opts.max_frames, opts.hash, opts),
        // Robustness only asks how it fails; a short decode is enough.
        Scenario::Robust => decode(&mut *dec, &mut r, if opts.max_frames == 0 { 60 } else { opts.max_frames }, false, &Opts { dump: None, ..opts.clone() }),
        Scenario::Seek => seek(&mut *dec, &mut r, opts),
    };
    if let Err(e) = res {
        r = r.fail(&e);
    }
    // Again after decoding: backends may learn which decoder actually ran from the first picture.
    r.info = Some(dec.info().clone());
    r
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

fn decode(dec: &mut dyn harness_core::Decoder, r: &mut RunResult, max: u64, hash: bool, opts: &Opts) -> Result<()> {
    let wanted: BTreeSet<u64> = if opts.dump.is_some() { seek_targets(opts, dec.info().frames()).into_iter().collect() } else { BTreeSet::new() };
    let every = opts.dump_every.max(1);
    let mut per_frame = Vec::new();
    loop {
        if max > 0 && r.frames >= max {
            break;
        }
        let t = Instant::now();
        let Some(frame) = dec.next_frame()? else { break };
        per_frame.push(ms(t));
        let index = r.frames;
        r.frames += 1;
        if hash {
            r.frame_md5.push(frame.md5());
        }
        if let Some(dir) = &opts.dump {
            if index % every == 0 || wanted.contains(&index) {
                dump_frame(dir, index, &frame);
            }
        }
    }
    // Hashing and dumping sit between frames, so wall time is the sum of the decode calls.
    r.wall_ms = per_frame.iter().sum();
    r.frame_ms = Summary::of(&per_frame);
    if r.frames == 0 {
        return Err(DecodeError::Decode("no frames decoded".into()));
    }
    Ok(())
}

fn seek(dec: &mut dyn harness_core::Decoder, r: &mut RunResult, opts: &Opts) -> Result<()> {
    let total = dec.info().frames();
    if total == 0 {
        return Err(DecodeError::Decode("frame count unknown; cannot pick seek targets".into()));
    }
    // Prime once so the first seek isn't also the first decode (decoder tables, thread pools).
    let _ = dec.next_frame()?;
    let targets = seek_targets(opts, total);
    let mut times = Vec::new();
    for index in targets {
        let t = Instant::now();
        dec.seek(index)?;
        let frame = dec.next_frame()?;
        let wall_ms = ms(t);
        times.push(wall_ms);
        if let (Some(dir), Some(f)) = (&opts.dump, &frame) {
            dump_frame(dir, index, f);
        }
        r.seeks.push(SeekSample { index, wall_ms, md5: frame.as_ref().map(|f| f.md5()), pts_s: frame.as_ref().map(|f| f.pts_s) });
    }
    r.wall_ms = times.iter().sum();
    r.seek_ms = Summary::of(&times);
    Ok(())
}
