/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::sync::{Arc, OnceLock};

use glib::prelude::*;
use servo_media_gstreamer_render::Render;
use servo_media_player::PlayerError;
use servo_media_player::context::PlayerGLContext;
use servo_media_player::video::{Buffer, VideoFrame, VideoFrameData};

#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
))]
mod platform {
    extern crate servo_media_gstreamer_render_unix;
    pub use self::servo_media_gstreamer_render_unix::RenderUnix as Render;
    use super::*;

    pub fn create_render(gl_context: Box<dyn PlayerGLContext>) -> Option<Render> {
        Render::new(gl_context)
    }
}

#[cfg(target_os = "android")]
mod platform {
    extern crate servo_media_gstreamer_render_android;
    pub use self::servo_media_gstreamer_render_android::RenderAndroid as Render;
    use super::*;

    pub fn create_render(gl_context: Box<dyn PlayerGLContext>) -> Option<Render> {
        Render::new(gl_context)
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "android",
)))]
mod platform {
    use servo_media_gstreamer_render::Render as RenderTrait;
    use servo_media_player::PlayerError;
    use servo_media_player::context::PlayerGLContext;
    use servo_media_player::video::VideoFrame;

    pub struct RenderDummy();
    pub type Render = RenderDummy;

    pub fn create_render(_: Box<dyn PlayerGLContext>) -> Option<RenderDummy> {
        None
    }

    impl RenderTrait for RenderDummy {
        fn is_gl(&self) -> bool {
            false
        }

        fn build_frame(&self, _: gstreamer::Sample) -> Option<VideoFrame> {
            None
        }

        fn build_video_sink(
            &self,
            _: &gstreamer::Element,
            _: &gstreamer::Element,
        ) -> Result<(), PlayerError> {
            Err(PlayerError::Backend(
                "Not available videosink decorator".to_owned(),
            ))
        }
    }
}

struct GStreamerBuffer {
    frame: gstreamer_video::VideoFrame<gstreamer_video::video_frame::Readable>,
}

impl Buffer for GStreamerBuffer {
    fn to_vec(&self) -> Option<VideoFrameData> {
        let data = self.frame.plane_data(0).ok()?;
        Some(VideoFrameData::Raw(Arc::new(data.to_vec())))
    }
}

/// A video output the embedder installs for all players, for instance to
/// keep decoded frames on the GPU.  Players that get one from
/// [`VideoOutput::new_player`] do not use the GL render.
pub trait VideoOutput: Send + Sync {
    /// The output of a new player, or `None` for the default one.
    fn new_player(&self) -> Option<Box<dyn PlayerVideoOutput>>;
}

/// One player's embedder video output.
pub trait PlayerVideoOutput: Send {
    /// Set `pipeline`'s video sink, built around `appsink` (whose callbacks
    /// the player installs).  Returns false to use the default sink.
    fn setup_video_sink(
        &self,
        appsink: &gstreamer_app::AppSink,
        pipeline: &gstreamer::Element,
    ) -> bool;

    /// The frame for a sample, or `None` to map it into memory as usual.
    fn frame(&self, sample: &gstreamer::Sample) -> Option<VideoFrame>;
}

static VIDEO_OUTPUT: OnceLock<Box<dyn VideoOutput>> = OnceLock::new();

/// Install the embedder's video output.  Must happen before the first player
/// is created; returns false if one is already installed.
pub fn set_video_output(output: Box<dyn VideoOutput>) -> bool {
    VIDEO_OUTPUT.set(output).is_ok()
}

pub struct GStreamerRender {
    render: Option<platform::Render>,
    output: Option<Box<dyn PlayerVideoOutput>>,
}

impl GStreamerRender {
    pub fn new(gl_context: Box<dyn PlayerGLContext>) -> Self {
        let output = VIDEO_OUTPUT.get().and_then(|output| output.new_player());
        let render = if output.is_some() {
            None
        } else {
            platform::create_render(gl_context)
        };
        GStreamerRender { render, output }
    }

    pub fn is_gl(&self) -> bool {
        if let Some(render) = self.render.as_ref() {
            render.is_gl()
        } else {
            false
        }
    }

    pub fn get_frame_from_sample(&self, sample: gstreamer::Sample) -> Option<VideoFrame> {
        if let Some(output) = self.output.as_ref() &&
            let Some(frame) = output.frame(&sample)
        {
            return Some(frame);
        }
        if let Some(render) = self.render.as_ref() {
            render.build_frame(sample)
        } else {
            let buffer = sample.buffer_owned()?;
            let caps = sample.caps()?;
            let info = gstreamer_video::VideoInfo::from_caps(caps).ok()?;
            let frame = gstreamer_video::VideoFrame::from_buffer_readable(buffer, &info).ok()?;

            VideoFrame::new(
                info.width() as i32,
                info.height() as i32,
                Arc::new(GStreamerBuffer { frame }),
            )
        }
    }

    pub fn setup_video_sink(
        &self,
        pipeline: &gstreamer::Element,
    ) -> Result<gstreamer_app::AppSink, PlayerError> {
        let appsink = gstreamer::ElementFactory::make("appsink")
            .build()
            .map_err(|error| PlayerError::Backend(format!("appsink creation failed: {error:?}")))?
            .downcast::<gstreamer_app::AppSink>()
            .unwrap();

        if let Some(output) = self.output.as_ref() &&
            output.setup_video_sink(&appsink, pipeline)
        {
            return Ok(appsink);
        }

        if let Some(render) = self.render.as_ref() {
            render.build_video_sink(appsink.upcast_ref::<gstreamer::Element>(), pipeline)?
        } else {
            let caps = gstreamer::Caps::builder("video/x-raw")
                .field("format", gstreamer_video::VideoFormat::Bgra.to_str())
                .field("pixel-aspect-ratio", gstreamer::Fraction::from((1, 1)))
                .build();

            appsink.set_caps(Some(&caps));
            pipeline.set_property("video-sink", &appsink);
        };

        Ok(appsink)
    }
}
