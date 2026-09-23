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
}

thread_local! {
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
    DISPLAY_LISTS.with(|lists| {
        lists.borrow_mut().insert(
            pipeline_id,
            CapturedDisplayList { info, display_list },
        )
    });
}

/// Forget display lists of a pipeline that has gone away.
pub(crate) fn remove_pipeline(pipeline_id: PipelineId) {
    let pipeline_id: WebRenderPipelineId = pipeline_id.into();
    DISPLAY_LISTS.with(|lists| lists.borrow_mut().remove(&pipeline_id));
}

/// Run `f` with the captured display lists.
pub(crate) fn with_display_lists<R>(
    f: impl FnOnce(&FxHashMap<WebRenderPipelineId, CapturedDisplayList>) -> R,
) -> R {
    DISPLAY_LISTS.with(|lists| f(&lists.borrow()))
}
