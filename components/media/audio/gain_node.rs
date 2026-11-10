/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use malloc_size_of_derive::MallocSizeOf;

use crate::audio_node::{AudioNodeEngine, AudioNodeType, BlockInfo, ChannelInfo};
use crate::block::{Chunk, FRAMES_PER_BLOCK, Tick};
use crate::param::{Param, ParamType};

#[derive(Copy, Clone, Debug, MallocSizeOf)]
pub struct GainNodeOptions {
    pub gain: f32,
}

impl Default for GainNodeOptions {
    fn default() -> Self {
        GainNodeOptions { gain: 1. }
    }
}

#[derive(AudioNodeCommon)]
pub(crate) struct GainNode {
    channel_info: ChannelInfo,
    gain: Param,
}

impl GainNode {
    pub fn new(options: GainNodeOptions, channel_info: ChannelInfo) -> Self {
        Self {
            channel_info,
            gain: Param::new(options.gain),
        }
    }

    pub fn update_parameters(&mut self, info: &BlockInfo, tick: Tick) -> bool {
        self.gain.update(info, tick)
    }
}

impl AudioNodeEngine for GainNode {
    fn node_type(&self) -> AudioNodeType {
        AudioNodeType::GainNode
    }

    fn process(&mut self, mut inputs: Chunk, info: &BlockInfo) -> Chunk {
        debug_assert!(inputs.len() == 1);

        if inputs.blocks[0].is_silence() {
            // The gain's timeline runs all the same: a value due while the
            // input is silent is in force when sound comes.
            for tick in 0..FRAMES_PER_BLOCK.0 {
                self.update_parameters(info, Tick(tick));
            }
            return inputs;
        }

        {
            let mut iter = inputs.blocks[0].iter();
            let mut gain = self.gain.value();

            while let Some(mut frame) = iter.next() {
                if self.update_parameters(info, frame.tick()) {
                    gain = self.gain.value();
                }
                frame.mutate_with(|sample, _| *sample *= gain);
            }
        }
        inputs
    }

    fn get_param(&mut self, id: ParamType) -> &mut Param {
        match id {
            ParamType::Gain => &mut self.gain,
            _ => panic!("Unknown param {:?} for GainNode", id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::Block;
    use crate::param::AutomationEvent;

    #[test]
    fn the_gain_runs_its_timeline_over_silent_input() {
        let mut node = GainNode::new(GainNodeOptions { gain: 0. }, ChannelInfo::default());
        node.get_param(ParamType::Gain)
            .insert_event(AutomationEvent::SetValueAtTime(1., Tick(300)));
        for block in 0..4u64 {
            let mut chunk = Chunk::default();
            chunk.blocks.push(Block::default());
            let info = BlockInfo {
                sample_rate: 48000.,
                frame: Tick(block * 128),
                time: 0.,
                media_time: None,
            };
            node.process(chunk, &info);
        }
        assert_eq!(node.gain.value(), 1.);
    }
}
