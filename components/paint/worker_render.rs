/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! CPU rasterizer for the Worker WASM target. The Worker has no WebRender
//! renderer, so this interprets the display lists captured in
//! [`crate::worker_frame`] with `vello_cpu`, the rasterizer canvas 2D uses.
//!
//! Supported: the spatial tree (2D transforms, scroll offsets; sticky frames at
//! their static positions), rect and rounded-rect clips, rectangles, text,
//! images (stretched and repeated), borders (all styles, radii for uniform
//! solid borders), lines, linear/radial gradients, box and text shadows,
//! stacking-context opacity, blend modes and filters, and iframes. 3D
//! transforms are drawn flattened.

use std::sync::Arc;

use euclid::default::Size2D;
use paint_api::display_list::{ScrollTree, SpatialTreeNodeInfo};
use vello_common::filter_effects::{EdgeMode, Filter, FilterFunction, FilterPrimitive};
use vello_cpu::kurbo::{self, Affine, BezPath, Cap, Rect, RoundedRect, RoundedRectRadii, Shape, Stroke};
use vello_cpu::peniko::{
    self, BlendMode, Color, ColorStop, Compose, Extend, Gradient, ImageQuality, ImageSampler, Mix,
};
use pixels::{EncodedImageType, Snapshot, SnapshotAlphaMode, SnapshotPixelFormat};
use rustc_hash::FxHashMap;
use vello_cpu::{Glyph, Mask, Pixmap, RenderContext, RenderSettings, Resources};
use webrender_api::units::{LayoutRect, LayoutSize, LayoutTransform};
use webrender_api::{
    AlphaType, BorderDetails, BorderRadius, BorderSide, BorderStyle, BoxShadowClipMode,
    BuiltDisplayList, ClipChainId, ClipId, ClipMode, ColorF, DisplayItem, ExtendMode, FilterOp,
    GradientStop, ImageFormat, ImageKey, MixBlendMode, PipelineId, PropertyBinding,
    ReferenceFrameKind, ReferenceTransformBinding, Shadow, SpatialTreeItem,
};

use crate::worker_frame::{self, WorkerResources};

/// Largest rendered dimension in pixels (vello_cpu uses 16-bit sizes).
const MAX_DIMENSION: u32 = 16_384;
/// Iframes nested deeper than this are not drawn.
const MAX_IFRAME_DEPTH: usize = 8;
/// Largest full-page capture in pixels (32 MiB as RGBA); Worker isolates have
/// 128 MB of memory in total.
const MAX_FULL_PAGE_PIXELS: u32 = 8 * 1024 * 1024;

/// Render the latest top-level document to a PNG: the viewport, or with
/// `full_page` the whole document from its top (height capped for memory).
pub(crate) fn render_png(full_page: bool) -> Result<Vec<u8>, String> {
    let (width, height, pixmap) = worker_frame::with_display_lists(|lists| {
        let root = worker_frame::root_pipeline(lists).ok_or("No page has been rendered yet")?;
        let info = &lists[&root].info;
        let scale = info.viewport_details.hidpi_scale_factor.get();
        let mut size = info.viewport_details.size;
        if full_page {
            if let Some((_, content)) = scroll_nodes(&info.scroll_tree).get(&1) {
                size.width = size.width.max(content.width);
                size.height = size.height.max(content.height);
            }
        }
        let size = size * scale;
        let width = (size.width.ceil() as u32).clamp(1, MAX_DIMENSION);
        let mut height = (size.height.ceil() as u32).clamp(1, MAX_DIMENSION);
        if full_page {
            height = height.min((MAX_FULL_PAGE_PIXELS / width).max(1));
        }
        let pixmap = worker_frame::with_resources(|resources| {
            let mut renderer = Renderer::new(width as u16, height as u16, lists, resources);
            renderer.full_page = full_page;
            renderer.draw_pipeline(root, Affine::scale(scale as f64), 0);
            renderer.finish()
        });
        Ok::<_, String>((width, height, pixmap))
    })?;

    let mut snapshot = Snapshot::from_vec(
        Size2D::new(width, height),
        SnapshotPixelFormat::RGBA,
        SnapshotAlphaMode::Transparent {
            premultiplied: true,
        },
        pixmap.data_as_u8_slice().to_vec(),
    );
    let mut png = Vec::new();
    snapshot
        .encode_for_mime_type(&EncodedImageType::Png, None, &mut png)
        .map_err(|error| format!("PNG encoding failed: {error:?}"))?;
    Ok(png)
}

#[derive(Clone)]
enum ClipShape {
    Rect(LayoutRect),
    RoundedRect(LayoutRect, BorderRadius),
    /// An image whose alpha (or, with `luminance`, its luminance) masks
    /// content (`mask-image`); applied only to stacking contexts, as a mask
    /// layer. `tile_size` may be smaller than `rect` when `mask-repeat`
    /// tiles the image across it.
    ImageMask {
        key: ImageKey,
        rect: LayoutRect,
        tile_size: LayoutSize,
        luminance: bool,
    },
}

#[derive(Clone)]
struct Clip {
    spatial: (PipelineId, usize),
    shape: ClipShape,
}

struct Renderer<'a> {
    context: RenderContext,
    resources: Resources,
    lists: &'a FxHashMap<PipelineId, worker_frame::CapturedDisplayList>,
    frame_resources: &'a WorkerResources,
    /// Transform of each spatial node to device pixels.
    spatial_nodes: FxHashMap<(PipelineId, usize), Affine>,
    clips: FxHashMap<ClipId, Clip>,
    clip_chains: FxHashMap<ClipChainId, (Option<ClipChainId>, Vec<ClipId>)>,
    image_cache: FxHashMap<ImageKey, Option<Arc<Pixmap>>>,
    device_rect: Rect,
    /// Text shadows in effect (between PushShadow and PopAllShadows).
    shadows: Vec<Shadow>,
    /// Contexts suspended while a stacking context with color filters draws
    /// into its own context, with that context's color matrix.
    offscreen: Vec<(RenderContext, ColorMatrix)>,
    /// The transform of the item being drawn.
    item_transform: Affine,
    /// Render the whole document: the root scroll frame is drawn unscrolled.
    full_page: bool,
}

/// A CSS/SVG color matrix: 4 rows of 5 (the last column is an offset, 0..1).
type ColorMatrix = [f32; 20];

/// What a stacking context pushed, to undo it when it ends.
struct StackingEntry {
    layers: usize,
    offscreen: bool,
}

impl<'a> Renderer<'a> {
    fn new(
        width: u16,
        height: u16,
        lists: &'a FxHashMap<PipelineId, worker_frame::CapturedDisplayList>,
        frame_resources: &'a WorkerResources,
    ) -> Self {
        // Single-threaded: the Worker has one thread, and vello_cpu's default
        // settings query the thread count, which panics on wasm32.
        let settings = RenderSettings {
            level: vello_cpu::Level::try_detect().unwrap_or(vello_cpu::Level::baseline()),
            num_threads: 0,
        };
        let mut context = RenderContext::new_with(width, height, settings);
        // Browsers paint an opaque white canvas behind the root element.
        context.set_paint(Color::WHITE);
        context.fill_rect(&Rect::new(0.0, 0.0, width as f64, height as f64));
        Self {
            context,
            resources: Resources::new(),
            lists,
            frame_resources,
            spatial_nodes: FxHashMap::default(),
            clips: FxHashMap::default(),
            clip_chains: FxHashMap::default(),
            image_cache: FxHashMap::default(),
            device_rect: Rect::new(0.0, 0.0, width as f64, height as f64),
            shadows: Vec::new(),
            offscreen: Vec::new(),
            item_transform: Affine::IDENTITY,
            full_page: false,
        }
    }

    fn finish(mut self) -> Pixmap {
        let mut pixmap = Pixmap::new(self.context.width(), self.context.height());
        self.context.flush();
        self.context.render(&mut pixmap, &mut self.resources);
        pixmap
    }

    fn node(&self, pipeline: PipelineId, index: usize) -> Affine {
        self.spatial_nodes
            .get(&(pipeline, index))
            .copied()
            .unwrap_or(Affine::IDENTITY)
    }

    fn build_spatial_tree(
        &mut self,
        pipeline: PipelineId,
        list: &BuiltDisplayList,
        scroll_tree: &ScrollTree,
        base: Affine,
        is_root_document: bool,
    ) {
        // Scroll offsets by spatial node index, from Servo's scroll tree (which
        // also covers the implicit root scroll node that scrolls the page).
        let scrolling = scroll_nodes(scroll_tree);
        let full_page = self.full_page && is_root_document;
        let scrolled = |index: usize, parent: Affine| {
            let offset = match scrolling.get(&index) {
                // A full-page capture shows the document from its top.
                Some(_) if full_page && index == 1 => Default::default(),
                Some((offset, _)) => *offset,
                None => Default::default(),
            };
            parent * Affine::translate((-offset.x as f64, -offset.y as f64))
        };
        // Nodes 0 and 1 are the implicit root reference frame and root scroll node.
        self.spatial_nodes.insert((pipeline, 0), base);
        self.spatial_nodes.insert((pipeline, 1), scrolled(1, base));
        let mut entries = Vec::new();
        list.iter_spatial_tree(|item| entries.push(*item));
        for item in entries {
            match item {
                SpatialTreeItem::ReferenceFrame(descriptor) => {
                    let parent = self.node(pipeline, descriptor.parent_spatial_id.0);
                    let transform = match descriptor.reference_frame.kind {
                        // Perspective is a 3D effect; draw its content flat.
                        ReferenceFrameKind::Perspective { .. } => Affine::IDENTITY,
                        _ => reference_transform(&descriptor.reference_frame.transform),
                    };
                    let origin = Affine::translate((
                        descriptor.origin.x as f64,
                        descriptor.origin.y as f64,
                    ));
                    self.spatial_nodes.insert(
                        (pipeline, descriptor.reference_frame.id.0),
                        parent * origin * transform,
                    );
                },
                SpatialTreeItem::ScrollFrame(descriptor) => {
                    let parent = self.node(pipeline, descriptor.parent_space.0);
                    let index = descriptor.scroll_frame_id.0;
                    self.spatial_nodes.insert((pipeline, index), scrolled(index, parent));
                },
                SpatialTreeItem::StickyFrame(descriptor) => {
                    let parent = self.node(pipeline, descriptor.parent_spatial_id.0);
                    self.spatial_nodes.insert((pipeline, descriptor.id.0), parent);
                },
                SpatialTreeItem::Invalid => {},
            }
        }
    }

    fn draw_pipeline(&mut self, pipeline: PipelineId, base: Affine, depth: usize) {
        let Some(captured) = self.lists.get(&pipeline) else {
            return;
        };
        let list = &captured.display_list;
        self.build_spatial_tree(pipeline, list, &captured.info.scroll_tree, base, depth == 0);

        let mut stacking: Vec<StackingEntry> = Vec::new();
        let mut iter = list.iter();
        while let Some(item) = iter.next() {
            match item.item() {
                DisplayItem::RectClip(clip) => {
                    self.clips.insert(
                        clip.id,
                        Clip {
                            spatial: (pipeline, clip.spatial_id.0),
                            shape: ClipShape::Rect(clip.clip_rect),
                        },
                    );
                },
                DisplayItem::RoundedRectClip(clip) => {
                    // A clip-out region cannot be drawn as a clip layer; ignore it.
                    if clip.clip.mode == ClipMode::Clip {
                        self.clips.insert(
                            clip.id,
                            Clip {
                                spatial: (pipeline, clip.spatial_id.0),
                                shape: ClipShape::RoundedRect(clip.clip.rect, clip.clip.radii),
                            },
                        );
                    }
                },
                DisplayItem::ImageMaskClip(clip) => {
                    let rect = clip.image_mask.rect;
                    // Zero (WebRender's `Default`) means "one tile spanning `rect`".
                    let tile_size = if clip.image_mask.tile_size.width > 0.0 &&
                        clip.image_mask.tile_size.height > 0.0
                    {
                        clip.image_mask.tile_size
                    } else {
                        rect.size()
                    };
                    self.clips.insert(
                        clip.id,
                        Clip {
                            spatial: (pipeline, clip.spatial_id.0),
                            shape: ClipShape::ImageMask {
                                key: clip.image_mask.image,
                                rect,
                                tile_size,
                                luminance: clip.image_mask.luminance,
                            },
                        },
                    );
                },
                DisplayItem::ClipChain(chain) => {
                    let clip_ids = item.clip_chain_items().iter().collect();
                    self.clip_chains.insert(chain.id, (chain.parent, clip_ids));
                },
                DisplayItem::PushStackingContext(push) => {
                    let filters: Vec<FilterOp> = item.filters().iter().collect();
                    let transform = self.node(pipeline, push.spatial_id.0);
                    let masks = self.image_masks(push.stacking_context.clip_chain_id);
                    let entry = self.push_stacking_context(
                        push.stacking_context.mix_blend_mode,
                        &filters,
                        transform,
                        masks,
                    );
                    stacking.push(entry);
                },
                DisplayItem::PopStackingContext => {
                    if let Some(entry) = stacking.pop() {
                        self.pop_stacking_context(entry);
                    }
                },
                DisplayItem::PushShadow(push) => self.shadows.push(push.shadow),
                DisplayItem::PopAllShadows => self.shadows.clear(),
                DisplayItem::BoxShadow(shadow) => {
                    let shadow = *shadow;
                    let extent = shadow_extent(&shadow);
                    self.draw(pipeline, &shadow.common, extent, |renderer| {
                        renderer.fill_box_shadow(&shadow);
                    });
                },
                DisplayItem::Rectangle(rectangle) => {
                    let color = binding_value(&rectangle.color);
                    self.draw(pipeline, &rectangle.common, rectangle.bounds, |renderer| {
                        renderer.context.set_paint(color_of(color));
                        renderer.context.fill_rect(&rect_of(rectangle.bounds));
                    });
                },
                DisplayItem::Text(text) => {
                    let Some(instance) = self.frame_resources.font_instances.get(&text.font_key)
                    else {
                        continue;
                    };
                    let Some(font) = self.frame_resources.fonts.get(&instance.font_key) else {
                        continue;
                    };
                    let size = instance.size;
                    let glyphs: Vec<Glyph> = item
                        .glyphs()
                        .iter()
                        .map(|glyph| Glyph {
                            id: glyph.index,
                            x: glyph.point.x,
                            y: glyph.point.y,
                        })
                        .collect();
                    let color = color_of(text.color);
                    let bounds = inflate_for_shadows(text.bounds, &self.shadows);
                    self.draw(pipeline, &text.common, bounds, |renderer| {
                        renderer.with_shadows(color, |renderer, color| {
                            renderer.context.set_paint(color);
                            renderer
                                .context
                                .glyph_run(&mut renderer.resources, font)
                                .font_size(size)
                                .fill_glyphs(glyphs.iter().copied());
                        });
                    });
                },
                DisplayItem::Image(image) => {
                    let Some(pixmap) = self.image(image.image_key, image.alpha_type) else {
                        continue;
                    };
                    let bounds = image.bounds;
                    self.draw(pipeline, &image.common, bounds, |renderer| {
                        renderer.fill_image(pixmap, bounds, bounds.size(), Extend::Pad);
                    });
                },
                DisplayItem::RepeatingImage(image) => {
                    let Some(pixmap) = self.image(image.image_key, image.alpha_type) else {
                        continue;
                    };
                    let (bounds, stretch) = (image.bounds, image.stretch_size);
                    self.draw(pipeline, &image.common, bounds, |renderer| {
                        renderer.fill_image(pixmap, bounds, stretch, Extend::Repeat);
                    });
                },
                DisplayItem::Border(border) => {
                    let BorderDetails::Normal(details) = border.details else {
                        continue;
                    };
                    let (bounds, widths) = (border.bounds, border.widths);
                    self.draw(pipeline, &border.common, bounds, |renderer| {
                        renderer.fill_border(bounds, widths, &details);
                    });
                },
                DisplayItem::Line(line) => {
                    let (area, color) = (line.area, color_of(line.color));
                    let bounds = inflate_for_shadows(area, &self.shadows);
                    self.draw(pipeline, &line.common, bounds, |renderer| {
                        renderer.with_shadows(color, |renderer, color| {
                            renderer.context.set_paint(color);
                            renderer.context.fill_rect(&rect_of(area));
                        });
                    });
                },
                DisplayItem::Gradient(gradient) => {
                    let stops = stops_of(item.gradient_stops().iter());
                    let start = point_in(gradient.bounds, gradient.gradient.start_point);
                    let end = point_in(gradient.bounds, gradient.gradient.end_point);
                    let mut paint = Gradient::new_linear(start, end);
                    paint.stops = stops;
                    paint.extend = extend_of(gradient.gradient.extend_mode);
                    let bounds = gradient.bounds;
                    self.draw(pipeline, &gradient.common, bounds, |renderer| {
                        renderer.context.set_paint(paint);
                        renderer.context.fill_rect(&rect_of(bounds));
                    });
                },
                DisplayItem::RadialGradient(gradient) => {
                    let spec = gradient.gradient;
                    if spec.radius.width <= 0.0 || spec.radius.height <= 0.0 {
                        continue;
                    }
                    // Draw a circle of the horizontal radius, scaled to the ellipse.
                    let center = point_in(gradient.bounds, spec.center);
                    let radius = spec.radius.width;
                    let mut paint = Gradient::new_two_point_radial(
                        center,
                        radius * spec.start_offset,
                        center,
                        radius * spec.end_offset,
                    );
                    paint.stops = stops_of(item.gradient_stops().iter());
                    paint.extend = extend_of(spec.extend_mode);
                    let squash = Affine::translate(center.to_vec2()) *
                        Affine::scale_non_uniform(1.0, (spec.radius.height / radius) as f64) *
                        Affine::translate(-center.to_vec2());
                    let bounds = gradient.bounds;
                    self.draw(pipeline, &gradient.common, bounds, |renderer| {
                        renderer.context.set_paint(paint);
                        renderer.context.set_paint_transform(squash);
                        renderer.context.fill_rect(&rect_of(bounds));
                        renderer.context.reset_paint_transform();
                    });
                },
                DisplayItem::Iframe(iframe) => {
                    if depth >= MAX_IFRAME_DEPTH {
                        continue;
                    }
                    let parent = self.node(pipeline, iframe.space_and_clip.spatial_id.0);
                    let clips = self.clip_layers(
                        pipeline,
                        iframe.space_and_clip.clip_chain_id,
                        Some(iframe.clip_rect),
                        iframe.space_and_clip.spatial_id.0,
                        iframe.bounds,
                    );
                    let origin = Affine::translate((
                        iframe.bounds.min.x as f64,
                        iframe.bounds.min.y as f64,
                    ));
                    self.draw_pipeline(iframe.pipeline_id, parent * origin, depth + 1);
                    for _ in 0..clips {
                        self.context.pop_layer();
                    }
                },
                _ => {},
            }
        }
        while let Some(entry) = stacking.pop() {
            self.pop_stacking_context(entry);
        }
        self.shadows.clear();
    }

    /// Draw an item in its spatial node's coordinate space, inside its clips.
    fn draw(
        &mut self,
        pipeline: PipelineId,
        common: &webrender_api::CommonItemProperties,
        bounds: LayoutRect,
        paint: impl FnOnce(&mut Self),
    ) {
        let transform = self.node(pipeline, common.spatial_id.0);
        let device_bounds = transform.transform_rect_bbox(rect_of(bounds));
        if device_bounds.intersect(self.device_rect).is_zero_area() {
            return;
        }
        let clips = self.clip_layers(
            pipeline,
            common.clip_chain_id,
            Some(common.clip_rect),
            common.spatial_id.0,
            bounds,
        );
        self.context.set_transform(transform);
        self.item_transform = transform;
        paint(self);
        for _ in 0..clips {
            self.context.pop_layer();
        }
    }

    /// Push clip layers for a clip chain plus an optional rect in the item's
    /// space. Clips that already contain the item are skipped. Returns the
    /// number of layers pushed.
    fn clip_layers(
        &mut self,
        pipeline: PipelineId,
        chain: ClipChainId,
        item_clip: Option<LayoutRect>,
        spatial: usize,
        bounds: LayoutRect,
    ) -> usize {
        let item_transform = self.node(pipeline, spatial);
        let item_device_bounds = item_transform.transform_rect_bbox(rect_of(bounds));
        let mut clips: Vec<Clip> = Vec::new();
        if let Some(rect) = item_clip {
            clips.push(Clip {
                spatial: (pipeline, spatial),
                shape: ClipShape::Rect(rect),
            });
        }
        let mut next = Some(chain);
        let mut guard = 0;
        while let Some(chain_id) = next {
            guard += 1;
            let Some((parent, ids)) = self.clip_chains.get(&chain_id) else {
                break;
            };
            clips.extend(ids.iter().filter_map(|id| self.clips.get(id).cloned()));
            next = *parent;
            if guard > 64 {
                break;
            }
        }

        let mut pushed = 0;
        for clip in clips {
            let transform = self
                .spatial_nodes
                .get(&clip.spatial)
                .copied()
                .unwrap_or(Affine::IDENTITY);
            let path = match clip.shape {
                ClipShape::Rect(rect) => {
                    let device = transform.transform_rect_bbox(rect_of(rect));
                    let axis_aligned = transform.as_coeffs()[1] == 0.0 &&
                        transform.as_coeffs()[2] == 0.0;
                    if axis_aligned && contains(device, item_device_bounds) {
                        continue;
                    }
                    transform * rect_of(rect).to_path(0.1)
                },
                ClipShape::RoundedRect(rect, radii) => {
                    transform * rounded_rect_of(rect, &radii).to_path(0.1)
                },
                ClipShape::ImageMask { .. } => continue,
            };
            self.context.set_transform(Affine::IDENTITY);
            self.context.push_clip_layer(&path);
            pushed += 1;
        }
        pushed
    }

    /// Rasterize this stacking context's `mask-image` layers (from its clip
    /// chain) into one canvas-sized alpha mask, combining several layers as
    /// their union -- `mask-composite: add`, the default and the only
    /// composite mode this renderer implements (see
    /// `add_mask_image_clip`'s doc comment in layout for the full list of
    /// what `mask-image` does not yet support).
    fn image_masks(&mut self, chain: Option<ClipChainId>) -> Option<Mask> {
        let mut shapes = Vec::new();
        let mut next = chain;
        let mut guard = 0;
        while let Some(chain_id) = next {
            guard += 1;
            let Some((parent, ids)) = self.clip_chains.get(&chain_id) else {
                break;
            };
            for id in ids {
                if let Some(Clip {
                    spatial,
                    shape:
                        ClipShape::ImageMask {
                            key,
                            rect,
                            tile_size,
                            luminance,
                        },
                }) = self.clips.get(id)
                {
                    shapes.push((*spatial, *key, *rect, *tile_size, *luminance));
                }
            }
            next = *parent;
            if guard > 64 {
                break;
            }
        }
        if shapes.is_empty() {
            return None;
        }
        let (width, height) = (self.context.width(), self.context.height());
        let mut combined: Option<Vec<u8>> = None;
        for (spatial, key, rect, tile_size, luminance) in shapes {
            let transform = self.spatial_nodes.get(&spatial).copied().unwrap_or(Affine::IDENTITY);
            let mut pixmap = Pixmap::new(width, height);
            // An image that has not loaded (yet) masks this layer out entirely,
            // by leaving `pixmap` fully transparent.
            if let Some(image) = self.image(key, AlphaType::PremultipliedAlpha) {
                let settings = RenderSettings {
                    level: vello_cpu::Level::try_detect().unwrap_or(vello_cpu::Level::baseline()),
                    num_threads: 0,
                };
                let context = RenderContext::new_with(width, height, settings);
                let parent = std::mem::replace(&mut self.context, context);
                self.context.set_transform(transform);
                let extend = if tile_size.width < rect.width() || tile_size.height < rect.height() {
                    Extend::Repeat
                } else {
                    Extend::Pad
                };
                self.fill_image(image, rect, tile_size, extend);
                let mut context = std::mem::replace(&mut self.context, parent);
                context.flush();
                context.render(&mut pixmap, &mut self.resources);
            }
            // Per-pixel mask value: the alpha channel, or with `mask-mode:
            // luminance`, the (alpha-premultiplied) luminance. Mirrors
            // `vello_common::mask::Mask::new_with`, whose two constructors
            // (`new_alpha`/`new_luminance`) only produce a standalone `Mask`
            // each, with no way to combine several first.
            let layer: Vec<u8> = pixmap
                .data()
                .iter()
                .map(|pixel| {
                    if !luminance {
                        pixel.a
                    } else {
                        let r = f32::from(pixel.r) / 255.0;
                        let g = f32::from(pixel.g) / 255.0;
                        let b = f32::from(pixel.b) / 255.0;
                        // See CSS Masking Module Level 1 § 7.10.1
                        // <https://www.w3.org/TR/css-masking-1/#MaskValues>.
                        let luma = r * 0.2126 + g * 0.7152 + b * 0.0722;
                        (luma * 255.0 + 0.5) as u8
                    }
                })
                .collect();
            combined = Some(match combined {
                None => layer,
                // `mask-composite: add` (the default): union the masks via
                // Porter-Duff "over", `a + b * (1 - a)` -- symmetric in `a`
                // and `b`, so layer order does not matter here.
                Some(mut acc) => {
                    for (a, b) in acc.iter_mut().zip(layer) {
                        let (a32, b32) = (u16::from(*a), u16::from(b));
                        *a = (a32 + (255 - a32) * b32 / 255) as u8;
                    }
                    acc
                },
            });
        }
        combined.map(|data| Mask::from_parts(data, width, height))
    }

    fn image(&mut self, key: ImageKey, alpha_type: AlphaType) -> Option<Arc<Pixmap>> {
        if let Some(cached) = self.image_cache.get(&key) {
            return cached.clone();
        }
        let pixmap = self
            .frame_resources
            .images
            .get(&key)
            .and_then(|image| pixmap_of(image, alpha_type))
            .map(Arc::new);
        self.image_cache.insert(key, pixmap.clone());
        pixmap
    }

    /// Fill `bounds` with an image, one copy per `tile` size (stretched to it).
    fn fill_image(&mut self, pixmap: Arc<Pixmap>, bounds: LayoutRect, tile: LayoutSize, extend: Extend) {
        let (image_width, image_height) = (pixmap.width() as f64, pixmap.height() as f64);
        if tile.width <= 0.0 || tile.height <= 0.0 || image_width == 0.0 || image_height == 0.0 {
            return;
        }
        let paint_transform = Affine::translate((bounds.min.x as f64, bounds.min.y as f64)) *
            Affine::scale_non_uniform(
                tile.width as f64 / image_width,
                tile.height as f64 / image_height,
            );
        self.context.set_paint(vello_cpu::Image {
            image: vello_cpu::ImageSource::Pixmap(pixmap),
            sampler: ImageSampler {
                x_extend: extend,
                y_extend: extend,
                quality: ImageQuality::Medium,
                alpha: 1.0,
            },
        });
        self.context.set_paint_transform(paint_transform);
        self.context.fill_rect(&rect_of(bounds));
        self.context.reset_paint_transform();
    }

    fn fill_border(
        &mut self,
        bounds: LayoutRect,
        widths: webrender_api::units::LayoutSideOffsets,
        details: &webrender_api::NormalBorder,
    ) {
        let visible = |side: &BorderSide| {
            !matches!(side.style, BorderStyle::None | BorderStyle::Hidden) && side.color.a > 0.0
        };
        let sides = [details.top, details.right, details.bottom, details.left];
        let outer = rect_of(bounds);
        let inner = Rect::new(
            outer.x0 + widths.left as f64,
            outer.y0 + widths.top as f64,
            outer.x1 - widths.right as f64,
            outer.y1 - widths.bottom as f64,
        );

        let uniform_solid = sides.iter().all(|side| {
            side.color == details.top.color && side.style == BorderStyle::Solid
        });
        if uniform_solid {
            if !visible(&details.top) {
                return;
            }
            // One ring: the outer rounded rect minus the inner one.
            let radii = &details.radius;
            let mut path = rounded_rect_of(bounds, radii).to_path(0.1);
            let inner_radii = RoundedRectRadii::new(
                (radii.top_left.width - widths.left).max(0.0) as f64,
                (radii.top_right.width - widths.right).max(0.0) as f64,
                (radii.bottom_right.width - widths.right).max(0.0) as f64,
                (radii.bottom_left.width - widths.left).max(0.0) as f64,
            );
            if inner.width() > 0.0 && inner.height() > 0.0 {
                path.extend(RoundedRect::from_rect(inner, inner_radii).to_path(0.1));
            }
            self.context.set_paint(color_of(details.top.color));
            self.context.set_fill_rule(peniko::Fill::EvenOdd);
            self.context.fill_path(&path);
            self.context.set_fill_rule(peniko::Fill::NonZero);
            return;
        }

        // Per side: index 0 top, 1 right, 2 bottom, 3 left.
        let side_widths = [widths.top, widths.right, widths.bottom, widths.left];
        for (index, side) in sides.iter().enumerate() {
            let width = side_widths[index] as f64;
            if !visible(side) || width <= 0.0 {
                continue;
            }
            let color = color_of(side.color);
            // Sides lit from the top left: top/left are the "light" sides.
            let top_left = index == 0 || index == 3;
            match side.style {
                BorderStyle::Dotted => {
                    // Round dots, evenly spaced so both ends get one.
                    let (start, end) = side_center_line(outer, inner, index);
                    let length = (end - start).hypot();
                    let gaps = (length / (width * 2.0)).round().max(1.0);
                    let mut dots = BezPath::new();
                    for step in 0..=gaps as usize {
                        let center = start.lerp(end, step as f64 / gaps);
                        dots.extend(kurbo::Circle::new(center, width / 2.0).to_path(0.1));
                    }
                    self.context.set_paint(color);
                    self.context.fill_path(&dots);
                },
                BorderStyle::Dashed => {
                    let (start, end) = side_center_line(outer, inner, index);
                    let mut line = BezPath::new();
                    line.move_to(start);
                    line.line_to(end);
                    self.context.set_stroke(
                        Stroke::new(width)
                            .with_caps(Cap::Butt)
                            .with_dashes(0.0, [width * 3.0, width * 3.0]),
                    );
                    self.context.set_paint(color);
                    self.context.stroke_path(&line);
                },
                BorderStyle::Double => {
                    let third = |fraction: f64| lerp_rect(outer, inner, fraction);
                    self.fill_side(outer, third(1.0 / 3.0), index, color);
                    self.fill_side(third(2.0 / 3.0), inner, index, color);
                },
                BorderStyle::Groove | BorderStyle::Ridge => {
                    let middle = lerp_rect(outer, inner, 0.5);
                    let (dark, light) = (shade(side.color, 0.5), color);
                    let outer_color = if (side.style == BorderStyle::Groove) == top_left { dark } else { light };
                    let inner_color = if outer_color == dark { light } else { dark };
                    self.fill_side(outer, middle, index, outer_color);
                    self.fill_side(middle, inner, index, inner_color);
                },
                BorderStyle::Inset | BorderStyle::Outset => {
                    let darken = (side.style == BorderStyle::Inset) == top_left;
                    let color = if darken { shade(side.color, 0.5) } else { color };
                    self.fill_side(outer, inner, index, color);
                },
                _ => self.fill_side(outer, inner, index, color),
            }
        }
    }

    /// Fill one side of the ring between `outer` and `inner`, as a trapezoid
    /// meeting its neighbours at the corners.
    fn fill_side(&mut self, outer: Rect, inner: Rect, index: usize, color: Color) {
        let points = match index {
            0 => [(outer.x0, outer.y0), (outer.x1, outer.y0), (inner.x1, inner.y0), (inner.x0, inner.y0)],
            1 => [(outer.x1, outer.y0), (outer.x1, outer.y1), (inner.x1, inner.y1), (inner.x1, inner.y0)],
            2 => [(outer.x1, outer.y1), (outer.x0, outer.y1), (inner.x0, inner.y1), (inner.x1, inner.y1)],
            _ => [(outer.x0, outer.y1), (outer.x0, outer.y0), (inner.x0, inner.y0), (inner.x0, inner.y1)],
        };
        let mut path = BezPath::new();
        path.move_to(points[0]);
        for point in &points[1..] {
            path.line_to(*point);
        }
        path.close_path();
        self.context.set_paint(color);
        self.context.fill_path(&path);
    }

    fn fill_box_shadow(&mut self, shadow: &webrender_api::BoxShadowDisplayItem) {
        let color = color_of(shadow.color);
        // CSS blur radius is twice the Gaussian standard deviation.
        let std_dev = shadow.blur_radius / 2.0;
        let box_rect = rect_of(shadow.box_bounds);
        let box_path = rounded_rect_of(shadow.box_bounds, &shadow.border_radius).to_path(0.1);
        let spread = shadow.spread_radius as f64;
        let offset = kurbo::Vec2::new(shadow.offset.x as f64, shadow.offset.y as f64);
        let corner = corner_radius(shadow.border_radius.top_left);
        let transform = self.item_transform;

        match shadow.clip_mode {
            BoxShadowClipMode::Outset => {
                let shadow_rect = box_rect.inflate(spread, spread) + offset;
                let radius = (corner + spread).max(0.0) as f32;
                // An outer shadow is not drawn under its box.
                let mut clip = self.device_rect.inflate(1.0, 1.0).to_path(0.1);
                clip.extend(transform * box_path);
                self.push_clip(&clip, peniko::Fill::EvenOdd);
                self.context.set_transform(transform);
                self.context.set_paint(color);
                if std_dev > 0.25 {
                    self.context.fill_blurred_rounded_rect(&shadow_rect, radius, std_dev, false);
                } else {
                    self.context.fill_path(&RoundedRect::from_rect(shadow_rect, radius as f64).to_path(0.1));
                }
                self.context.pop_layer();
            },
            BoxShadowClipMode::Inset => {
                let shadow_rect = box_rect.inflate(-spread, -spread) + offset;
                let radius = (corner - spread).max(0.0) as f32;
                self.push_clip(&(transform * box_path), peniko::Fill::NonZero);
                self.context.set_transform(transform);
                self.context.set_paint(color);
                if std_dev > 0.25 {
                    self.context.fill_blurred_rounded_rect(&shadow_rect, radius, std_dev, true);
                } else {
                    let mut ring = box_rect.inflate(1.0, 1.0).to_path(0.1);
                    ring.extend(RoundedRect::from_rect(shadow_rect, radius as f64).to_path(0.1));
                    self.context.set_fill_rule(peniko::Fill::EvenOdd);
                    self.context.fill_path(&ring);
                    self.context.set_fill_rule(peniko::Fill::NonZero);
                }
                self.context.pop_layer();
            },
        }
    }

    /// Push a device-space clip layer.
    fn push_clip(&mut self, path: &BezPath, fill: peniko::Fill) {
        self.context.set_transform(Affine::IDENTITY);
        self.context.set_fill_rule(fill);
        self.context.push_clip_layer(path);
        self.context.set_fill_rule(peniko::Fill::NonZero);
    }

    /// Draw text shadows (if any) and then the item itself, calling `paint`
    /// with the color to use; the current transform is the item's.
    fn with_shadows(&mut self, color: Color, mut paint: impl FnMut(&mut Self, Color)) {
        let transform = self.item_transform;
        for shadow in self.shadows.clone() {
            let blurred = shadow.blur_radius > 0.0;
            if blurred {
                self.context.set_transform(Affine::IDENTITY);
                self.context.push_filter_layer(Filter::from_primitive(FilterPrimitive::GaussianBlur {
                    std_deviation: shadow.blur_radius / 2.0,
                    edge_mode: EdgeMode::None,
                }));
            }
            self.context.set_transform(
                transform * Affine::translate((shadow.offset.x as f64, shadow.offset.y as f64)),
            );
            paint(self, color_of(shadow.color));
            if blurred {
                self.context.pop_layer();
            }
        }
        self.context.set_transform(transform);
        paint(self, color);
    }

    fn push_stacking_context(
        &mut self,
        blend: MixBlendMode,
        filters: &[FilterOp],
        transform: Affine,
        mask: Option<Mask>,
    ) -> StackingEntry {
        let mut opacity = 1.0f32;
        let mut matrix: Option<ColorMatrix> = None;
        let mut effects = Vec::new();
        for filter in filters {
            let step = match *filter {
                FilterOp::Opacity(ref binding, _) => {
                    opacity *= binding_value(binding);
                    None
                },
                FilterOp::Blur(width, height) => {
                    // Blur scales with the element's transform.
                    let scale = transform.as_coeffs()[0].hypot(transform.as_coeffs()[1]) as f32;
                    effects.push(Filter::from_function(FilterFunction::Blur {
                        radius: width.max(height) * scale,
                    }));
                    None
                },
                FilterOp::DropShadow(shadow) => {
                    effects.push(Filter::from_primitive(FilterPrimitive::DropShadow {
                        dx: shadow.offset.x,
                        dy: shadow.offset.y,
                        std_deviation: shadow.blur_radius / 2.0,
                        color: color_of(shadow.color),
                        edge_mode: EdgeMode::None,
                    }));
                    None
                },
                FilterOp::Brightness(amount) => Some(brightness_matrix(amount)),
                FilterOp::Contrast(amount) => Some(contrast_matrix(amount)),
                FilterOp::Grayscale(amount) => Some(saturate_matrix(1.0 - amount.clamp(0.0, 1.0))),
                FilterOp::Saturate(amount) => Some(saturate_matrix(amount)),
                FilterOp::HueRotate(degrees) => Some(hue_rotate_matrix(degrees)),
                FilterOp::Invert(amount) => Some(invert_matrix(amount.clamp(0.0, 1.0))),
                FilterOp::Sepia(amount) => Some(sepia_matrix(amount.clamp(0.0, 1.0))),
                _ => None,
            };
            if let Some(step) = step {
                matrix = Some(match matrix {
                    Some(previous) => multiply(&step, &previous),
                    None => step,
                });
            }
        }

        // Outermost first: blend with the backdrop, then opacity, then effects;
        // color filters apply first, to the element's own pixels.
        let mut layers = 0;
        self.context.set_transform(Affine::IDENTITY);
        if let Some(mix) = mix_of(blend) {
            self.context.push_blend_layer(mix);
            layers += 1;
        }
        if let Some(mask) = mask {
            self.context.push_mask_layer(mask);
            layers += 1;
        }
        if opacity < 1.0 {
            self.context.push_opacity_layer(opacity.max(0.0));
            layers += 1;
        }
        for effect in effects.into_iter().rev() {
            self.context.push_filter_layer(effect);
            layers += 1;
        }
        let offscreen = matrix.is_some();
        if let Some(matrix) = matrix {
            let settings = RenderSettings {
                level: vello_cpu::Level::try_detect().unwrap_or(vello_cpu::Level::baseline()),
                num_threads: 0,
            };
            let context =
                RenderContext::new_with(self.context.width(), self.context.height(), settings);
            let parent = std::mem::replace(&mut self.context, context);
            self.offscreen.push((parent, matrix));
        }
        StackingEntry { layers, offscreen }
    }

    fn pop_stacking_context(&mut self, entry: StackingEntry) {
        if entry.offscreen {
            if let Some((parent, matrix)) = self.offscreen.pop() {
                let mut context = std::mem::replace(&mut self.context, parent);
                let mut pixmap = Pixmap::new(context.width(), context.height());
                context.flush();
                context.render(&mut pixmap, &mut self.resources);
                apply_color_matrix(&mut pixmap, &matrix);
                let device = self.device_rect;
                self.context.set_transform(Affine::IDENTITY);
                self.context.set_paint(vello_cpu::Image {
                    image: vello_cpu::ImageSource::Pixmap(Arc::new(pixmap)),
                    sampler: ImageSampler {
                        x_extend: Extend::Pad,
                        y_extend: Extend::Pad,
                        quality: ImageQuality::Low,
                        alpha: 1.0,
                    },
                });
                self.context.reset_paint_transform();
                self.context.fill_rect(&device);
            }
        }
        for _ in 0..entry.layers {
            self.context.pop_layer();
        }
    }
}

fn reference_transform(binding: &ReferenceTransformBinding) -> Affine {
    match binding {
        ReferenceTransformBinding::Static { binding } => affine_of(&binding_value(binding)),
        ReferenceTransformBinding::Computed { .. } => Affine::IDENTITY,
    }
}

/// The 2D part of a 3D transform (projected onto the z = 0 plane).
fn affine_of(transform: &LayoutTransform) -> Affine {
    Affine::new([
        transform.m11 as f64,
        transform.m12 as f64,
        transform.m21 as f64,
        transform.m22 as f64,
        transform.m41 as f64,
        transform.m42 as f64,
    ])
}

fn binding_value<T: Copy>(binding: &PropertyBinding<T>) -> T {
    match binding {
        PropertyBinding::Value(value) | PropertyBinding::Binding(_, value) => *value,
    }
}

fn color_of(color: ColorF) -> Color {
    Color::new([color.r, color.g, color.b, color.a])
}

fn rect_of(rect: LayoutRect) -> Rect {
    Rect::new(
        rect.min.x as f64,
        rect.min.y as f64,
        rect.max.x as f64,
        rect.max.y as f64,
    )
}

fn rounded_rect_of(rect: LayoutRect, radii: &BorderRadius) -> RoundedRect {
    // kurbo uses one radius per corner; use the smaller of each elliptical pair.
    let corner = |size: LayoutSize| size.width.min(size.height).max(0.0) as f64;
    RoundedRect::from_rect(
        rect_of(rect),
        RoundedRectRadii::new(
            corner(radii.top_left),
            corner(radii.top_right),
            corner(radii.bottom_right),
            corner(radii.bottom_left),
        ),
    )
}

fn contains(outer: Rect, inner: Rect) -> bool {
    outer.x0 <= inner.x0 && outer.y0 <= inner.y0 && outer.x1 >= inner.x1 && outer.y1 >= inner.y1
}

/// Gradient points are relative to the item's bounds.
fn point_in(bounds: LayoutRect, point: webrender_api::units::LayoutPoint) -> kurbo::Point {
    kurbo::Point::new(
        (bounds.min.x + point.x) as f64,
        (bounds.min.y + point.y) as f64,
    )
}

fn stops_of(stops: impl Iterator<Item = GradientStop>) -> peniko::ColorStops {
    stops
        .map(|stop| ColorStop::from((stop.offset, color_of(stop.color))))
        .collect::<Vec<_>>()
        .as_slice()
        .into()
}

fn extend_of(mode: ExtendMode) -> Extend {
    match mode {
        ExtendMode::Clamp => Extend::Pad,
        ExtendMode::Repeat => Extend::Repeat,
    }
}

/// Convert image bytes to vello_cpu's premultiplied RGBA pixmap.
fn pixmap_of(image: &worker_frame::WorkerImage, alpha_type: AlphaType) -> Option<Pixmap> {
    let descriptor = &image.descriptor;
    let (width, height) = (descriptor.size.width, descriptor.size.height);
    if width <= 0 || height <= 0 || width > u16::MAX as i32 || height > u16::MAX as i32 {
        return None;
    }
    let (red, blue) = match descriptor.format {
        ImageFormat::BGRA8 => (2, 0),
        ImageFormat::RGBA8 => (0, 2),
        _ => return None,
    };
    let stride = descriptor.stride.unwrap_or(width * 4) as usize;
    let offset = descriptor.offset.max(0) as usize;
    let mut pixels = Vec::with_capacity((width * height) as usize);
    for y in 0..height as usize {
        let row = image.data.get(offset + y * stride..offset + y * stride + width as usize * 4)?;
        for pixel in row.chunks_exact(4) {
            let alpha = pixel[3];
            let premultiply = |value: u8| match alpha_type {
                AlphaType::PremultipliedAlpha => value,
                AlphaType::Alpha => ((value as u16 * alpha as u16 + 127) / 255) as u8,
            };
            pixels.push(peniko::color::PremulRgba8 {
                r: premultiply(pixel[red]),
                g: premultiply(pixel[1]),
                b: premultiply(pixel[blue]),
                a: alpha,
            });
        }
    }
    Some(Pixmap::from_parts(pixels, width as u16, height as u16))
}

/// Scroll offset and content size of each scrolling spatial node, by index.
fn scroll_nodes(scroll_tree: &ScrollTree) -> FxHashMap<usize, (webrender_api::units::LayoutVector2D, LayoutSize)> {
    scroll_tree
        .nodes
        .iter()
        .filter_map(|node| match (&node.info, node.webrender_id) {
            (SpatialTreeNodeInfo::Scroll(info), Some(id)) => {
                Some((id.0, (info.offset, info.content_rect.size())))
            },
            _ => None,
        })
        .collect()
}

fn corner_radius(size: LayoutSize) -> f64 {
    size.width.min(size.height).max(0.0) as f64
}

/// The area a box shadow can paint.
fn shadow_extent(shadow: &webrender_api::BoxShadowDisplayItem) -> LayoutRect {
    let reach = shadow.spread_radius.abs() + shadow.blur_radius * 1.5 +
        shadow.offset.x.abs().max(shadow.offset.y.abs());
    shadow.box_bounds.inflate(reach, reach)
}

/// Grow an item's bounds to cover its text shadows.
fn inflate_for_shadows(bounds: LayoutRect, shadows: &[Shadow]) -> LayoutRect {
    shadows.iter().fold(bounds, |bounds, shadow| {
        let reach = shadow.blur_radius * 1.5 + shadow.offset.x.abs().max(shadow.offset.y.abs());
        bounds.union(&bounds.inflate(reach, reach))
    })
}

/// The centre line of a border side (index 0 top, 1 right, 2 bottom, 3 left).
fn side_center_line(outer: Rect, inner: Rect, index: usize) -> (kurbo::Point, kurbo::Point) {
    let middle = lerp_rect(outer, inner, 0.5);
    match index {
        0 => ((outer.x0, middle.y0).into(), (outer.x1, middle.y0).into()),
        1 => ((middle.x1, outer.y0).into(), (middle.x1, outer.y1).into()),
        2 => ((outer.x1, middle.y1).into(), (outer.x0, middle.y1).into()),
        _ => ((middle.x0, outer.y1).into(), (middle.x0, outer.y0).into()),
    }
}

fn lerp_rect(outer: Rect, inner: Rect, fraction: f64) -> Rect {
    let lerp = |a: f64, b: f64| a + (b - a) * fraction;
    Rect::new(
        lerp(outer.x0, inner.x0),
        lerp(outer.y0, inner.y0),
        lerp(outer.x1, inner.x1),
        lerp(outer.y1, inner.y1),
    )
}

fn shade(color: ColorF, factor: f32) -> Color {
    Color::new([color.r * factor, color.g * factor, color.b * factor, color.a])
}

fn mix_of(blend: MixBlendMode) -> Option<BlendMode> {
    let mix = match blend {
        MixBlendMode::Normal => return None,
        MixBlendMode::PlusLighter => return Some(BlendMode::new(Mix::Normal, Compose::Plus)),
        MixBlendMode::Multiply => Mix::Multiply,
        MixBlendMode::Screen => Mix::Screen,
        MixBlendMode::Overlay => Mix::Overlay,
        MixBlendMode::Darken => Mix::Darken,
        MixBlendMode::Lighten => Mix::Lighten,
        MixBlendMode::ColorDodge => Mix::ColorDodge,
        MixBlendMode::ColorBurn => Mix::ColorBurn,
        MixBlendMode::HardLight => Mix::HardLight,
        MixBlendMode::SoftLight => Mix::SoftLight,
        MixBlendMode::Difference => Mix::Difference,
        MixBlendMode::Exclusion => Mix::Exclusion,
        MixBlendMode::Hue => Mix::Hue,
        MixBlendMode::Saturation => Mix::Saturation,
        MixBlendMode::Color => Mix::Color,
        MixBlendMode::Luminosity => Mix::Luminosity,
    };
    Some(BlendMode::new(mix, Compose::SrcOver))
}

// Color matrices from the Filter Effects specification.

fn brightness_matrix(amount: f32) -> ColorMatrix {
    let a = amount.max(0.0);
    [a, 0., 0., 0., 0., 0., a, 0., 0., 0., 0., 0., a, 0., 0., 0., 0., 0., 1., 0.]
}

fn contrast_matrix(amount: f32) -> ColorMatrix {
    let (a, o) = (amount.max(0.0), 0.5 - 0.5 * amount.max(0.0));
    [a, 0., 0., 0., o, 0., a, 0., 0., o, 0., 0., a, 0., o, 0., 0., 0., 1., 0.]
}

fn invert_matrix(a: f32) -> ColorMatrix {
    let d = 1.0 - 2.0 * a;
    [d, 0., 0., 0., a, 0., d, 0., 0., a, 0., 0., d, 0., a, 0., 0., 0., 1., 0.]
}

fn saturate_matrix(s: f32) -> ColorMatrix {
    let s = s.max(0.0);
    [
        0.213 + 0.787 * s, 0.715 - 0.715 * s, 0.072 - 0.072 * s, 0., 0.,
        0.213 - 0.213 * s, 0.715 + 0.285 * s, 0.072 - 0.072 * s, 0., 0.,
        0.213 - 0.213 * s, 0.715 - 0.715 * s, 0.072 + 0.928 * s, 0., 0.,
        0., 0., 0., 1., 0.,
    ]
}

fn sepia_matrix(a: f32) -> ColorMatrix {
    let i = 1.0 - a;
    [
        0.393 + 0.607 * i, 0.769 - 0.769 * i, 0.189 - 0.189 * i, 0., 0.,
        0.349 - 0.349 * i, 0.686 + 0.314 * i, 0.168 - 0.168 * i, 0., 0.,
        0.272 - 0.272 * i, 0.534 - 0.534 * i, 0.131 + 0.869 * i, 0., 0.,
        0., 0., 0., 1., 0.,
    ]
}

fn hue_rotate_matrix(degrees: f32) -> ColorMatrix {
    let (sin, cos) = degrees.to_radians().sin_cos();
    [
        0.213 + cos * 0.787 - sin * 0.213,
        0.715 - cos * 0.715 - sin * 0.715,
        0.072 - cos * 0.072 + sin * 0.928,
        0., 0.,
        0.213 - cos * 0.213 + sin * 0.143,
        0.715 + cos * 0.285 + sin * 0.140,
        0.072 - cos * 0.072 - sin * 0.283,
        0., 0.,
        0.213 - cos * 0.213 - sin * 0.787,
        0.715 - cos * 0.715 + sin * 0.715,
        0.072 + cos * 0.928 + sin * 0.072,
        0., 0.,
        0., 0., 0., 1., 0.,
    ]
}

/// `after * before`: apply `before`, then `after`.
fn multiply(after: &ColorMatrix, before: &ColorMatrix) -> ColorMatrix {
    let mut result = [0.0; 20];
    for row in 0..4 {
        for column in 0..5 {
            let mut value = if column == 4 { after[row * 5 + 4] } else { 0.0 };
            for k in 0..4 {
                value += after[row * 5 + k] * before[k * 5 + column];
            }
            result[row * 5 + column] = value;
        }
    }
    result
}

/// Apply a color matrix to premultiplied RGBA pixels, in unpremultiplied space.
fn apply_color_matrix(pixmap: &mut Pixmap, matrix: &ColorMatrix) {
    for pixel in pixmap.data_as_u8_slice_mut().chunks_exact_mut(4) {
        let alpha = pixel[3] as f32 / 255.0;
        if alpha == 0.0 {
            continue;
        }
        let channel = |value: u8| (value as f32 / 255.0) / alpha;
        let input = [channel(pixel[0]), channel(pixel[1]), channel(pixel[2]), alpha];
        let mut output = [0.0f32; 4];
        for (row, out) in output.iter_mut().enumerate() {
            let m = &matrix[row * 5..row * 5 + 5];
            *out = (m[0] * input[0] + m[1] * input[1] + m[2] * input[2] + m[3] * input[3] + m[4])
                .clamp(0.0, 1.0);
        }
        let new_alpha = output[3];
        for index in 0..3 {
            pixel[index] = (output[index] * new_alpha * 255.0 + 0.5) as u8;
        }
        pixel[3] = (new_alpha * 255.0 + 0.5) as u8;
    }
}
