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
    /// The next delivery starts a new run (see `AudioRenderer::start_run`):
    /// it anchors a timeline of its own.
    new_run: bool,
}

/// A delivery whose time is this far from where the samples before it put
/// it starts a new timeline: playback resumed, or the media seeked or
/// looped. Decoders give contiguous timestamps, so anything past rounding
/// is a new timeline.
const NEW_TIMELINE_NS: f64 = 1e6;

#[derive(AudioNodeCommon)]
pub(crate) struct MediaElementSourceNode {
    channel_info: ChannelInfo,
    renderer: Arc<Mutex<dyn AudioRenderer>>,
    delivered: Arc<Mutex<Delivered>>,
    /// Samples before this index have been played (or skipped).
    playback_offset: usize,
    /// Samples before this index have been rendered into a block, whatever
    /// timeline placed them. A new timeline's anchor puts the samples
    /// delivered before it just ahead of it, and after a pause or a seek
    /// while playing the first of those were heard already, on the timeline
    /// before: none is heard twice.
    rendered: i64,
    /// The last block left samples heard already silent: said once.
    skipping: bool,
}

/// Played samples are freed once this many have accumulated.
const TRIM_AFTER: usize = 48_000;

impl MediaElementSourceNode {
    pub fn new(channel_info: ChannelInfo, sample_rate: f32) -> Self {
        let delivered = Arc::new(Mutex::new(Delivered::default()));
        let renderer = Arc::new(Mutex::new(MediaElementSourceNodeRenderer::new(
            delivered.clone(),
            sample_rate,
        )));
        Self {
            channel_info,
            renderer,
            delivered,
            playback_offset: 0,
            rendered: 0,
            skipping: false,
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
        self.rendered -= drain as i64;
    }

    /// Copy `count` samples from `start` into a block. Those before `floor`
    /// (rendered already), before the beginning or past the end of what was
    /// delivered are silence.
    fn block_from(delivered: &Delivered, start: i64, count: usize, floor: i64) -> Block {
        let chans = delivered.channels.len() as u8;
        let len = delivered.channels[0].len() as i64;
        let (first, last) = (start.max(floor).max(0), (start + count as i64).min(len));
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
        let end = start + FRAMES_PER_BLOCK_USIZE as i64;
        let block = Self::block_from(&delivered, start, FRAMES_PER_BLOCK_USIZE, self.rendered);
        inputs.blocks.push(block);
        // A start placed before what was rendered already (on resuming, the
        // samples delivered ahead of the pause, the first of which were heard
        // before it) leaves those silent rather than repeat them.
        let skipping = self.rendered > start.max(0);
        if skipping && !std::mem::replace(&mut self.skipping, true) {
            log::info!(
                "media: a start placed {} samples heard before it ahead of it; they are not repeated",
                self.rendered - start.max(0)
            );
        }
        self.skipping = skipping;
        self.rendered = self.rendered.max(end.min(delivered.channels[0].len() as i64));
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
    /// The context's sample rate. The node plays one delivered sample per
    /// frame of the context, so samples must come at this rate.
    sample_rate: f32,
}

impl MediaElementSourceNodeRenderer {
    pub fn new(delivered: Arc<Mutex<Delivered>>, sample_rate: f32) -> Self {
        Self {
            delivered,
            channels: HashMap::new(),
            sample_rate,
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
            let new_run = std::mem::take(&mut delivered.new_run);
            let follows = !new_run &&
                match (delivered.anchor, delivered.rate) {
                    (Some((time, at)), Some(rate)) => {
                        let expected = time as f64 + (index - at) as f64 * 1e9 / rate as f64;
                        (present_at as f64 - expected).abs() <= NEW_TIMELINE_NS
                    },
                    (Some(_), None) => true,
                    (None, _) => false,
                };
            if !follows {
                delivered.anchor = Some((present_at, index));
                delivered.started = Some((index, std::time::Instant::now()));
            }
        }
        delivered.channels[channel - 1].extend_from_slice((*sample).as_ref());
    }

    fn sample_rate(&self) -> Option<u32> {
        Some(self.sample_rate.round() as u32)
    }

    fn start_run(&mut self) {
        self.delivered.lock().unwrap().new_run = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{FRAMES_PER_BLOCK, Tick};

    const RATE: f32 = 48000.;
    /// One sample at 48 kHz, in ns.
    const SAMPLE_NS: f64 = 1e9 / RATE as f64;

    fn info(block: u64, media_time: u64) -> BlockInfo {
        BlockInfo {
            sample_rate: RATE,
            frame: Tick(block * FRAMES_PER_BLOCK.0),
            time: 0.,
            media_time: Some(media_time),
        }
    }

    /// Deliver mono samples numbered `from..from + count` (each sample's
    /// value is its number), the first presented at `present_at`.
    fn deliver(node: &MediaElementSourceNode, from: u32, count: u32, present_at: u64) {
        let samples: Vec<f32> = (from..from + count).map(|n| n as f32).collect();
        node.renderer
            .lock()
            .unwrap()
            .render(Box::new(samples), 1, Some(present_at));
    }

    /// The sample numbers of a rendered block; silence is -1.
    fn heard(chunk: &Chunk) -> Vec<i64> {
        let block = &chunk.blocks[0];
        if block.is_silence() {
            return vec![-1; FRAMES_PER_BLOCK_USIZE];
        }
        block
            .data_chan(0)
            .iter()
            .map(|v| if *v == 0. { -1 } else { *v as i64 })
            .collect()
    }

    fn at(base: u64, samples: i64) -> u64 {
        (base as f64 + samples as f64 * SAMPLE_NS).round() as u64
    }

    /// Plays samples ahead of a pause, pauses, and resumes with the samples
    /// after the delivered ones on a new timeline whose time puts the
    /// samples delivered before it, some of them heard already, just ahead
    /// of it: every sample is heard once, in order.
    #[test]
    fn a_resume_does_not_repeat_samples_heard_before_the_pause() {
        let mut node = MediaElementSourceNode::new(ChannelInfo::default(), RATE);
        let t0 = 1_000_000_000_000u64;
        let block = FRAMES_PER_BLOCK_USIZE as i64;
        let mut heard_all = Vec::new();
        let mut n = 0u64;
        let mut render = |node: &mut MediaElementSourceNode, time: u64| {
            let out = node.process(Chunk::default(), &info(n, time));
            n += 1;
            heard(&out)
        };
        // Samples 1..=1920 delivered (40 ms ahead), first heard at t0. The
        // renderer needs to know the rate: one block first.
        let _ = render(&mut node, t0 - 10 * SAMPLE_NS as u64 * block as u64);
        deliver(&node, 1, 1920, t0);
        // Ten blocks heard (samples 1..=320), then the element pauses.
        for b in 0..10 {
            heard_all.extend(render(&mut node, at(t0, b * block)));
        }
        // The resume: samples from 1921 on, on a new timeline 500 ms later
        // that puts sample 1921 at t1. The 1600 samples delivered before it
        // are placed just ahead of it, 320 of which were heard already.
        let t1 = t0 + 500_000_000;
        deliver(&node, 1921, 4800, t1);
        let resume = at(t1, -1920);
        for b in 0..120 {
            heard_all.extend(render(&mut node, at(resume, b * block)));
        }
        let samples: Vec<i64> = heard_all.into_iter().filter(|s| *s >= 0).collect();
        let mut sorted = samples.clone();
        sorted.dedup();
        assert_eq!(samples, sorted, "a sample was heard twice");
        assert!(
            samples.windows(2).all(|w| w[1] == w[0] + 1),
            "samples out of order or missing"
        );
        assert_eq!(samples.first(), Some(&1));
    }

    /// A seek whose new run the player places 32 samples after where the
    /// run before would have put it (one 256-sample codec block on an
    /// 800-sample cycle can do that): the new run is placed by its own
    /// time, not taken for the samples that follow on.
    #[test]
    fn a_new_run_near_the_old_timeline_is_placed_by_its_own_time() {
        let mut node = MediaElementSourceNode::new(ChannelInfo::default(), RATE);
        let t0 = 1_000_000_000_000u64;
        let block = FRAMES_PER_BLOCK_USIZE as i64;
        let mut n = 0u64;
        let mut render = |node: &mut MediaElementSourceNode, time: u64| {
            let out = node.process(Chunk::default(), &info(n, time));
            n += 1;
            heard(&out)
        };
        let _ = render(&mut node, at(t0, -10 * block));
        deliver(&node, 1, 4000, t0);
        let mut heard_all = Vec::new();
        for b in 0..100 {
            heard_all.extend(render(&mut node, at(t0, b * block)));
        }
        // The seek: its samples (numbered from 100001) start 32 samples
        // after where sample 4001 would have been heard.
        node.renderer.lock().unwrap().start_run();
        deliver(&node, 100_001, 4000, at(t0, 4000 + 32));
        for b in 100..200 {
            heard_all.extend(render(&mut node, at(t0, b * block)));
        }
        let first = heard_all.iter().position(|s| *s == 100_001).unwrap();
        assert_eq!(
            first,
            4000 + 32,
            "the run's first sample is heard at its time"
        );
    }
}
