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
use crate::block::{Block, Chunk, FRAMES_PER_BLOCK_USIZE};

#[derive(Debug, Clone, MallocSizeOf)]
pub enum MediaElementSourceNodeMessage {
    GetAudioRenderer(Sender<Arc<Mutex<dyn AudioRenderer>>>),
}

/// Audio delivered by the media element: one buffer per channel, all the
/// same length, plus the presentation time of one known sample so the rest
/// can be placed in time.
#[derive(Default)]
struct Delivered {
    channels: Vec<Vec<f32>>,
    /// (presentation time in CLOCK_MONOTONIC ns, index into the buffers) of
    /// the first timestamped delivery; later samples follow contiguously.
    anchor: Option<(u64, i64)>,
}

#[derive(AudioNodeCommon)]
pub(crate) struct MediaElementSourceNode {
    channel_info: ChannelInfo,
    renderer: Arc<Mutex<dyn AudioRenderer>>,
    delivered: Arc<Mutex<Delivered>>,
    /// Samples before this index have been played (or skipped).
    playback_offset: usize,
}

/// Played samples are freed once this many have accumulated.
const TRIM_AFTER: usize = 48_000;

impl MediaElementSourceNode {
    pub fn new(channel_info: ChannelInfo) -> Self {
        let delivered = Arc::new(Mutex::new(Delivered::default()));
        let renderer = Arc::new(Mutex::new(MediaElementSourceNodeRenderer::new(
            delivered.clone(),
        )));
        Self {
            channel_info,
            renderer,
            delivered,
            playback_offset: 0,
        }
    }

    pub fn handle_message(&mut self, message: MediaElementSourceNodeMessage, _: f32) {
        match message {
            MediaElementSourceNodeMessage::GetAudioRenderer(sender) => {
                let _ = sender.send(self.renderer.clone());
            },
        }
    }

    /// Free played samples, keeping the time anchor valid.
    fn trim(&mut self, delivered: &mut Delivered) {
        if self.playback_offset < TRIM_AFTER {
            return;
        }
        let drain = self.playback_offset.min(delivered.channels[0].len());
        for buffer in delivered.channels.iter_mut() {
            buffer.drain(..drain);
        }
        if let Some((_, index)) = delivered.anchor.as_mut() {
            *index -= drain as i64;
        }
        self.playback_offset -= drain;
    }

    /// Copy `count` samples from `start` (which may be before the beginning
    /// or past the end of what was delivered: those are silence) into a block.
    fn block_from(delivered: &Delivered, start: i64, count: usize) -> Block {
        let chans = delivered.channels.len() as u8;
        let len = delivered.channels[0].len() as i64;
        let (first, last) = (start.max(0), (start + count as i64).min(len));
        if first == start && last == start + count as i64 {
            let mut block = Block::empty();
            for chan in &delivered.channels {
                block.push_chan(&chan[first as usize..last as usize]);
            }
            return block;
        }
        let mut block = Block::default();
        block.repeat(chans);
        block.explicit_repeat();
        if first < last {
            for (c, chan) in delivered.channels.iter().enumerate() {
                let data = block.data_chan_mut(c as u8);
                let offset = (first - start) as usize;
                data[offset..offset + (last - first) as usize]
                    .copy_from_slice(&chan[first as usize..last as usize]);
            }
        }
        block
    }
}

impl AudioNodeEngine for MediaElementSourceNode {
    fn node_type(&self) -> AudioNodeType {
        AudioNodeType::MediaElementSourceNode
    }

    fn process(&mut self, mut inputs: Chunk, info: &BlockInfo) -> Chunk {
        debug_assert!(inputs.is_empty());

        let shared = self.delivered.clone();
        let mut delivered = shared.lock().unwrap();
        if delivered.channels.is_empty() {
            inputs.blocks.push(Default::default());
            return inputs;
        }
        self.trim(&mut delivered);

        let start = match (info.media_time, delivered.anchor) {
            // The sink plays on a clock the media timestamps relate to: take
            // the samples meant for the time this block will be heard.  Late
            // samples are skipped, early ones waited for, so the audio stays
            // with its video through stalls and codec block sizes alike.
            (Some(media_time), Some((anchor_time, anchor_index))) => {
                let offset = (media_time as f64 - anchor_time as f64) * info.sample_rate as f64 / 1e9;
                anchor_index + offset.round() as i64
            },
            // Otherwise play what was delivered, in order.
            _ => self.playback_offset as i64,
        };
        let block = Self::block_from(&delivered, start, FRAMES_PER_BLOCK_USIZE);
        inputs.blocks.push(block);
        self.playback_offset = (start + FRAMES_PER_BLOCK_USIZE as i64).max(0) as usize;
        inputs
    }

    fn input_count(&self) -> u32 {
        0
    }

    make_message_handler!(MediaElementSourceNode: handle_message);
}

struct MediaElementSourceNodeRenderer {
    delivered: Arc<Mutex<Delivered>>,
    channels: HashMap<u32, usize>,
}

impl MediaElementSourceNodeRenderer {
    pub fn new(delivered: Arc<Mutex<Delivered>>) -> Self {
        Self {
            delivered,
            channels: HashMap::new(),
        }
    }
}

impl AudioRenderer for MediaElementSourceNodeRenderer {
    fn render(&mut self, sample: Box<dyn AsRef<[f32]>>, channel_pos: u32, present_at: Option<u64>) {
        let mut delivered = self.delivered.lock().unwrap();
        let channel = match self.channels.entry(channel_pos) {
            Entry::Occupied(entry) => *entry.get(),
            Entry::Vacant(entry) => {
                delivered.channels.push(Vec::new());
                *entry.insert(delivered.channels.len())
            },
        };
        if channel == 1 && delivered.anchor.is_none() {
            if let Some(present_at) = present_at {
                let index = delivered.channels[0].len() as i64;
                delivered.anchor = Some((present_at, index));
            }
        }
        delivered.channels[channel - 1].extend_from_slice((*sample).as_ref());
    }
}
