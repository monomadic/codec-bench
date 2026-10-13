//! Two backends on one file, in one process, frame by frame: bit-exact count and luma PSNR.
//! The quickest way to see *where* a decoder diverges while you work on it.

use anyhow::{bail, Result};
use harness_core::BackendDesc;
use std::path::Path;

pub fn run(a: BackendDesc, b: BackendDesc, file: &Path, max_frames: u64) -> Result<()> {
    let mut da = (a.open)(file).map_err(|e| anyhow::anyhow!("{}: {e}", a.name))?;
    let mut db = (b.open)(file).map_err(|e| anyhow::anyhow!("{}: {e}", b.name))?;
    let (ia, ib) = (da.info().clone(), db.info().clone());
    println!("{:<10} {}x{} {} {} fps={:?} frames≈{}", a.name, ia.width, ia.height, ia.codec, ia.pixel_format, ia.frame_rate, ia.frames());
    println!("{:<10} {}x{} {} {} fps={:?} frames≈{}", b.name, ib.width, ib.height, ib.codec, ib.pixel_format, ib.frame_rate, ib.frames());
    if (ia.width, ia.height) != (ib.width, ib.height) {
        bail!("dimensions differ; nothing to compare");
    }
    let (mut n, mut exact) = (0u64, 0u64);
    let (mut min_db, mut sum_db, mut finite) = (f64::INFINITY, 0.0, 0u64);
    let mut first_diff = None;
    loop {
        if max_frames > 0 && n >= max_frames {
            break;
        }
        let fa = da.next_frame().map_err(|e| anyhow::anyhow!("{} frame {n}: {e}", a.name))?;
        let fb = db.next_frame().map_err(|e| anyhow::anyhow!("{} frame {n}: {e}", b.name))?;
        let (fa, fb) = match (fa, fb) {
            (Some(fa), Some(fb)) => (fa, fb),
            (None, None) => break,
            (ended_a, _) => {
                println!("stream lengths differ after {n} frames ({} ended first)", if ended_a.is_none() { a.name } else { b.name });
                break;
            }
        };
        if fa.md5() == fb.md5() {
            exact += 1;
        } else {
            first_diff.get_or_insert(n);
            match fa.luma_psnr(&fb) {
                Some(db) if db.is_finite() => {
                    min_db = min_db.min(db);
                    sum_db += db;
                    finite += 1;
                    if n < 10 || n % 50 == 0 {
                        println!("frame {n:>6}: luma PSNR {db:.2} dB");
                    }
                }
                Some(_) => {}
                None => println!("frame {n:>6}: differs, not comparable (format/depth)"),
            }
        }
        n += 1;
    }
    println!();
    println!("frames compared: {n}");
    println!("bit-exact:       {exact} ({:.1}%)", if n > 0 { exact as f64 * 100.0 / n as f64 } else { 0.0 });
    if let Some(i) = first_diff {
        println!("first difference at frame {i}");
    }
    if finite > 0 {
        println!("luma PSNR of differing frames: min {min_db:.2} dB, mean {:.2} dB", sum_db / finite as f64);
    }
    Ok(())
}
