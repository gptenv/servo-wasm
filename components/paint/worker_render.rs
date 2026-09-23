/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! CPU rasterizer for the Worker WASM target. The Worker has no WebRender
//! renderer, so this interprets the display lists captured in
//! [`crate::worker_frame`] with `vello_cpu`, the rasterizer canvas 2D uses.
//!
//! Supported: the spatial tree (2D transforms; scroll and sticky frames at their
//! unscrolled positions), rect and rounded-rect clips, rectangles, text,
//! images (stretched and repeated), borders (solid, with radii), lines,
//! linear/radial gradients, stacking-context opacity and iframes. Not yet:
//! box and text shadows, dashed/dotted/3D border styles, blend modes, filters
//! other than opacity, 3D transforms and scroll offsets.

use std::sync::Arc;

use euclid::default::Size2D;
use vello_cpu::kurbo::{self, Affine, BezPath, Rect, RoundedRect, RoundedRectRadii, Shape};
use vello_cpu::peniko::{self, Color, ColorStop, Extend, Gradient, ImageQuality, ImageSampler};
use pixels::{EncodedImageType, Snapshot, SnapshotAlphaMode, SnapshotPixelFormat};
use rustc_hash::FxHashMap;
use vello_cpu::{Glyph, Pixmap, RenderContext, RenderSettings, Resources};
use webrender_api::units::{LayoutRect, LayoutSize, LayoutTransform};
use webrender_api::{
    AlphaType, BorderDetails, BorderRadius, BorderStyle, BuiltDisplayList, ClipChainId, ClipId,
    ClipMode, ColorF, DisplayItem, ExtendMode, FilterOp, GradientStop, ImageFormat, ImageKey,
    PipelineId, PropertyBinding, ReferenceFrameKind, ReferenceTransformBinding, SpatialTreeItem,
};

use crate::worker_frame::{self, WorkerResources};

/// Largest rendered dimension in pixels (vello_cpu uses 16-bit sizes).
const MAX_DIMENSION: u32 = 16_384;
/// Iframes nested deeper than this are not drawn.
const MAX_IFRAME_DEPTH: usize = 8;

/// Render the latest top-level document to a PNG.
pub(crate) fn render_png() -> Result<Vec<u8>, String> {
    let (width, height, pixmap) = worker_frame::with_display_lists(|lists| {
        let root = worker_frame::root_pipeline(lists).ok_or("No page has been rendered yet")?;
        let info = &lists[&root].info;
        let scale = info.viewport_details.hidpi_scale_factor.get();
        let size = info.viewport_details.size * scale;
        let width = (size.width.ceil() as u32).clamp(1, MAX_DIMENSION);
        let height = (size.height.ceil() as u32).clamp(1, MAX_DIMENSION);
        let pixmap = worker_frame::with_resources(|resources| {
            let mut renderer = Renderer::new(width as u16, height as u16, lists, resources);
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

    fn build_spatial_tree(&mut self, pipeline: PipelineId, list: &BuiltDisplayList, base: Affine) {
        // Nodes 0 and 1 are the implicit root reference frame and root scroll node.
        self.spatial_nodes.insert((pipeline, 0), base);
        self.spatial_nodes.insert((pipeline, 1), base);
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
                    self.spatial_nodes
                        .insert((pipeline, descriptor.scroll_frame_id.0), parent);
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
        self.build_spatial_tree(pipeline, list, base);

        // Opacity layers pushed for each open stacking context.
        let mut stacking_layers: Vec<bool> = Vec::new();
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
                DisplayItem::ClipChain(chain) => {
                    let clip_ids = item.clip_chain_items().iter().collect();
                    self.clip_chains.insert(chain.id, (chain.parent, clip_ids));
                },
                DisplayItem::PushStackingContext(stacking_context) => {
                    let opacity: f32 = item
                        .filters()
                        .iter()
                        .filter_map(|filter| match filter {
                            FilterOp::Opacity(binding, _) => Some(binding_value(&binding)),
                            _ => None,
                        })
                        .product();
                    let pushed = opacity < 1.0;
                    if pushed {
                        self.context.set_transform(Affine::IDENTITY);
                        self.context.push_opacity_layer(opacity.max(0.0));
                    }
                    let _ = stacking_context;
                    stacking_layers.push(pushed);
                },
                DisplayItem::PopStackingContext => {
                    if stacking_layers.pop() == Some(true) {
                        self.context.pop_layer();
                    }
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
                    let color = text.color;
                    self.draw(pipeline, &text.common, text.bounds, |renderer| {
                        renderer.context.set_paint(color_of(color));
                        renderer
                            .context
                            .glyph_run(&mut renderer.resources, font)
                            .font_size(size)
                            .fill_glyphs(glyphs.into_iter());
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
                    let (area, color) = (line.area, line.color);
                    self.draw(pipeline, &line.common, area, |renderer| {
                        renderer.context.set_paint(color_of(color));
                        renderer.context.fill_rect(&rect_of(area));
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
        for pushed in stacking_layers {
            if pushed {
                self.context.pop_layer();
            }
        }
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
            };
            self.context.set_transform(Affine::IDENTITY);
            self.context.push_clip_layer(&path);
            pushed += 1;
        }
        pushed
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
        let visible = |style: BorderStyle| !matches!(style, BorderStyle::None | BorderStyle::Hidden);
        let sides = [details.top, details.right, details.bottom, details.left];
        let uniform = sides.iter().all(|side| {
            side.color == details.top.color && side.style == details.top.style
        });
        let outer = rect_of(bounds);
        let inner = Rect::new(
            outer.x0 + widths.left as f64,
            outer.y0 + widths.top as f64,
            outer.x1 - widths.right as f64,
            outer.y1 - widths.bottom as f64,
        );

        if uniform {
            if !visible(details.top.style) {
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

        // Sides differ: draw each side as a trapezoid meeting at the corners.
        let quads = [
            (details.top, [(outer.x0, outer.y0), (outer.x1, outer.y0), (inner.x1, inner.y0), (inner.x0, inner.y0)]),
            (details.right, [(outer.x1, outer.y0), (outer.x1, outer.y1), (inner.x1, inner.y1), (inner.x1, inner.y0)]),
            (details.bottom, [(outer.x1, outer.y1), (outer.x0, outer.y1), (inner.x0, inner.y1), (inner.x1, inner.y1)]),
            (details.left, [(outer.x0, outer.y1), (outer.x0, outer.y0), (inner.x0, inner.y0), (inner.x0, inner.y1)]),
        ];
        for (side, points) in quads {
            if !visible(side.style) || side.color.a == 0.0 {
                continue;
            }
            let mut path = BezPath::new();
            path.move_to(points[0]);
            for point in &points[1..] {
                path.line_to(*point);
            }
            path.close_path();
            self.context.set_paint(color_of(side.color));
            self.context.fill_path(&path);
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
