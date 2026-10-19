//! Images are sampled once per presentation, including its offscreen passes.
//! HTML #the-video-element represents one current video frame. Filter Effects
//! 2 #backdrop-filter-operation copies the already-painted Backdrop Root Image;
//! re-reading a live surface during that copy can both change the backdrop and
//! destroy a registration still referenced by queued draws. Share this lazy
//! snapshot between the main renderer and its effects renderer. The next
//! presentation samples new frames; SVG raster sizes remain per-use.
//! Local snapshots: WHATWG HTML e5071a20c856, CSSWG 81c27f686901.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::{ImageHandle, ImageResource, Scene};
use crate::media::frame::VideoFrame;

#[derive(Clone)]
enum Sample {
    Image(ImageResource, u64),
    Video(Arc<VideoFrame>, u64),
}

#[derive(Clone, Default)]
pub(super) struct FrameImages(Arc<Mutex<HashMap<ImageHandle, Option<Sample>>>>);

impl FrameImages {
    fn sample(&self, scene: &Scene, handle: ImageHandle) -> Option<Sample> {
        self.0
            .lock()
            .unwrap()
            .entry(handle)
            .or_insert_with(|| {
                if let Some(id) = handle.video_surface() {
                    let (frame, serial) = crate::media::player::surface(id)?.current()?;
                    Some(Sample::Video(frame, serial))
                } else if let Some(canvas) = scene.canvas_images.get(&handle) {
                    Some(Sample::Image(canvas.image.clone(), canvas.generation))
                } else {
                    let (image, revision) = scene.image_store.get_with_revision(handle)?;
                    Some(Sample::Image(image, revision))
                }
            })
            .clone()
    }

    pub fn image(&self, scene: &Scene, handle: ImageHandle) -> Option<(ImageResource, u64)> {
        match self.sample(scene, handle)? {
            Sample::Image(image, revision) => Some((image, revision)),
            Sample::Video(frame, serial) => {
                super::video_frame_pixels(handle.video_surface()?, &frame, serial)
                    .map(|image| (image, serial))
            }
        }
    }

    pub fn video(&self, scene: &Scene, handle: ImageHandle) -> Option<(Arc<VideoFrame>, u64)> {
        match self.sample(scene, handle)? {
            Sample::Video(frame, serial) => Some((frame, serial)),
            Sample::Image(..) => None,
        }
    }
}
