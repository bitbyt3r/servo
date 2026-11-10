/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::sync::Arc;

use malloc_size_of_derive::MallocSizeOf;

#[derive(Clone, MallocSizeOf)]
pub enum VideoFrameData {
    Raw(#[conditional_malloc_size_of] Arc<Vec<u8>>),
    Texture(u32),
    OESTexture(u32),
    /// The pixels stay on the GPU: the embedder supplies them at composite
    /// time as the texture of this WebRender external image id (see
    /// `embedder_traits::EmbedderTextures`).  Frames like this come from an
    /// embedder-installed video output (`servo_media_gstreamer::set_video_output`).
    External(u64),
}

pub trait Buffer: Send + Sync {
    fn to_vec(&self) -> Option<VideoFrameData>;

    /// The frame's pixels in memory, read back on demand, for a frame whose
    /// data is not raw (a frame kept on the GPU that a page draws into a
    /// canvas): BGRA, top row first, `width * 4` bytes a row.  `None` if
    /// the buffer cannot do that.
    fn read_pixels(&self) -> Option<Vec<u8>> {
        None
    }
}

#[derive(Clone, MallocSizeOf)]
pub struct VideoFrame {
    width: i32,
    height: i32,
    data: VideoFrameData,
    #[ignore_malloc_size_of = "Difficult"]
    _buffer: Arc<dyn Buffer>,
    /// When this frame is meant to be shown, in CLOCK_MONOTONIC nanoseconds,
    /// if the backend knows.
    present_at: Option<u64>,
}

impl VideoFrame {
    pub fn new(width: i32, height: i32, buffer: Arc<dyn Buffer>) -> Option<Self> {
        let data = buffer.to_vec()?;
        Some(VideoFrame {
            width,
            height,
            data,
            _buffer: buffer,
            present_at: None,
        })
    }

    pub fn with_present_at(mut self, present_at: Option<u64>) -> Self {
        self.present_at = present_at;
        self
    }

    pub fn present_at(&self) -> Option<u64> {
        self.present_at
    }

    pub fn get_width(&self) -> i32 {
        self.width
    }

    pub fn get_height(&self) -> i32 {
        self.height
    }

    pub fn get_data(&self) -> Arc<Vec<u8>> {
        match self.data {
            VideoFrameData::Raw(ref data) => data.clone(),
            _ => unreachable!("invalid raw data request for texture frame"),
        }
    }

    pub fn get_texture_id(&self) -> u32 {
        match self.data {
            VideoFrameData::Texture(data) | VideoFrameData::OESTexture(data) => data,
            _ => unreachable!("invalid texture id request for raw data frame"),
        }
    }

    pub fn is_gl_texture(&self) -> bool {
        matches!(
            self.data,
            VideoFrameData::Texture(_) | VideoFrameData::OESTexture(_)
        )
    }

    pub fn is_external_oes(&self) -> bool {
        matches!(self.data, VideoFrameData::OESTexture(_))
    }

    /// Whether the pixels are in memory (`get_data`).
    pub fn is_raw(&self) -> bool {
        matches!(self.data, VideoFrameData::Raw(_))
    }

    /// The pixels of a frame that is not raw, read back on demand (see
    /// [`Buffer::read_pixels`]).
    pub fn read_pixels(&self) -> Option<Vec<u8>> {
        self._buffer.read_pixels()
    }

    /// The WebRender external image id of an embedder-supplied frame.
    pub fn external_image_id(&self) -> Option<u64> {
        match self.data {
            VideoFrameData::External(id) => Some(id),
            _ => None,
        }
    }
}

pub trait VideoFrameRenderer: Send + 'static {
    fn render(&mut self, frame: VideoFrame);
}
