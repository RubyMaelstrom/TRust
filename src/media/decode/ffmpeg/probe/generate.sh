#!/bin/sh
# Regenerates the keyframes that probe hardware decoding (`hardware.rs`) with
# the ffmpeg command line tool (FFmpeg 9.0.2 with libx264, libx265, libvpx and
# libaom): one flat grey 256x256 8-bit 4:2:0 keyframe per format, large
# enough for every decoder's minimum size (NVDEC: 144x144 for HEVC). H.264
# and HEVC are Annex B access units with their parameter sets; VP9 is one
# frame and AV1 one temporal unit (with its sequence header), taken from IVF.
set -e
cd "$(dirname "$0")"
FF="ffmpeg -hide_banner -loglevel error -y -bitexact"
V="-f lavfi -i color=c=gray:size=256x256:rate=1 -frames:v 1 -pix_fmt yuv420p"
$FF $V -c:v libx264 -profile:v high -f h264 h264.bin
$FF $V -c:v libx265 -x265-params log-level=error -f hevc hevc.bin
# IVF: a 32-byte file header, then a 12-byte header before each frame.
ivf_frame() {
    tail -c +45 "$1" > "$2"
    rm "$1"
}
$FF $V -c:v libvpx-vp9 -f ivf vp9.ivf && ivf_frame vp9.ivf vp9.bin
$FF $V -c:v libaom-av1 -cpu-used 8 -f ivf av1.ivf && ivf_frame av1.ivf av1.bin
