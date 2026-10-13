# codec-bench

A headless harness that runs the same files through three decoder stacks and writes down what
happened: how fast, how much CPU, whether the pixels agree, and how each one fails.

| backend | what it is | role |
|---|---|---|
| `ffmpeg` | `ffprobe` + `ffmpeg` as external processes, raw planar output on a pipe | the oracle for pixels; a fair sequential-decode baseline; **not** a fair seek baseline (one process per seek) |
| `libav` (feature, stub) | in-process libav via rsmpeg, resident demuxer + decoder | the fair seek baseline, and VideoToolbox for hardware comparisons; port switchblade's `SeekablePlayer` |
| `filmcraft` | FilmCraft's pure-Rust demuxers and decoders, git-pinned | what EffectCraft ships; the "could we drop ffmpeg" candidate |
| `own` | your decoder crates | starts by failing every file; the report is the ladder |

Nothing links ffmpeg. The harness needs only the `ffmpeg`/`ffprobe` binaries on `PATH` for the
oracle backend and for `scripts/make-fixtures.sh`.

## Run

```sh
cargo build --release
./scripts/make-fixtures.sh                # ffmpeg-generated clips → corpus/files/ (gitignored)
./target/release/codec-bench suite --hash --max-frames 300 --seeks 12
./target/release/codec-bench report runs/<timestamp>
```

Other entry points:

```sh
codec-bench backends                                   # what this binary was built with
codec-bench run --backend filmcraft --scenario decode --hash corpus/files/fixtures/h264_1080p_gop250.mp4
codec-bench compare --a ffmpeg --b filmcraft corpus/files/fixtures/vp9_1080p.webm   # per-frame PSNR, in one process
codec-bench suite --set malformed --scenarios robust   # just the crash hunt
```

`suite` runs every (backend × file × scenario) as its own subprocess. A crash or hang is a row in
the report, not the end of the run. CPU time and peak RSS come from the kernel's child accounting
(`getrusage(RUSAGE_CHILDREN)`), so the ffmpeg processes of the CLI backend are counted too.

## Scenarios

- **decode**: every frame (or the first N) in order; wall ms per frame, CPU core-ms per frame, per-frame MD5 with `--hash`.
- **seek**: N random exact seeks from a seed (every backend gets the same targets); p50/p95 latency and whether the seeked frames match the reference's.
- **robust**: open and decode a little; the only output is how it failed. Malformed files run only this.

The runner never judges. The report compares. If you want pass/fail, write it over `results.jsonl`.

## Canonical frame

Planar Y'CbCr at the stream's own subsampling and bit depth (`Planes::Yuv8` / `Yuv16`), tightly
packed, Y Cb Cr [A]. The harness never colour-converts, so a difference is the decoder's, not the
converter's. Frame MD5s are the harness's own convention (planes concatenated, 16-bit samples
little-endian); conformance suites' reference MD5s use their own layouts and need per-suite
adapters (see `docs/DESIGN.md`).

## What the first smoke run showed (macOS, Apple silicon, 60 frames, 6 seeks)

Numbers are from `runs/smoke` on this machine and will differ on yours; they are here to show
the kind of thing the harness finds, not as results.

- **Bit-exact with ffmpeg**: H.264 (all three fixtures, including VFR), HEVC 8- and 10-bit, AV1, VP9 (after asking a quarter-frame into each frame, see the note in `backend-filmcraft`).
- **ProRes differs by one LSB** (luma PSNR ≈ 74.5 dB): expected, ProRes has no normative decoder; the report says "differs @0" because it is strictly about bit-exactness.
- **FilmCraft is faster than ffmpeg's software path on H.264 and 10-bit HEVC in wall time, but spends more CPU** (it threads wider): 1080p long-GOP H.264 at 1.6 vs 2.6 ms/frame wall, 20 vs 16 core-ms.
- **FilmCraft's AV1 decoder is ~10× slower** than ffmpeg's (dav1d): 22 vs 2.3 ms/frame.
- **FilmCraft's HEVC seek fails** on one target with "frame not produced" (five of six seeks succeed). A real bug at the pinned revision; re-test at a newer pin.
- **ffmpeg conceals the bit-flipped file and keeps going; FilmCraft stops at the first invalid `ref_idx`.** Neither crashed. Whether to conceal or stop is a policy question the harness just records.
- The ffmpeg CLI backend's seek numbers (100–500 ms) are process spawns, which is why switchblade moved to in-process libav; the `libav` feature exists to measure that properly.

## Layout

```
crates/harness-core      Decoder trait, Frame, StreamInfo, RunResult, Summary, Rng
crates/backend-ffmpeg    CLI backend (+ libav stub behind a feature)
crates/backend-filmcraft FilmCraft MediaSource adapter
crates/backend-own       your decoders
crates/codec-bench       the binary: run / suite / report / compare
corpus/manifest.toml     every file the suite knows about, by set
scripts/                 make-fixtures.sh, fetch-conformance.sh
docs/DESIGN.md           decisions and the rules for adding a backend or a corpus set
```
