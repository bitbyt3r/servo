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
    /// the delivery that started the current timeline; later samples follow
    /// contiguously.
    anchor: Option<(u64, i64)>,
    /// The context's sample rate, once a block has been rendered.
    rate: Option<f32>,
    /// The index of the first sample of the newest timeline and when it
    /// came, until a block reaches it: to say whether a start (a play, a
    /// seek, a loop) came in time to be heard from its first sample.
    started: Option<(i64, std::time::Instant)>,
    /// The newest timeline's second delivery has not come yet: it places
    /// the timeline (see `MediaElementSourceNodeRenderer::render`).
    second: bool,
}

/// A delivery whose time is this far from where the samples before it put
/// it starts a new timeline: playback resumed, or the media seeked or
/// looped. Decoders give contiguous timestamps, so anything past rounding
/// is a new timeline.
const NEW_TIMELINE_NS: f64 = 1e6;

/// A start whose first delivery sits this many samples or more off the one
/// after it is placed by the later one (see `render`).
const WHOLE_SAMPLE_OFF: f64 = 0.75;

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
        if let Some((index, _)) = delivered.started.as_mut() {
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
        delivered.rate = Some(info.sample_rate);
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
        // The block that reaches the start of a timeline says whether its
        // first sample came before it was due here.
        if let Some((first, came)) = delivered.started &&
            start + FRAMES_PER_BLOCK_USIZE as i64 > first
        {
            delivered.started = None;
            if start > first {
                log::warn!(
                    "media: the first {} samples of a start came after their time and were skipped",
                    start - first
                );
            } else {
                log::info!(
                    "media: a start's first sample was rendered {:.1} ms after it came",
                    came.elapsed().as_secs_f64() * 1e3
                );
            }
        }
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
        // The first delivery anchors the samples in time, and so does each
        // that does not follow on from the ones before it. Samples delivered
        // before a new anchor stay just ahead of it: after a pause they are
        // the ones that come next.
        if channel == 1 &&
            let Some(present_at) = present_at
        {
            let index = delivered.channels[0].len() as i64;
            let off = match (delivered.anchor, delivered.rate) {
                (Some((time, at)), Some(rate)) => {
                    let expected = time as f64 + (index - at) as f64 * 1e9 / rate as f64;
                    Some(Some((present_at as f64 - expected, rate)))
                },
                (Some(_), None) => Some(None),
                (None, _) => None,
            };
            match off {
                // A timeline's first delivery after a seek is clipped at the
                // start of the segment and stamped with the segment's start,
                // though the decoder kept the sample at or before it (it
                // counts the samples to cut off rounding down), so its first
                // sample can sit a whole sample before its stamp. The
                // deliveries after it carry the decoder's own count from its
                // packets' times. So when the second delivery puts the
                // timeline a whole sample off (more than a 90 kHz tick, 0.53
                // samples at 48 kHz, which any timestamp may be off), it
                // places the timeline, if nothing of it has played yet.
                Some(Some((off_ns, rate))) if off_ns.abs() <= NEW_TIMELINE_NS => {
                    let samples = off_ns * rate as f64 / 1e9;
                    if std::mem::take(&mut delivered.second) {
                        let unplayed = delivered.started.is_some();
                        let moved = unplayed && samples.abs() >= WHOLE_SAMPLE_OFF;
                        log::info!(
                            "media: the first delivery of a start sat {:+.3} samples off the one after it{}",
                            -samples,
                            if moved {
                                "; placed by the later"
                            } else if samples.abs() >= WHOLE_SAMPLE_OFF {
                                "; it had begun to play, so it stays"
                            } else {
                                ""
                            }
                        );
                        if moved {
                            delivered.anchor = Some((present_at, index));
                        }
                    }
                },
                Some(None) => {},
                _ => {
                    delivered.anchor = Some((present_at, index));
                    delivered.started = Some((index, std::time::Instant::now()));
                    delivered.second = true;
                },
            }
        }
        delivered.channels[channel - 1].extend_from_slice((*sample).as_ref());
    }
}
