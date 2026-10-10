#!/bin/sh
# Regenerates the synthetic test streams (src/media/decode/ffmpeg/testdata) and their reference checksums with
# the ffmpeg command line tool (FFmpeg 9.0.2 with libx264, libx265, libvpx,
# libaom, libmp3lame). The streams contain only lavfi test patterns / sine
# tones. For each video stream, NAME.md5 is `ffmpeg -f framemd5` of the
# stream decoded by FFmpeg itself, in the stream's native pixel format.
set -e
cd "$(dirname "$0")"
FF="ffmpeg -hide_banner -loglevel error -y -bitexact"
V="-f lavfi -i testsrc2=size=160x90:rate=10 -frames:v 10"
A="-f lavfi -i sine=frequency=440:sample_rate=48000:duration=1"

# H.264 / HEVC in Annex B with in-band parameter sets.
$FF $V -c:v libx264 -profile:v baseline -pix_fmt yuv420p -bf 0 -g 5 -f h264 h264_baseline.h264
$FF $V -c:v libx264 -profile:v high -pix_fmt yuv420p -bf 2 -g 10 -x264-params ref=3 -f h264 h264_high_bframes.h264
$FF $V -c:v libx264 -profile:v high10 -pix_fmt yuv420p10le -bf 2 -g 10 -f h264 h264_high10.h264
$FF $V -c:v libx264 -profile:v high422 -pix_fmt yuv422p -bf 0 -g 10 -f h264 h264_high422.h264
$FF $V -c:v libx264 -profile:v high444 -pix_fmt yuv444p -bf 0 -g 10 -f h264 h264_high444.h264
$FF $V -c:v libx264 -profile:v high422 -pix_fmt yuv422p10le -bf 0 -g 10 -f h264 h264_high422_10.h264
$FF $V -c:v libx264 -profile:v high444 -pix_fmt yuv444p10le -bf 0 -g 10 -f h264 h264_high444_10.h264
# Monochrome (chroma_format_idc 0) decodes to full-range yuvj420p with flat chroma.
$FF $V -c:v libx264 -profile:v high -vf format=gray -pix_fmt gray -bf 0 -g 10 -f h264 h264_monochrome.h264
$FF $V -c:v libx264 -profile:v high -pix_fmt yuvj420p -bf 0 -g 10 -f h264 h264_fullrange.h264
$FF $V -c:v libx265 -pix_fmt yuv420p -x265-params log-level=error:bframes=2:keyint=10 -f hevc hevc_main.hevc
$FF $V -c:v libx265 -pix_fmt gray -x265-params log-level=error:bframes=0:keyint=10 -f hevc hevc_gray.hevc

# VP8, VP9, AV1 in IVF.
$FF $V -c:v libvpx -pix_fmt yuv420p -g 5 -f ivf vp8.ivf
$FF $V -c:v libvpx-vp9 -pix_fmt yuv420p -g 5 -f ivf vp9.ivf
$FF $V -c:v libvpx-vp9 -pix_fmt yuv444p -g 5 -f ivf vp9_444.ivf
$FF $V -c:v libaom-av1 -cpu-used 8 -pix_fmt yuv420p -g 5 -f ivf av1.ivf
$FF $V -c:v libaom-av1 -cpu-used 8 -pix_fmt yuv420p10le -g 5 -f ivf av1_10bit.ivf
$FF $V -c:v libaom-av1 -cpu-used 8 -pix_fmt yuv420p12le -g 5 -f ivf av1_12bit.ivf

for f in h264_*.h264 hevc_*.hevc vp*.ivf av1*.ivf; do
    ffmpeg -hide_banner -loglevel error -y -i "$f" -f framemd5 - | grep -v '^#' > "${f%.*}.md5"
done

# Audio: left channel 440 Hz, right channel 880 Hz (checks channel order).
STEREO="-f lavfi -i sine=frequency=440:sample_rate=48000:duration=1 -f lavfi -i sine=frequency=880:sample_rate=48000:duration=1 -filter_complex [0:a][1:a]join=inputs=2:channel_layout=stereo"
$FF $STEREO -c:a aac -b:a 96k -f adts aac_stereo.adts
$FF $A -c:a aac -b:a 64k -f adts aac_mono.adts
$FF $STEREO -c:a ac3 -b:a 192k -f ac3 ac3_stereo.ac3
$FF $STEREO -c:a eac3 -b:a 192k -f eac3 eac3_stereo.eac3
$FF $STEREO -c:a libmp3lame -b:a 128k -write_xing 0 -id3v2_version 0 -f mp3 mp3_stereo.mp3
