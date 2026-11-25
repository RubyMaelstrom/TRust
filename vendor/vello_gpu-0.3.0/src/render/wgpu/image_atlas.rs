// Copyright 2026 the Vello Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Texture backing for the logical image atlas pages.

use alloc::{vec, vec::Vec};
use vello_common::{
    geometry::{RectU16, SizeU16},
    image_cache::{ImageCache, ImageResource},
    multi_atlas::{AtlasConfig, AtlasId},
};
use wgpu::{
    CommandEncoder, Device, Extent3d, TexelCopyTextureInfo, Texture, TextureView,
    TextureViewDescriptor,
};

#[derive(Debug)]
pub(super) struct ImageAtlases {
    limit: SizeU16,
    textures: Vec<Option<AtlasTexture>>,
    adaptive: Option<Adaptive>,
    retired_textures: bool,
}

#[derive(Debug)]
struct Adaptive {
    removed: bool,
    previous_transients: Vec<RectU16>,
    transients: Vec<RectU16>,
    occupied: Vec<RectU16>,
}

#[derive(Debug)]
pub(super) struct AtlasTexture {
    pub(super) texture: Texture,
    pub(super) view: TextureView,
    bounds: RectU16,
}

impl ImageAtlases {
    pub(super) fn new(device: &Device, config: &AtlasConfig, adaptive: bool) -> Self {
        let mut atlases = Self {
            limit: config.atlas_size.into(),
            textures: Vec::new(),
            retired_textures: false,
            adaptive: adaptive.then(|| Adaptive {
                removed: false,
                previous_transients: vec![RectU16::INVERTED; config.max_atlases],
                transients: vec![RectU16::INVERTED; config.max_atlases],
                occupied: vec![RectU16::INVERTED; config.max_atlases],
            }),
        };
        if !adaptive {
            atlases.allocate_fixed(device, config.initial_atlas_count);
        }
        atlases
    }

    pub(super) fn get(&self, id: AtlasId) -> &AtlasTexture {
        self.textures[id.as_u32() as usize]
            .as_ref()
            .expect("image atlas has no texture backing")
    }

    /// The separately submitted vector glyph render passes use fixed pages.
    pub(super) fn allocate_fixed(&mut self, device: &Device, count: usize) -> bool {
        if self.adaptive.is_some() {
            return false;
        }
        let bounds = RectU16::new(0, 0, self.limit.width(), self.limit.height());
        while self.textures.len() < count {
            self.textures.push(Some(AtlasTexture::new(device, bounds)));
        }
        true
    }

    pub(super) fn ensure(
        &mut self,
        device: &Device,
        encoder: &mut CommandEncoder,
        image: &ImageResource,
    ) -> &AtlasTexture {
        let index = image.atlas_id.as_u32() as usize;
        if self.allocate_fixed(device, index + 1) {
            return self.get(image.atlas_id);
        }
        if self.textures.len() <= index {
            self.textures.resize_with(index + 1, || None);
        }
        let occupied = image_bounds(image);
        let atlas = &mut self.textures[index];
        if let Some(atlas) = atlas {
            let mut union = atlas.bounds;
            union.union(occupied);
            if union != atlas.bounds {
                atlas.resize(device, encoder, backing_bounds(union, self.limit));
                self.retired_textures = true;
            }
        } else {
            *atlas = Some(AtlasTexture::new(
                device,
                backing_bounds(occupied, self.limit),
            ));
        }
        self.get(image.atlas_id)
    }

    pub(super) fn take_retired_textures(&mut self) -> bool {
        core::mem::take(&mut self.retired_textures)
    }

    pub(super) fn removed(&mut self) {
        if let Some(adaptive) = &mut self.adaptive {
            adaptive.removed = true;
        }
    }

    pub(super) fn track_transient(&mut self, image: &ImageResource) {
        if let Some(adaptive) = &mut self.adaptive {
            adaptive.transients[image.atlas_id.as_u32() as usize].union(image_bounds(image));
        }
    }

    pub(super) fn trim(
        &mut self,
        device: &Device,
        encoder: &mut CommandEncoder,
        images: &ImageCache,
    ) {
        let Some(adaptive) = &mut self.adaptive else {
            return;
        };
        // Bitmap glyphs and effect images release their logical slots after
        // every draw. Keep their backing for the next frame unless its
        // transient paints collectively use less space.
        let mut pending = adaptive.removed;
        for (previous, current) in adaptive
            .previous_transients
            .iter_mut()
            .zip(&mut adaptive.transients)
            .take(self.textures.len())
        {
            pending |= !previous.is_empty() && current.intersect(*previous) != *previous;
            *previous = core::mem::replace(current, RectU16::INVERTED);
        }
        if !pending {
            return;
        }
        adaptive.removed = false;
        adaptive.occupied.fill(RectU16::INVERTED);
        for image in images.allocated_images() {
            adaptive.occupied[image.atlas_id.as_u32() as usize].union(image_bounds(image));
        }
        for (slot, occupied) in self.textures.iter_mut().zip(&adaptive.occupied) {
            if occupied.is_empty() {
                self.retired_textures |= slot.take().is_some();
            } else if let Some(atlas) = slot {
                let desired = backing_bounds(*occupied, self.limit);
                // Keep one step of spare capacity to avoid repeatedly
                // reallocating around a power-of-two boundary.
                if area(atlas.bounds) > area(desired).saturating_mul(2) {
                    atlas.resize(device, encoder, desired);
                    self.retired_textures = true;
                }
            }
        }
    }
}

impl AtlasTexture {
    fn new(device: &Device, bounds: RectU16) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Atlas Texture"),
            size: texture_extent(bounds),
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = texture.create_view(&TextureViewDescriptor::default());
        Self {
            texture,
            view,
            bounds,
        }
    }

    pub(super) fn relative_offset(&self, [x, y]: [u16; 2]) -> [u16; 2] {
        [x - self.bounds.x0, y - self.bounds.y0]
    }

    fn copy_at(&self, origin: [u16; 2]) -> TexelCopyTextureInfo<'_> {
        let [x, y] = self.relative_offset(origin);
        TexelCopyTextureInfo {
            origin: wgpu::Origin3d {
                x: u32::from(x),
                y: u32::from(y),
                z: 0,
            },
            ..self.texture.as_image_copy()
        }
    }

    fn resize(&mut self, device: &Device, encoder: &mut CommandEncoder, bounds: RectU16) {
        let new = Self::new(device, bounds);
        let overlap = self.bounds.intersect(bounds);
        if !overlap.is_empty() {
            // WebGPU copyTextureToTexture validates the extent against both
            // subresources. Copy their intersection in the same command
            // stream as previous atlas writes and subsequent draws.
            let origin = [overlap.x0, overlap.y0];
            encoder.copy_texture_to_texture(
                self.copy_at(origin),
                new.copy_at(origin),
                texture_extent(overlap),
            );
        }
        *self = new;
    }
}

fn image_bounds(image: &ImageResource) -> RectU16 {
    RectU16::new(
        image.offset[0] - image.padding,
        image.offset[1] - image.padding,
        image.offset[0] + image.width + image.padding,
        image.offset[1] + image.height + image.padding,
    )
}

fn backing_bounds(occupied: RectU16, limit: SizeU16) -> RectU16 {
    // Round each dimension independently so a long narrow image stays
    // narrow. Fit the backing into the logical page even near its edges.
    const MIN_DIMENSION: u32 = 64;
    let dimension = |length: u16, max: u16| {
        let rounded = u32::from(length).max(MIN_DIMENSION).next_power_of_two();
        u16::try_from(rounded).unwrap_or(max).min(max)
    };
    let width = dimension(occupied.width(), limit.width());
    let height = dimension(occupied.height(), limit.height());
    let x0 = occupied.x0.min(limit.width() - width);
    let y0 = occupied.y0.min(limit.height() - height);
    RectU16::new(x0, y0, x0 + width, y0 + height)
}

fn texture_extent(rect: RectU16) -> Extent3d {
    Extent3d {
        width: u32::from(rect.width()),
        height: u32::from(rect.height()),
        depth_or_array_layers: 1,
    }
}

fn area(rect: RectU16) -> u32 {
    u32::from(rect.width()) * u32::from(rect.height())
}
