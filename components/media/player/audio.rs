/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

pub trait AudioRenderer: Send + 'static {
    /// `present_at`: when the first sample is meant to be heard, in
    /// CLOCK_MONOTONIC nanoseconds, if the backend knows.
    fn render(&mut self, sample: Box<dyn AsRef<[f32]>>, channel: u32, present_at: Option<u64>);
}
