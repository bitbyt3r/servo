/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

#![deny(unsafe_code)]

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use crossbeam_channel::Sender;
use embedder_traits::{EventLoopWaker, ShutdownState};
use paint_api::{PaintMessage, PaintProxy, WebRenderExternalImageApi};
use profile_traits::{mem, time};
use servo_base::generic_channel::RoutedReceiver;
use servo_constellation_traits::EmbedderToConstellationMessage;
#[cfg(feature = "webxr")]
use webxr::WebXrRegistry;

pub use crate::paint::{Paint, WebRenderDebugOption};

#[macro_use]
mod tracing;

mod largest_contentful_paint_calculator;
mod paint;
mod painter;
mod pinch_zoom;
mod pipeline_details;
mod refresh_driver;
mod render_notifier;
mod screenshot;
mod touch;
mod web_content_animation;
mod webrender_external_images;
mod webview_renderer;

/// Data used to initialize the `Paint` subsystem.
pub struct InitialPaintState {
    /// A channel to `Paint`.
    pub paint_proxy: PaintProxy,
    /// A port on which messages inbound to `Paint` can be received.
    pub receiver: RoutedReceiver<PaintMessage>,
    /// A channel to the constellation.
    pub embedder_to_constellation_sender: Sender<EmbedderToConstellationMessage>,
    /// A channel to the time profiler thread.
    pub time_profiler_chan: time::ProfilerChan,
    /// A channel to the memory profiler thread.
    pub mem_profiler_chan: mem::ProfilerChan,
    /// A shared state which tracks whether Servo has started or has finished
    /// shutting down.
    pub shutdown_state: Rc<Cell<ShutdownState>>,
    /// An [`EventLoopWaker`] used in order to wake up the embedder when it is
    /// time to paint.
    pub event_loop_waker: Box<dyn EventLoopWaker>,
    /// If WebXR is enabled, a [`WebXrRegistry`] to register WebXR threads.
    #[cfg(feature = "webxr")]
    pub webxr_registry: Box<dyn WebXrRegistry>,
    /// `bops-render` extension. Optional embedder-supplied factory that
    /// produces one [`WebRenderExternalImageApi`] handler per [`Painter`]
    /// (each `Painter` owns its own [`WebRenderExternalImageHandlers`],
    /// so the factory is invoked once per panel-rendering-context). The
    /// factory returns a fresh boxed handler each call; typical
    /// implementations wrap an `Arc<AssetCache>` (or equivalent state)
    /// and route `lock(external_id)` to the cache's image entries.
    /// `None` disables the extension; unknown external_ids then panic
    /// in `WebRenderExternalImageHandlers::lock` as before.
    pub bops_asset_external_image_handler_factory:
        Option<Arc<dyn BopsAssetExternalImageHandlerFactory>>,
}

/// `bops-render` extension. Factory for [`WebRenderExternalImageApi`]
/// handlers, called once per [`Painter`] construction. See
/// [`InitialPaintState::bops_asset_external_image_handler_factory`].
pub trait BopsAssetExternalImageHandlerFactory: Send + Sync {
    fn make_handler(&self) -> Box<dyn WebRenderExternalImageApi>;
}
