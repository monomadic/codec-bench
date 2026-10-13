#!/usr/bin/env bash
# Generate the "fixtures" and "malformed" sets with ffmpeg. Deterministic sources (testsrc2 +
# seeded noise), long GOPs, no audio. Re-run freely; existing files are kept unless FORCE=1.
set -euo pipefail
cd "$(dirname "$0")/.."
F=corpus/files/fixtures
M=corpus/files/malformed
mkdir -p "$F" "$M"
SECS=${SECS:-20}
FF="ffmpeg -hide_banner -loglevel error -y -nostdin"

have() { ffmpeg -hide_banner -encoders 2>/dev/null | grep -q " $1 "; }
need() { [ "${FORCE:-0}" = 1 ] || [ ! -s "$1" ]; }

# A busy, deterministic source: testsrc2 with seeded noise so inter prediction has work to do.
src() { # width height fps
  echo "testsrc2=size=$1x$2:rate=$3:duration=$SECS,noise=alls=12:allf=t+u:all_seed=42,format=yuv420p"
}

if need $F/h264_1080p_gop250.mp4; then echo "h264 1080p"; $FF -f lavfi -i "$(src 1920 1080 30)" \
  -c:v libx264 -preset medium -profile:v high -g 250 -keyint_min 250 -bf 3 -pix_fmt yuv420p -movflags +faststart $F/h264_1080p_gop250.mp4; fi
if need $F/h264_2160p_gop250.mp4; then echo "h264 2160p"; $FF -f lavfi -i "$(src 3840 2160 30)" \
  -c:v libx264 -preset fast -profile:v high -g 250 -keyint_min 250 -bf 3 -pix_fmt yuv420p -movflags +faststart $F/h264_2160p_gop250.mp4; fi
if need $F/h264_1080p_intra.mp4; then echo "h264 intra"; $FF -f lavfi -i "$(src 1920 1080 30)" \
  -c:v libx264 -preset medium -g 1 -pix_fmt yuv420p $F/h264_1080p_intra.mp4; fi
# Every codec at 1080p and 2160p: libraries are mostly one or the other.
if have libx265; then
  for R in "1080p 1920 1080" "2160p 3840 2160"; do set -- $R
    if need $F/hevc_${1}_gop250.mp4; then echo "hevc $1"; $FF -f lavfi -i "$(src $2 $3 30)" \
      -c:v libx265 -preset medium -x265-params "keyint=250:min-keyint=250:log-level=error" -pix_fmt yuv420p -tag:v hvc1 $F/hevc_${1}_gop250.mp4; fi
    if need $F/hevc_${1}10_gop250.mp4; then echo "hevc ${1}10"; $FF -f lavfi -i "$(src $2 $3 30)" \
      -c:v libx265 -preset medium -x265-params "keyint=250:min-keyint=250:log-level=error" -pix_fmt yuv420p10le -tag:v hvc1 $F/hevc_${1}10_gop250.mp4; fi
  done
else echo "skip hevc: no libx265"; fi
if have libvpx-vp9; then
  for R in "1080p 1920 1080 4M" "2160p 3840 2160 16M"; do set -- $R
    if need $F/vp9_$1.webm; then echo "vp9 $1"; $FF -f lavfi -i "$(src $2 $3 30)" \
      -c:v libvpx-vp9 -b:v $4 -deadline good -cpu-used 4 -row-mt 1 -g 250 -pix_fmt yuv420p $F/vp9_$1.webm; fi
  done
else echo "skip vp9: no libvpx-vp9"; fi
for R in "1080p 1920 1080" "2160p 3840 2160"; do set -- $R
  if need $F/av1_$1.mp4; then
    if have libsvtav1; then echo "av1 $1 (svt)"; $FF -f lavfi -i "$(src $2 $3 30)" -c:v libsvtav1 -preset 8 -g 250 -pix_fmt yuv420p $F/av1_$1.mp4
    elif have libaom-av1; then echo "av1 $1 (aom)"; $FF -f lavfi -i "$(src $2 $3 30)" -c:v libaom-av1 -cpu-used 8 -g 250 -pix_fmt yuv420p $F/av1_$1.mp4
    else echo "skip av1: no encoder"; fi
  fi
done
if need $F/prores422_1080p.mov; then echo "prores 422 1080p"; $FF -f lavfi -i "$(src 1920 1080 30)" \
  -c:v prores_ks -profile:v 2 -pix_fmt yuv422p10le $F/prores422_1080p.mov; fi
# 2160p ProRes is ~75 MB/s: 8 seconds, not $SECS.
if need $F/prores422_2160p.mov; then echo "prores 422 2160p"; $FF -f lavfi -i "testsrc2=size=3840x2160:rate=30:duration=8,noise=alls=12:allf=t+u:all_seed=42,format=yuv420p" \
  -c:v prores_ks -profile:v 2 -pix_fmt yuv422p10le $F/prores422_2160p.mov; fi
if need $F/h264_720p_vfr.mp4; then echo "h264 vfr"; $FF -f lavfi -i "$(src 1280 720 30)" \
  -vf "setpts=PTS+random(1)*0.02/TB" -fps_mode passthrough -c:v libx264 -preset fast -g 60 -pix_fmt yuv420p $F/h264_720p_vfr.mp4; fi

# malformed: derived from the first fixture
if need $M/h264_1080p_truncated.mp4; then
  echo "truncated"; SIZE=$(stat -f%z $F/h264_1080p_gop250.mp4 2>/dev/null || stat -c%s $F/h264_1080p_gop250.mp4)
  head -c $((SIZE * 6 / 10)) $F/h264_1080p_gop250.mp4 > $M/h264_1080p_truncated.mp4
fi
if need $M/h264_1080p_bitflips.mp4; then
  echo "bitflips"; cp $F/h264_1080p_gop250.mp4 $M/h264_1080p_bitflips.mp4
  SIZE=$(stat -f%z $M/h264_1080p_bitflips.mp4 2>/dev/null || stat -c%s $M/h264_1080p_bitflips.mp4)
  # deterministic positions past the first 64 KiB (faststart puts moov there), 200 bytes
  python3 - "$M/h264_1080p_bitflips.mp4" "$SIZE" <<'PY'
import sys, random
p, size = sys.argv[1], int(sys.argv[2]); r = random.Random(42)
with open(p, 'r+b') as f:
    for _ in range(200):
        f.seek(r.randrange(65536, size)); f.write(bytes([r.randrange(256)]))
PY
fi
echo "fixtures in $F; malformed in $M"
