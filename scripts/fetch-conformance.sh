#!/usr/bin/env bash
# Public conformance bitstreams. These are large; fetch what you are working on and add a
# [[file]] entry per stream to corpus/manifest.toml. Nothing here is committed.
#
#   H.264 (JVT):   https://www.itu.int/wftp3/av-arch/jvt-site/draft_conformance/
#                  (professional profiles under .../professional_profiles/)
#   HEVC (JCT-VC): https://www.itu.int/wftp3/av-arch/jctvc-site/bitstream_exchange/draft_conformance/
#   VP9 (libvpx):  https://storage.googleapis.com/downloads.webmproject.org/test_data/libvpx/
#                  (vp90-2-*.webm with .md5 companions; the file list is test/test_vectors.cc upstream)
#   AV1 (AOM):     https://storage.googleapis.com/aom-test-data/  (av1-1-b8-*.ivf + .md5)
#                  Argon: https://aomedia.org/ (registration)
#
# Note on hashes: each suite's reference MD5 is over its own output layout (whole decoded YUV
# file, per-picture SEI, libvpx per-frame). codec-bench's per-frame MD5 is its own convention,
# so compare backends with each other in the report; wiring a suite's native MD5 check is a
# per-suite job (see docs/DESIGN.md "Conformance hashes").
set -euo pipefail
cd "$(dirname "$0")/.."
D=corpus/files/conformance
mkdir -p "$D"/{h264,hevc,vp9,av1}
case "${1:-}" in
  vp9)
    base=https://storage.googleapis.com/downloads.webmproject.org/test_data/libvpx
    for f in vp90-2-00-quantizer-00.webm vp90-2-02-size-08x08.webm vp90-2-03-deltaq.webm vp90-2-08-tile_1x2.webm vp90-2-10-show-existing-frame.webm; do
      [ -s "$D/vp9/$f" ] || curl -fsSL -o "$D/vp9/$f" "$base/$f"
      [ -s "$D/vp9/$f.md5" ] || curl -fsSL -o "$D/vp9/$f.md5" "$base/$f.md5"
    done ;;
  av1)
    base=https://storage.googleapis.com/aom-test-data
    for f in av1-1-b8-01-size-16x16.ivf av1-1-b8-02-allintra.ivf av1-1-b8-03-sizeup.mkv; do
      [ -s "$D/av1/$f" ] || curl -fsSL -o "$D/av1/$f" "$base/$f"
      [ -s "$D/av1/$f.md5" ] || curl -fsSL -o "$D/av1/$f.md5" "$base/$f.md5"
    done ;;
  h264|hevc)
    echo "The ITU FTP listings change; browse the URL above, pick streams (e.g. BA1_Sony_D.zip, CABA1_SVA_B.zip for H.264), unzip into $D/$1/, add manifest entries." ;;
  *) echo "usage: $0 {vp9|av1|h264|hevc}"; exit 2 ;;
esac
echo "done: $D"
