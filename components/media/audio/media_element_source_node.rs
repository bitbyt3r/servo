/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use malloc_size_of_derive::MallocSizeOf;
use player::audio::AudioRenderer;

use crate::audio_node::{AudioNodeEngine, AudioNodeType, BlockInfo, ChannelInfo};
use crate::block::{Block, Chunk, FRAMES_PER_BLOCK, FRAMES_PER_BLOCK_USIZE};

#[derive(Debug, Clone, MallocSizeOf)]
pub enum MediaElementSourceNodeMessage {
    GetAudioRenderer(Sender<Arc<Mutex<dyn AudioRenderer>>>),
}

#[derive(AudioNodeCommon)]
pub(crate) struct MediaElementSourceNode {
    channel_info: ChannelInfo,
    renderer: Arc<Mutex<dyn AudioRenderer>>,
    buffers: Arc<Mutex<Vec<Vec<f32>>>>,
    playback_offset: usize,
    /// Smallest backlog (delivered but not yet played samples) seen in the
    /// current window.  Deliveries arrive in decoder-sized bursts ahead of
    /// time, so the backlog oscillates; its minimum stays near zero while
    /// playback keeps up and rises for good after a stall of the audio clock.
    backlog_min: usize,
    window_blocks: u32,
}

/// Window over which the backlog minimum is observed (one second at 48 kHz).
const RESYNC_WINDOW_BLOCKS: u32 = 375;
/// Backlog the minimum may keep before playback is moved forward to it.
const RESYNC_KEEP: usize = 2 * FRAMES_PER_BLOCK_USIZE;

impl MediaElementSourceNode {
    pub fn new(channel_info: ChannelInfo) -> Self {
        let buffers = Arc::new(Mutex::new(Vec::new()));
        let renderer = Arc::new(Mutex::new(MediaElementSourceNodeRenderer::new(
            buffers.clone(),
        )));
        Self {
            channel_info,
            renderer,
            buffers,
            playback_offset: 0,
            backlog_min: usize::MAX,
            window_blocks: 0,
        }
    }

    pub fn handle_message(&mut self, message: MediaElementSourceNodeMessage, _: f32) {
        match message {
            MediaElementSourceNodeMessage::GetAudioRenderer(sender) => {
                let _ = sender.send(self.renderer.clone());
            },
        }
    }
}

impl MediaElementSourceNode {
    /// The media element delivers audio on its own clock while this node
    /// plays it on the context's.  If the context clock stalls (the audio
    /// output could not run for a while), the backlog grows and would stay,
    /// leaving the audio permanently behind the video.  Once a second, if the
    /// backlog never dropped to a small value, skip forward to it; and free
    /// the samples already played.
    fn keep_in_sync(&mut self, buffers: &mut [Vec<f32>]) {
        let len = buffers[0].len();
        let backlog = len.saturating_sub(self.playback_offset);
        self.backlog_min = self.backlog_min.min(backlog);
        self.window_blocks += 1;
        if self.window_blocks < RESYNC_WINDOW_BLOCKS {
            return;
        }
        self.window_blocks = 0;
        if self.backlog_min > RESYNC_KEEP {
            let skip = self.backlog_min - RESYNC_KEEP;
            log::info!("media element audio: skipping {skip} samples to catch up with its source");
            self.playback_offset += skip;
        }
        self.backlog_min = usize::MAX;
        if self.playback_offset > 0 {
            for buffer in buffers.iter_mut() {
                buffer.drain(..self.playback_offset.min(buffer.len()));
            }
            self.playback_offset = 0;
        }
    }
}

impl AudioNodeEngine for MediaElementSourceNode {
    fn node_type(&self) -> AudioNodeType {
        AudioNodeType::MediaElementSourceNode
    }

    fn process(&mut self, mut inputs: Chunk, _info: &BlockInfo) -> Chunk {
        debug_assert!(inputs.is_empty());

        let shared = self.buffers.clone();
        let mut buffers = shared.lock().unwrap();
        let chans = buffers.len() as u8;

        if chans == 0 {
            inputs.blocks.push(Default::default());
            return inputs;
        }

        self.keep_in_sync(&mut buffers);
        let len = buffers[0].len();

        let frames_per_block = FRAMES_PER_BLOCK.0 as usize;
        let samples_to_copy = if self.playback_offset + frames_per_block > len {
            len - self.playback_offset
        } else {
            frames_per_block
        };
        let next_offset = self.playback_offset + samples_to_copy;
        if samples_to_copy == FRAMES_PER_BLOCK.0 as usize {
            // copy entire chan
            let mut block = Block::empty();
            for chan in 0..chans {
                block.push_chan(&buffers[chan as usize][self.playback_offset..next_offset]);
            }
            inputs.blocks.push(block)
        } else {
            // silent fill and copy
            let mut block = Block::default();
            block.repeat(chans);
            block.explicit_repeat();
            for chan in 0..chans {
                let data = block.data_chan_mut(chan);
                let (_, data) = data.split_at_mut(0);
                let (data, _) = data.split_at_mut(samples_to_copy);
                data.copy_from_slice(&buffers[chan as usize][self.playback_offset..next_offset]);
            }
            inputs.blocks.push(block)
        }

        self.playback_offset = next_offset;

        inputs
    }

    fn input_count(&self) -> u32 {
        0
    }

    make_message_handler!(MediaElementSourceNode: handle_message);
}

struct MediaElementSourceNodeRenderer {
    buffers: Arc<Mutex<Vec<Vec<f32>>>>,
    channels: HashMap<u32, usize>,
}

impl MediaElementSourceNodeRenderer {
    pub fn new(buffers: Arc<Mutex<Vec<Vec<f32>>>>) -> Self {
        Self {
            buffers,
            channels: HashMap::new(),
        }
    }
}

impl AudioRenderer for MediaElementSourceNodeRenderer {
    fn render(&mut self, sample: Box<dyn AsRef<[f32]>>, channel_pos: u32) {
        let channel = match self.channels.entry(channel_pos) {
            Entry::Occupied(entry) => *entry.get(),
            Entry::Vacant(entry) => {
                let mut buffers = self.buffers.lock().unwrap();
                let len = buffers.len();
                buffers.resize(len + 1, Vec::new());
                *entry.insert(buffers.len())
            },
        };
        self.buffers.lock().unwrap()[channel - 1].extend_from_slice((*sample).as_ref());
    }
}
