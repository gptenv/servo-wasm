/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Worker WASM frame capture. The Worker has no WebRender `Painter`, so Paint
//! keeps the latest display list of each pipeline here instead of submitting
//! it to WebRender, for the Worker's CPU renderer to draw on demand.

use std::cell::RefCell;

use paint_api::display_list::PaintDisplayListInfo;
use paint_api::SerializableDisplayListPayload;
use rustc_hash::FxHashMap;
use servo_base::generic_channel::GenericReceiver;
use servo_base::id::PipelineId;
use webrender_api::{
    BuiltDisplayList, BuiltDisplayListDescriptor, DisplayListPayload,
    PipelineId as WebRenderPipelineId,
};

pub(crate) struct CapturedDisplayList {
    pub(crate) info: PaintDisplayListInfo,
    pub(crate) display_list: BuiltDisplayList,
    /// Capture order, to find the most recent top-level document.
    pub(crate) sequence: u64,
}

thread_local! {
    static NEXT_SEQUENCE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static DISPLAY_LISTS: RefCell<FxHashMap<WebRenderPipelineId, CapturedDisplayList>> =
        RefCell::new(FxHashMap::default());
}

/// Keep a display list sent by layout. Layout sends the info and payload right
/// after the message, on this same thread, so they are already queued.
pub(crate) fn capture_display_list(
    descriptor: BuiltDisplayListDescriptor,
    info_receiver: GenericReceiver<PaintDisplayListInfo>,
    data_receiver: GenericReceiver<SerializableDisplayListPayload>,
) {
    let (Ok(info), Ok(data)) = (info_receiver.try_recv(), data_receiver.try_recv()) else {
        log::warn!("Worker frame capture: display list parts were not queued");
        return;
    };
    let display_list = BuiltDisplayList::from_data(
        DisplayListPayload {
            items_data: data.items_data,
            spatial_tree: data.spatial_tree,
        },
        descriptor,
    );
    let pipeline_id = info.pipeline_id;
    let sequence = NEXT_SEQUENCE.with(|next| next.replace(next.get() + 1));
    DISPLAY_LISTS.with(|lists| {
        lists.borrow_mut().insert(
            pipeline_id,
            CapturedDisplayList {
                info,
                display_list,
                sequence,
            },
        )
    });
}

/// Apply a script scroll to a captured display list's scroll tree. Layout sends
/// the node's new scroll offset (not a delta) in `ScrollNodeByDelta`; later
/// display lists carry it themselves.
pub(crate) fn set_scroll_offset(
    pipeline_id: WebRenderPipelineId,
    scroll_id: webrender_api::ExternalScrollId,
    offset: webrender_api::units::LayoutVector2D,
) {
    DISPLAY_LISTS.with(|lists| {
        if let Some(captured) = lists.borrow_mut().get_mut(&pipeline_id) {
            captured
                .info
                .scroll_tree
                .set_scroll_offset_for_node_with_external_scroll_id(
                    scroll_id,
                    offset,
                    paint_api::display_list::ScrollType::Script,
                );
        }
    });
}

/// A human-readable summary of the captured spatial trees and scroll offsets,
/// for diagnosing the Worker renderer.
pub(crate) fn describe() -> String {
    use std::fmt::Write;
    let mut out = String::new();
    DISPLAY_LISTS.with(|lists| {
        for (pipeline, captured) in lists.borrow().iter() {
            let _ = writeln!(
                out,
                "pipeline {pipeline:?} seq {} viewport {:?}",
                captured.sequence, captured.info.viewport_details.size
            );
            captured.display_list.iter_spatial_tree(|item| {
                let _ = writeln!(out, "  {item:?}");
            });
            let _ = writeln!(out, "  offsets {:?}", captured.info.scroll_tree.scroll_offsets());
        }
    });
    out
}

/// Forget display lists of a pipeline that has gone away.
pub(crate) fn remove_pipeline(pipeline_id: PipelineId) {
    let pipeline_id: WebRenderPipelineId = pipeline_id.into();
    DISPLAY_LISTS.with(|lists| lists.borrow_mut().remove(&pipeline_id));
}

/// The most recently captured display list that is not an iframe's content.
pub(crate) fn root_pipeline(
    lists: &FxHashMap<WebRenderPipelineId, CapturedDisplayList>,
) -> Option<WebRenderPipelineId> {
    let mut children = rustc_hash::FxHashSet::default();
    for captured in lists.values() {
        let mut iter = captured.display_list.iter();
        while let Some(item) = iter.next() {
            if let webrender_api::DisplayItem::Iframe(iframe) = item.item() {
                children.insert(iframe.pipeline_id);
            }
        }
    }
    lists
        .iter()
        .filter(|(pipeline, _)| !children.contains(*pipeline))
        .max_by_key(|(_, captured)| captured.sequence)
        .map(|(pipeline, _)| *pipeline)
}

/// Run `f` with the captured display lists.
pub(crate) fn with_display_lists<R>(
    f: impl FnOnce(&FxHashMap<WebRenderPipelineId, CapturedDisplayList>) -> R,
) -> R {
    DISPLAY_LISTS.with(|lists| f(&lists.borrow()))
}

/// Pixel data registered for an image key, as WebRender would receive it.
pub(crate) struct WorkerImage {
    pub(crate) descriptor: webrender_api::ImageDescriptor,
    pub(crate) data: std::sync::Arc<Vec<u8>>,
}

pub(crate) struct WorkerFontInstance {
    pub(crate) font_key: webrender_api::FontKey,
    /// Font size in device pixels.
    pub(crate) size: f32,
    pub(crate) variations: Vec<webrender_api::FontVariation>,
}

#[derive(Default)]
pub(crate) struct WorkerResources {
    pub(crate) images: FxHashMap<webrender_api::ImageKey, WorkerImage>,
    pub(crate) fonts: FxHashMap<webrender_api::FontKey, vello_cpu::peniko::FontData>,
    pub(crate) font_instances: FxHashMap<webrender_api::FontInstanceKey, WorkerFontInstance>,
}

thread_local! {
    static RESOURCES: RefCell<WorkerResources> = RefCell::new(WorkerResources::default());
    static RESOURCE_GENERATION: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Changes whenever an image, font or font instance arrives, so a host can
/// tell whether a frame triggered resource loads that a later frame will show.
pub(crate) fn resource_generation() -> u32 {
    RESOURCE_GENERATION.with(std::cell::Cell::get)
}

fn bump_resource_generation() {
    RESOURCE_GENERATION.with(|generation| generation.set(generation.get().wrapping_add(1)));
}

pub(crate) fn with_resources<R>(f: impl FnOnce(&WorkerResources) -> R) -> R {
    RESOURCES.with(|resources| f(&resources.borrow()))
}

pub(crate) fn update_images(updates: impl IntoIterator<Item = paint_api::ImageUpdate>) {
    use paint_api::{ImageUpdate, SerializableImageData};
    bump_resource_generation();
    RESOURCES.with(|resources| {
        let images = &mut resources.borrow_mut().images;
        for update in updates {
            match update {
                ImageUpdate::AddImage(key, descriptor, data, _) |
                ImageUpdate::UpdateImage(key, descriptor, data, _) => match data {
                    SerializableImageData::Raw(bytes) => {
                        images.insert(
                            key,
                            WorkerImage {
                                descriptor,
                                data: std::sync::Arc::new(bytes.to_vec()),
                            },
                        );
                    },
                    // External images (WebGL, media, WebGPU) do not exist on the Worker.
                    SerializableImageData::External(_) => {
                        images.remove(&key);
                    },
                },
                ImageUpdate::UpdateImageForAnimation(key, descriptor) => {
                    if let Some(image) = images.get_mut(&key) {
                        image.descriptor = descriptor;
                    }
                },
                ImageUpdate::DeleteImage(key) => {
                    images.remove(&key);
                },
            }
        }
    });
}

pub(crate) fn add_font(key: webrender_api::FontKey, data: &[u8], index: u32) {
    let font = vello_cpu::peniko::FontData::new(
        vello_cpu::peniko::Blob::new(std::sync::Arc::new(data.to_vec())),
        index,
    );
    RESOURCES.with(|resources| resources.borrow_mut().fonts.insert(key, font));
    bump_resource_generation();
}

/// Local fonts on the Worker live in the font registry; their handle path is
/// the registry identifier (`worker-font:<n>`).
pub(crate) fn add_system_font(key: webrender_api::FontKey, handle: webrender_api::NativeFontHandle) {
    let Some(data) = handle
        .path
        .to_str()
        .and_then(|path| path.strip_prefix(fonts_traits::worker_fonts::PATH_PREFIX))
        .and_then(|index| index.parse::<usize>().ok())
        .and_then(fonts_traits::worker_fonts::get)
    else {
        log::warn!("Worker renderer: unknown system font {:?}", handle.path);
        return;
    };
    add_font(key, data.as_ref(), handle.index);
}

pub(crate) fn add_font_instance(
    instance_key: webrender_api::FontInstanceKey,
    font_key: webrender_api::FontKey,
    size: f32,
    variations: Vec<webrender_api::FontVariation>,
) {
    bump_resource_generation();
    RESOURCES.with(|resources| {
        resources.borrow_mut().font_instances.insert(
            instance_key,
            WorkerFontInstance {
                font_key,
                size,
                variations,
            },
        )
    });
}

pub(crate) fn remove_fonts(
    keys: Vec<webrender_api::FontKey>,
    instance_keys: Vec<webrender_api::FontInstanceKey>,
) {
    RESOURCES.with(|resources| {
        let mut resources = resources.borrow_mut();
        for key in keys {
            resources.fonts.remove(&key);
        }
        for key in instance_keys {
            resources.font_instances.remove(&key);
        }
    });
}
