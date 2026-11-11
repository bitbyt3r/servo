/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex, Once};
use std::time;
use std::time::{Duration, Instant};

use byte_slice_cast::AsSliceOf;
use glib;
use glib::prelude::*;
use gstreamer;
use gstreamer_app;
use gstreamer_play;
use gstreamer_play::prelude::*;
use servo_base::generic_channel::{self, GenericCallback, GenericReceiver};
use servo_media::MediaInstanceError;
use servo_media_player::audio::AudioRenderer;
use servo_media_player::context::PlayerGLContext;
use servo_media_player::metadata::Metadata;
use servo_media_player::video::VideoFrameRenderer;
use servo_media_player::{
    PlaybackState, Player, PlayerError, PlayerEvent, SeekLock, SeekLockMsg, StreamType,
};
use servo_media_streams::registry::{MediaStreamId, get_stream};
use servo_media_traits::{BackendMsg, ClientContextId, MediaInstance};

use super::BACKEND_BASE_TIME;
use crate::media_stream::GStreamerMediaStream;
use crate::media_stream_source::{ServoMediaStreamSrc, register_servo_media_stream_src};
use crate::render::GStreamerRender;
use crate::source::{ServoSrc, register_servo_src};

const DEFAULT_MUTED: bool = false;
const DEFAULT_PAUSED: bool = true;
const DEFAULT_CAN_RESUME: bool = false;
const DEFAULT_PLAYBACK_RATE: f64 = 1.0;
const DEFAULT_VOLUME: f64 = 1.0;
const DEFAULT_TIME_RANGES: Vec<Range<f64>> = vec![];

const MAX_BUFFER_SIZE: i32 = 500 * 1024 * 1024;

/// How long before its time a sample reaches the renderers, in ns.
static PRESENTATION_LEAD: AtomicU64 = AtomicU64::new(0);

/// Hand every sample to the renderers `lead_ns` before its time (base time
/// plus running time), so a host that presents samples on a clock has them
/// that early.  The sinks sync to the clock with this offset; the times the
/// samples carry (`present_at`) and the element's position are unchanged.
pub fn set_presentation_lead(lead_ns: u64) {
    PRESENTATION_LEAD.store(lead_ns, Ordering::Relaxed);
}

fn presentation_offset() -> i64 {
    -(PRESENTATION_LEAD.load(Ordering::Relaxed).min(i64::MAX as u64) as i64)
}

/// The host's cycle grid: the start of a recent cycle and the cycle length,
/// in CLOCK_MONOTONIC ns. Zero until the host sets it.
static GRID_START: AtomicU64 = AtomicU64::new(0);
static GRID_PERIOD: AtomicU64 = AtomicU64::new(0);

/// The host presents samples on a grid of cycles: one starts at `cycle_ns`
/// and the next ones follow every `period_ns`. Call it every cycle. Each
/// run of a player's samples is then placed on the grid: a video frame's
/// time falls [`presentation_phase`] before the start of the cycle that
/// shows it, whatever the moment playback started.
pub fn set_presentation_grid(cycle_ns: u64, period_ns: u64) {
    GRID_START.store(cycle_ns, Ordering::Relaxed);
    GRID_PERIOD.store(period_ns, Ordering::Relaxed);
}

/// How long before the start of the cycle that shows it an aligned video
/// frame's time falls: half a cycle, as far from either edge as it can be,
/// so jitter in the cycle times cannot move a frame to the next cycle. A
/// host that plays each audio sample this long after its time hears the
/// audio of a frame in the cycle that shows the frame.
pub fn presentation_phase(period_ns: u64) -> u64 {
    period_ns / 2
}

/// A sample this far from the grid is not about to be presented (a frame
/// prerolled on an old base time): it is left alone and starts no run.
const NEAR_GRID_NS: u64 = 1_000_000_000;

/// How long the audio of a new run waits for the video to place the run.
const AUDIO_WAITS_FOR_VIDEO: Duration = Duration::from_millis(20);

/// The shift that puts a sample due at `at` on the grid: the cycle it would
/// show on unshifted (the first that starts at or after `at`) stays, and
/// its time moves to the phase before that cycle. Between minus the phase
/// and a period less the phase, so nothing shows later than unshifted.
fn grid_shift(at: u64) -> Option<i64> {
    let start = GRID_START.load(Ordering::Relaxed);
    let period = GRID_PERIOD.load(Ordering::Relaxed);
    if start == 0 || period == 0 || at.abs_diff(start) > NEAR_GRID_NS {
        return None;
    }
    let (start, period) = (start as i64, period as i64);
    let since = at as i64 - start;
    let cycles = since.div_euclid(period) + i64::from(since.rem_euclid(period) != 0);
    let cycle = start + cycles * period;
    Some(cycle - presentation_phase(period as u64) as i64 - at as i64)
}

/// Puts a player's samples on the host's cycle grid. A run of samples
/// starts whenever the pipeline gets a new base time: a play, a seek while
/// playing, a loop back to the start. The run's first video frame sets its
/// shift (see [`grid_shift`]) and every sample of the run moves by it, so
/// the audio keeps its place against the frames. The audio of a run waits
/// for that frame, briefly, rather than set a shift of its own; it sets one
/// only when the player has no video.
///
/// A loop is one run following on from the one before: the seek back to
/// the start that ends a pass (see [`Alignment::follow_on`]) places the
/// next pass's first frame on the cycle after the pass ended, its last
/// frame or its last audio sample, whichever is later (a file whose sound
/// runs on after its last frame holds that frame until the sound ends), and
/// its audio right after the pass's last sample. Without that the next
/// pass was placed from the moment of the seek, which comes the
/// presentation lead before the end of the pass (the sinks see the end of
/// the stream that early), and its first frames took the place of the last
/// frames of the pass.
#[derive(Default)]
struct Alignment {
    state: Mutex<AlignmentState>,
    placed: Condvar,
    /// The shift of the newest run, for the sinks' `ts-offset`.
    shift: AtomicI64,
    sinks: Mutex<Vec<glib::WeakRef<gstreamer::Element>>>,
    /// The element is playing (not paused): a frame prerolled now, after a
    /// seek while playing, comes again as the run's first sample, on the
    /// run's time, and is not shown early as a preroll.
    playing: AtomicBool,
}

#[derive(Default)]
struct AlignmentState {
    /// (base time, shift) of the latest runs, newest last.
    runs: VecDeque<(u64, i64)>,
    /// A video frame went through this player.
    video: bool,
    /// When the newest run's samples end as presented (CLOCK_MONOTONIC
    /// ns): the later of the end of its last video frame and of its last
    /// audio sample, as the stream ends when both have. Zero before any.
    end: u64,
    /// The next run follows on from the newest one's end (a loop).
    follow: bool,
}

impl Alignment {
    /// The next run follows on from the newest one: the media looped.
    fn follow_on(&self) {
        self.state.lock().unwrap().follow = true;
    }

    /// When a sample due at `at` (base time plus running time) and lasting
    /// `duration` is to be presented. A run starting with this sample is
    /// placed by `natural`: when a frame clipped at the start of a segment
    /// would have started.
    fn place(&self, base: u64, at: u64, natural: u64, video: bool, duration: u64) -> u64 {
        let mut state = self.state.lock().unwrap();
        state.video |= video;
        let found = |state: &AlignmentState| {
            state.runs.iter().rev().find(|(b, _)| *b == base).map(|r| r.1)
        };
        if !video && state.video && found(&state).is_none() {
            let deadline = Instant::now() + AUDIO_WAITS_FOR_VIDEO;
            while found(&state).is_none() {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                state = self.placed.wait_timeout(state, deadline - now).unwrap().0;
            }
        }
        let shift = match found(&state) {
            Some(shift) => shift,
            None => {
                // A loop starts where the pass before it ended, or when it
                // can, if that has passed.
                let follows = std::mem::take(&mut state.follow) && state.end > 0;
                let target = if follows { natural.max(state.end) } else { natural };
                let Some(grid) = grid_shift(target) else {
                    return at;
                };
                let shift = target as i64 - natural as i64 + grid;
                let period = GRID_PERIOD.load(Ordering::Relaxed);
                let phase = presentation_phase(period) as i64;
                if follows {
                    log::info!(
                        "media: the loop's next pass follows on: its first {} placed {:.2} ms \
                         after the seek put it ({:.2} ms after the pass's end)",
                        if video { "frame" } else { "audio sample" },
                        shift as f64 / 1e6,
                        (target as i64 + grid - state.end as i64) as f64 / 1e6,
                    );
                } else {
                    log::info!(
                        "media: playback placed on the cycle grid: its first {} was due {:.2} ms \
                         before the cycle that shows it, now {:.2} ms",
                        if video { "frame" } else { "audio sample" },
                        (phase + shift) as f64 / 1e6,
                        phase as f64 / 1e6,
                    );
                }
                state.runs.push_back((base, shift));
                if state.runs.len() > 4 {
                    state.runs.pop_front();
                }
                state.end = at.saturating_add_signed(shift).saturating_add(duration);
                drop(state);
                self.placed.notify_all();
                self.set_offsets(shift);
                return at.saturating_add_signed(shift);
            },
        };
        let placed = at.saturating_add_signed(shift);
        if state.runs.back().is_some_and(|r| r.0 == base) {
            state.end = state.end.max(placed.saturating_add(duration));
        }
        placed
    }

    fn offset(&self) -> i64 {
        presentation_offset() + self.shift.load(Ordering::Relaxed)
    }

    /// The sinks hand samples over the presentation lead before their
    /// shifted time.
    fn set_offsets(&self, shift: i64) {
        self.shift.store(shift, Ordering::Relaxed);
        let want = self.offset();
        for sink in self.sinks.lock().unwrap().iter().filter_map(|s| s.upgrade()) {
            if sink.property::<i64>("ts-offset") != want {
                sink.set_property("ts-offset", want);
            }
        }
    }
}

/// Sets a sink's `ts-offset` to the presentation lead (less the shift of the
/// current run) and keeps it there: playsink sets both sinks' offsets (from
/// its `av-offset`, 0) once it has an audio and a video chain.
fn keep_presentation_offset(sink: &gstreamer::Element, alignment: &Arc<Alignment>) {
    alignment.sinks.lock().unwrap().push(sink.downgrade());
    sink.set_property("ts-offset", alignment.offset());
    let alignment = Arc::downgrade(alignment);
    sink.connect_notify(Some("ts-offset"), move |sink, _| {
        let Some(alignment) = alignment.upgrade() else {
            return;
        };
        let want = alignment.offset();
        if sink.property::<i64>("ts-offset") != want {
            sink.set_property("ts-offset", want);
        }
    });
}

fn metadata_from_media_info(media_info: &gstreamer_play::PlayMediaInfo) -> Result<Metadata, ()> {
    let dur = media_info.duration();
    let duration = if let Some(dur) = dur {
        let mut nanos = dur.nseconds();
        nanos %= 1_000_000_000;
        let seconds = dur.seconds();
        Some(time::Duration::new(seconds, nanos as u32))
    } else {
        None
    };

    let mut audio_tracks = Vec::new();
    let mut video_tracks = Vec::new();

    let format = media_info
        .container_format()
        .unwrap_or_else(|| glib::GString::from(""))
        .to_string();

    for stream_info in media_info.stream_list() {
        let stream_type = stream_info.stream_type();
        match stream_type.as_str() {
            "audio" => {
                let codec = stream_info
                    .codec()
                    .unwrap_or_else(|| glib::GString::from(""))
                    .to_string();
                audio_tracks.push(codec);
            },
            "video" => {
                let codec = stream_info
                    .codec()
                    .unwrap_or_else(|| glib::GString::from(""))
                    .to_string();
                video_tracks.push(codec);
            },
            _ => {},
        }
    }

    let mut width: u32 = 0;
    let height: u32 = if media_info.number_of_video_streams() > 0 {
        let first_video_stream = &media_info.video_streams()[0];
        width = first_video_stream.width() as u32;
        first_video_stream.height() as u32
    } else {
        0
    };

    let is_seekable = media_info.is_seekable();
    let is_live = media_info.is_live();
    let title = media_info.title().map(|s| s.as_str().to_string());

    Ok(Metadata {
        duration,
        width,
        height,
        format,
        is_seekable,
        audio_tracks,
        video_tracks,
        is_live,
        title,
    })
}

pub struct GStreamerAudioChunk(gstreamer::buffer::MappedBuffer<gstreamer::buffer::Readable>);
impl AsRef<[f32]> for GStreamerAudioChunk {
    fn as_ref(&self) -> &[f32] {
        self.0.as_ref().as_slice_of::<f32>().unwrap_or_default()
    }
}

#[derive(PartialEq)]
enum PlayerSource {
    Seekable(ServoSrc),
    Stream(ServoMediaStreamSrc),
}

struct PlayerInner {
    player: gstreamer_play::Play,
    signal_adapter: gstreamer_play::PlaySignalAdapter,
    source: Option<PlayerSource>,
    video_sink: gstreamer_app::AppSink,
    input_size: u64,
    seekable: bool,
    play_state: gstreamer_play::PlayState,
    paused: Cell<bool>,
    can_resume: Cell<bool>,
    playback_rate: Cell<f64>,
    muted: Cell<bool>,
    volume: Cell<f64>,
    stream_type: StreamType,
    last_metadata: Option<Metadata>,
    cat: gstreamer::DebugCategory,
    enough_data: Arc<AtomicBool>,
    /// Where the media timeline starts: the first frame is at time 0.
    origin: Arc<Mutex<Origin>>,
    /// Set at the end of the stream, cleared by a seek or a play.
    eos: Arc<AtomicBool>,
    /// Where the player's samples are presented.
    alignment: Arc<Alignment>,
}

impl PlayerInner {
    pub fn set_input_size(&mut self, size: u64) -> Result<(), PlayerError> {
        // Set input_size to proxy its value, since it
        // could be set by the user before calling .setup().
        self.input_size = size;
        if let Some(PlayerSource::Seekable(ref mut source)) = self.source {
            source.set_size(if size > 0 {
                size as i64
            } else {
                -1 // live source
            });
        }
        Ok(())
    }

    pub fn set_seekable(&mut self, seekable: bool) -> Result<(), PlayerError> {
        self.seekable = seekable;
        if let Some(PlayerSource::Seekable(ref mut source)) = self.source {
            source.set_seekable(seekable);
        }
        Ok(())
    }

    pub fn set_mute(&mut self, muted: bool) -> Result<(), PlayerError> {
        if self.muted.get() == muted {
            return Ok(());
        }

        self.muted.set(muted);
        self.player.set_mute(muted);
        Ok(())
    }

    pub fn muted(&self) -> bool {
        self.muted.get()
    }

    pub fn set_playback_rate(&mut self, playback_rate: f64) -> Result<(), PlayerError> {
        if self.stream_type != StreamType::Seekable {
            return Err(PlayerError::NonSeekableStream);
        }

        if self.playback_rate.get() == playback_rate {
            return Ok(());
        }

        self.playback_rate.set(playback_rate);

        // The new playback rate will not be passed to the pipeline if the
        // current GstPlay state is less than GST_STATE_PAUSED, which will be
        // set immediately before the initial gstreamer_play_MESSAGE_MEDIA_INFO_UPDATED
        // message is posted to bus.
        if self.last_metadata.is_some() {
            self.player.set_rate(playback_rate);
        }
        Ok(())
    }

    pub fn playback_rate(&self) -> f64 {
        self.playback_rate.get()
    }

    pub fn play(&mut self) -> Result<(), PlayerError> {
        if !self.paused.get() {
            return Ok(());
        }

        self.paused.set(false);
        self.can_resume.set(false);
        self.eos.store(false, Ordering::Relaxed);
        self.alignment.playing.store(true, Ordering::Relaxed);
        self.player.play();
        Ok(())
    }

    pub fn stop(&mut self) -> Result<(), PlayerError> {
        self.player.stop();
        self.paused.set(true);
        self.alignment.playing.store(false, Ordering::Relaxed);
        self.can_resume.set(false);
        self.last_metadata = None;
        self.source = None;
        Ok(())
    }

    pub fn pause(&mut self) -> Result<(), PlayerError> {
        if self.paused.get() {
            return Ok(());
        }

        self.paused.set(true);
        self.can_resume.set(true);
        self.alignment.playing.store(false, Ordering::Relaxed);
        // GstPlay seeks back to the start when paused after the end of the
        // stream, so the element would show its first frame. The element
        // pauses itself at the end and must keep showing its last frame.
        // The pipeline stays at the end; a later play starts from the
        // beginning, as GstPlay does after the end anyway.
        if !self.eos.load(Ordering::Relaxed) {
            self.player.pause();
        }
        Ok(())
    }

    pub fn paused(&self) -> bool {
        self.paused.get()
    }

    pub fn can_resume(&self) -> bool {
        self.can_resume.get()
    }

    pub fn position(&self) -> Option<f64> {
        let position = self.player.position()?;
        Some(self.origin.lock().unwrap().media_time(position))
    }

    pub fn end_of_stream(&mut self) -> Result<(), PlayerError> {
        match self.source {
            Some(ref mut source) => {
                if let PlayerSource::Seekable(source) = source {
                    source
                        .push_end_of_stream()
                        .map(|_| ())
                        .map_err(|_| PlayerError::EOSFailed)
                } else {
                    Ok(())
                }
            },
            _ => Ok(()),
        }
    }

    pub fn seek(&mut self, time: f64) -> Result<(), PlayerError> {
        if self.stream_type != StreamType::Seekable {
            return Err(PlayerError::NonSeekableStream);
        }
        if let Some(ref metadata) = self.last_metadata &&
            let Some(ref duration) = metadata.duration &&
            duration < &time::Duration::new(time as u64, 0)
        {
            gstreamer::warning!(self.cat, obj = &self.player, "Trying to seek out of range");
            return Err(PlayerError::SeekOutOfRange);
        }

        let time = time * 1_000_000_000.;
        let origin = self.origin.lock().unwrap().time();
        // A pause at the end left the pipeline playing: pause it now, so the
        // seek leaves the element paused. A seek at the end while playing is
        // a loop: the next pass follows on from this one.
        if self.eos.swap(false, Ordering::Relaxed) {
            if self.paused.get() {
                self.player.pause();
            } else {
                self.alignment.follow_on();
            }
        }
        self.player
            .seek(gstreamer::ClockTime::from_nseconds(time as u64) + origin);
        Ok(())
    }

    pub fn set_volume(&mut self, volume: f64) -> Result<(), PlayerError> {
        if self.volume.get() == volume {
            return Ok(());
        }

        self.volume.set(volume);
        self.player.set_volume(volume);
        Ok(())
    }

    pub fn volume(&self) -> f64 {
        self.volume.get()
    }

    pub fn push_data(&mut self, data: Vec<u8>) -> Result<(), PlayerError> {
        if let Some(PlayerSource::Seekable(ref mut source)) = self.source {
            if self.enough_data.load(Ordering::Relaxed) {
                return Err(PlayerError::EnoughData);
            }
            return source
                .push_buffer(data)
                .map(|_| ())
                .map_err(|_| PlayerError::BufferPushFailed);
        }
        Err(PlayerError::BufferPushFailed)
    }

    pub fn set_src(&mut self, source: PlayerSource) {
        self.source = Some(source);
    }

    pub fn buffered(&self) -> Vec<Range<f64>> {
        let mut buffered_ranges = vec![];

        let Some(duration) = self
            .last_metadata
            .as_ref()
            .and_then(|metadata| metadata.duration)
        else {
            return buffered_ranges;
        };

        let pipeline = self.player.pipeline();
        let mut buffering = gstreamer::query::Buffering::new(gstreamer::Format::Percent);
        if pipeline.query(&mut buffering) {
            let ranges = buffering.ranges();
            for (start, end) in ranges {
                let start = (if let gstreamer::GenericFormattedValue::Percent(start) = start {
                    start.unwrap()
                } else {
                    gstreamer::format::Percent::from_percent(0)
                } / gstreamer::format::Percent::MAX) as f64 *
                    duration.as_secs_f64();
                let end = (if let gstreamer::GenericFormattedValue::Percent(end) = end {
                    end.unwrap()
                } else {
                    gstreamer::format::Percent::from_percent(0)
                } / gstreamer::format::Percent::MAX) as f64 *
                    duration.as_secs_f64();
                buffered_ranges.push(Range { start, end });
            }
        }

        buffered_ranges
    }

    pub fn seekable(&self) -> Vec<Range<f64>> {
        // if the servosrc is seekable, we should return the duration of the media
        // (all of it, when the duration is not known yet: a resource still
        // growing, or one whose duration comes just after its metadata)
        // GstPlay asks whether the media is seekable once, early; a demuxer
        // in push mode (Matroska) only says so once it has read further, so
        // ask the pipeline again while it said no.
        if let Some(metadata) = self.last_metadata.as_ref() &&
            (metadata.is_seekable || self.pipeline_seekable())
        {
            return vec![Range {
                start: 0.0,
                end: metadata
                    .duration
                    .map_or(f64::INFINITY, |duration| duration.as_secs_f64()),
            }];
        }

        // if the servosrc is not seekable, we should return the buffered range
        self.buffered()
    }

    fn pipeline_seekable(&self) -> bool {
        if self.stream_type != StreamType::Seekable {
            return false;
        }
        let mut query = gstreamer::query::Seeking::new(gstreamer::Format::Time);
        let seekable = self.player.pipeline().query(&mut query) && query.result().0;
        gstreamer::debug!(self.cat, obj = &self.player, "Seekable now: {seekable}");
        seekable
    }

    fn set_stream(&mut self, stream: &MediaStreamId, only_stream: bool) -> Result<(), PlayerError> {
        debug_assert!(self.stream_type == StreamType::Stream);
        let Some(PlayerSource::Stream(ref source)) = self.source else {
            return Err(PlayerError::SetStreamFailed);
        };

        let stream = get_stream(stream).expect("Media streams registry does not contain such ID");
        let mut stream = stream.lock().unwrap();
        let Some(stream) = stream.as_mut_any().downcast_mut::<GStreamerMediaStream>() else {
            return Err(PlayerError::SetStreamFailed);
        };

        let playbin = self
            .player
            .pipeline()
            .dynamic_cast::<gstreamer::Pipeline>()
            .unwrap();
        let clock = gstreamer::SystemClock::obtain();
        playbin.set_base_time(*BACKEND_BASE_TIME);
        playbin.set_start_time(gstreamer::ClockTime::NONE);
        playbin.use_clock(Some(&clock));
        source
            .set_stream(stream, only_stream)
            .map_err(|_| PlayerError::SetStreamFailed)
    }

    fn set_audio_track(&mut self, stream_index: i32, enabled: bool) -> Result<(), PlayerError> {
        self.player
            .set_audio_track(stream_index)
            .map_err(|_| PlayerError::SetTrackFailed)?;
        self.player.set_audio_track_enabled(enabled);
        Ok(())
    }

    fn set_video_track(&mut self, stream_index: i32, enabled: bool) -> Result<(), PlayerError> {
        self.player
            .set_video_track(stream_index)
            .map_err(|_| PlayerError::SetTrackFailed)?;
        self.player.set_video_track_enabled(enabled);
        Ok(())
    }
}

macro_rules! notify(
    ($observer:expr_2021, $event:expr_2021) => {
        $observer.lock().unwrap().send($event)
    };
);

struct SeekChannel {
    sender: SeekLock,
    recv: GenericReceiver<SeekLockMsg>,
}

impl SeekChannel {
    fn new() -> Self {
        let (sender, recv) =
            generic_channel::channel::<SeekLockMsg>().expect("Couldn't create IPC channel");
        Self {
            sender: SeekLock {
                lock_channel: sender,
            },
            recv,
        }
    }

    fn sender(&self) -> SeekLock {
        self.sender.clone()
    }

    fn _await(&self) -> SeekLockMsg {
        self.recv.recv().unwrap()
    }
}

pub struct GStreamerPlayer {
    /// The player unique ID.
    id: usize,
    /// The ID of the client context this player belongs to.
    context_id: ClientContextId,
    /// Channel to communicate with the owner GStreamerBackend instance.
    backend_chan: Arc<Mutex<Sender<BackendMsg>>>,
    inner: RefCell<Option<Arc<Mutex<PlayerInner>>>>,
    observer: Arc<Mutex<GenericCallback<PlayerEvent>>>,
    audio_renderer: Option<Arc<Mutex<dyn AudioRenderer>>>,
    video_renderer: Option<Arc<Mutex<dyn VideoFrameRenderer>>>,
    /// Indicates whether the setup was succesfully performed and
    /// we are ready to consume a/v data.
    is_ready: Arc<Once>,
    /// Indicates whether the type of media stream to be played is a live stream.
    stream_type: StreamType,
    /// Decorator used to setup the video sink and process the produced frames.
    render: Arc<Mutex<GStreamerRender>>,
    /// Whether to enable download buffering. This is only enabled heuristically for some media,
    /// because it causes issues in certain situations. See <https://github.com/servo/servo/issues/23241>.
    download_buffering_enabled: AtomicBool,
}

impl GStreamerPlayer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: usize,
        context_id: &ClientContextId,
        backend_chan: Arc<Mutex<Sender<BackendMsg>>>,
        stream_type: StreamType,
        observer: GenericCallback<PlayerEvent>,
        video_renderer: Option<Arc<Mutex<dyn VideoFrameRenderer>>>,
        audio_renderer: Option<Arc<Mutex<dyn AudioRenderer>>>,
        gl_context: Box<dyn PlayerGLContext>,
    ) -> GStreamerPlayer {
        let _ = gstreamer::DebugCategory::new(
            "servoplayer",
            gstreamer::DebugColorFlags::empty(),
            Some("Servo player"),
        );

        Self {
            id,
            context_id: *context_id,
            backend_chan,
            inner: RefCell::new(None),
            observer: Arc::new(Mutex::new(observer)),
            audio_renderer,
            video_renderer,
            is_ready: Arc::new(Once::new()),
            stream_type,
            render: Arc::new(Mutex::new(GStreamerRender::new(gl_context))),
            download_buffering_enabled: AtomicBool::new(false),
        }
    }

    fn setup(&self) -> Result<(), PlayerError> {
        if self.inner.borrow().is_some() {
            return Ok(());
        }

        // Check that we actually have the elements that we
        // need to make this work.
        for element in ["playbin3", "decodebin3", "queue"] {
            if gstreamer::ElementFactory::find(element).is_none() {
                return Err(PlayerError::Backend(format!(
                    "Missing dependency: {}",
                    element
                )));
            }
        }

        let player = gstreamer_play::Play::default();
        let signal_adapter = gstreamer_play::PlaySignalAdapter::new_sync_emit(&player);
        let pipeline = player.pipeline();

        // FIXME(#282): The progressive downloading breaks playback on Windows and Android.
        if !cfg!(any(target_os = "windows", target_os = "android")) &&
            self.download_buffering_enabled.load(Ordering::Relaxed)
        {
            // Set player to perform progressive downloading. This will make the
            // player store the downloaded media in a local temporary file for
            // faster playback of already-downloaded chunks.
            let flags = pipeline.property_value("flags");
            let flags_class = match glib::FlagsClass::with_type(flags.type_()) {
                Some(flags) => flags,
                None => {
                    return Err(PlayerError::Backend(
                        "FlagsClass creation failed".to_owned(),
                    ));
                },
            };
            let flags_class = match flags_class.builder_with_value(flags) {
                Some(class) => class,
                None => {
                    return Err(PlayerError::Backend(
                        "FlagsClass creation failed".to_owned(),
                    ));
                },
            };
            let Some(flags) = flags_class.set_by_nick("download").build() else {
                return Err(PlayerError::Backend(
                    "FlagsClass creation failed".to_owned(),
                ));
            };
            pipeline.set_property_from_value("flags", &flags);
        }

        let origin = Arc::new(Mutex::new(Origin::default()));
        let eos = Arc::new(AtomicBool::new(false));
        let alignment = Arc::new(Alignment::default());

        // Set max size for the player buffer.
        pipeline.set_property("buffer-size", MAX_BUFFER_SIZE);

        // Set player position interval update to 0.5 seconds.
        let mut config = player.config();
        config.set_position_update_interval(500u32);
        // A seek lands on the frame asked for. Without this an MPEG-TS
        // demuxer starts from a point 2.5 s earlier and calls that the
        // position, and other demuxers snap to a keyframe.
        config.set_seek_accurate(true);
        player
            .set_config(config)
            .map_err(|e| PlayerError::Backend(e.to_string()))?;

        if let Some(ref audio_renderer) = self.audio_renderer {
            let audio_sink =
                gstreamer::ElementFactory::make("appsink")
                    .build()
                    .map_err(|error| {
                        PlayerError::Backend(format!("appsink creation failed: {error:?}"))
                    })?;

            keep_presentation_offset(&audio_sink, &alignment);
            pipeline.set_property("audio-sink", &audio_sink);

            let audio_sink = audio_sink.dynamic_cast::<gstreamer_app::AppSink>().unwrap();
            // The renderer wants f32 samples, one slice per channel; let the
            // pipeline convert whatever the decoder produces.
            audio_sink.set_caps(Some(
                &gstreamer_audio::AudioCapsBuilder::new()
                    .format(gstreamer_audio::AUDIO_FORMAT_F32)
                    .layout(gstreamer_audio::AudioLayout::Interleaved)
                    .build(),
            ));

            let weak_audio_renderer = Arc::downgrade(audio_renderer);
            let audio_origin = origin.clone();
            let audio_alignment = alignment.clone();

            audio_sink.set_callbacks(
                gstreamer_app::AppSinkCallbacks::builder()
                    .new_preroll(|_| Ok(gstreamer::FlowSuccess::Ok))
                    .new_sample(move |audio_sink| {
                        let sample = audio_sink
                            .pull_sample()
                            .map_err(|_| gstreamer::FlowError::Eos)?;
                        audio_origin.lock().unwrap().note(&sample, false);
                        let buffer = sample.buffer_owned().ok_or(gstreamer::FlowError::Error)?;
                        let audio_info = sample
                            .caps()
                            .and_then(|caps| gstreamer_audio::AudioInfo::from_caps(caps).ok())
                            .ok_or(gstreamer::FlowError::Error)?;
                        let positions =
                            audio_info.positions().ok_or(gstreamer::FlowError::Error)?;

                        let Some(audio_renderer) = weak_audio_renderer.upgrade() else {
                            return Err(gstreamer::FlowError::Flushing);
                        };

                        let present_at = running_time(&sample, buffer.pts())
                            .zip(audio_sink.base_time())
                            .map(|(running, base)| {
                                let at = base.nseconds() + running.nseconds();
                                let duration = buffer.duration().map_or_else(
                                    || {
                                        let frames = buffer.size() as u64 /
                                            audio_info.bpf().max(1) as u64;
                                        frames * 1_000_000_000 / audio_info.rate().max(1) as u64
                                    },
                                    |d| d.nseconds(),
                                );
                                audio_alignment.place(base.nseconds(), at, at, false, duration)
                            });
                        let map = buffer
                            .into_mapped_buffer_readable()
                            .map_err(|_| gstreamer::FlowError::Error)?;
                        let samples = GStreamerAudioChunk(map);
                        let interleaved = samples.as_ref();
                        let channels = positions.len().max(1);
                        for (index, position) in positions.iter().enumerate() {
                            let planar: Vec<f32> = interleaved
                                .iter()
                                .skip(index)
                                .step_by(channels)
                                .copied()
                                .collect();
                            let channel = position.to_mask() as u32;
                            audio_renderer
                                .lock()
                                .unwrap()
                                .render(Box::new(planar), channel, present_at);
                        }
                        Ok(gstreamer::FlowSuccess::Ok)
                    })
                    .build(),
            );
        } else if !crate::system_audio_output() {
            // No renderer: the page did not route this element's audio, and
            // the host forbids the system's output. playbin would pick
            // one (autoaudiosink: PulseAudio, then ALSA), so give it a sink
            // that discards the audio on the clock instead.
            let audio_sink = crate::discarding_audio_sink().map_err(|error| {
                PlayerError::Backend(format!("fakesink creation failed: {error:?}"))
            })?;
            keep_presentation_offset(&audio_sink, &alignment);
            pipeline.set_property("audio-sink", &audio_sink);
        }

        let video_sink = self.render.lock().unwrap().setup_video_sink(&pipeline)?;
        keep_presentation_offset(video_sink.upcast_ref(), &alignment);

        // There's a known bug in gstreamer that may cause a wrong transition
        // to the ready state while setting the uri property:
        // https://cgit.freedesktop.org/gstreamer/gst-plugins-bad/commit/?id=afbbc3a97ec391c6a582f3c746965fdc3eb3e1f3
        // This may affect things like setting the config, so until the bug is
        // fixed, make sure that state dependent code happens before this line.
        // The estimated version for the fix is 1.14.5 / 1.15.1.
        // https://github.com/servo/servo/issues/22010#issuecomment-432599657
        let uri = match self.stream_type {
            StreamType::Stream => {
                register_servo_media_stream_src().map_err(|error| {
                    PlayerError::Backend(format!(
                        "servomediastreamsrc registration error: {error:?}"
                    ))
                })?;
                "mediastream://".to_value()
            },
            StreamType::Seekable => {
                register_servo_src().map_err(|error| {
                    PlayerError::Backend(format!("servosrc registration error: {error:?}"))
                })?;
                "servosrc://".to_value()
            },
        };
        player.set_property("uri", &uri);

        // No video_renderers no video
        if self.video_renderer.is_none() {
            player.set_video_track_enabled(false);
        }

        *self.inner.borrow_mut() = Some(Arc::new(Mutex::new(PlayerInner {
            player,
            signal_adapter: signal_adapter.clone(),
            source: None,
            video_sink,
            input_size: 0,
            seekable: true,
            play_state: gstreamer_play::PlayState::Stopped,
            paused: Cell::new(DEFAULT_PAUSED),
            can_resume: Cell::new(DEFAULT_CAN_RESUME),
            playback_rate: Cell::new(DEFAULT_PLAYBACK_RATE),
            muted: Cell::new(DEFAULT_MUTED),
            volume: Cell::new(DEFAULT_VOLUME),
            stream_type: self.stream_type,
            last_metadata: None,
            cat: gstreamer::DebugCategory::get("servoplayer").unwrap(),
            enough_data: Arc::new(AtomicBool::new(false)),
            origin: origin.clone(),
            eos: eos.clone(),
            alignment: alignment.clone(),
        })));

        let inner = self.inner.borrow();
        let inner = inner.as_ref().unwrap();
        let observer = self.observer.clone();
        // Handle `end-of-stream` signal.
        signal_adapter.connect_end_of_stream(move |_| {
            eos.store(true, Ordering::Relaxed);
            let _ = notify!(observer, PlayerEvent::EndOfStream);
        });

        let observer = self.observer.clone();
        // Handle `error` signal
        signal_adapter.connect_error(move |_self, error, _details| {
            let _ = notify!(observer, PlayerEvent::Error(error.to_string()));
        });

        let inner_clone = inner.clone();
        let observer = self.observer.clone();
        // Handle `state-changed` signal.
        signal_adapter.connect_state_changed(move |_, play_state| {
            inner_clone.lock().unwrap().play_state = play_state;

            let state = match play_state {
                gstreamer_play::PlayState::Buffering => Some(PlaybackState::Buffering),
                gstreamer_play::PlayState::Stopped => Some(PlaybackState::Stopped),
                gstreamer_play::PlayState::Paused => Some(PlaybackState::Paused),
                gstreamer_play::PlayState::Playing => Some(PlaybackState::Playing),
                _ => None,
            };
            if let Some(v) = state {
                let _ = notify!(observer, PlayerEvent::StateChanged(v));
            }
        });

        let observer = self.observer.clone();
        let position_origin = origin.clone();
        // Handle `position-update` signal.
        signal_adapter.connect_position_updated(move |_, position| {
            if let Some(position) = position {
                let seconds = position_origin.lock().unwrap().media_time(position);
                let _ = notify!(observer, PlayerEvent::PositionChanged(seconds));
            }
        });

        let observer = self.observer.clone();
        let seek_origin = origin.clone();
        // Handle `seek-done` signal.
        signal_adapter.connect_seek_done(move |_, position| {
            let seconds = seek_origin.lock().unwrap().media_time(position);
            let _ = notify!(observer, PlayerEvent::SeekDone(seconds));
        });

        // Handle `media-info-updated` signal.
        let inner_clone = inner.clone();
        let observer = self.observer.clone();
        signal_adapter.connect_media_info_updated(move |_, info| {
            let Ok(metadata) = metadata_from_media_info(info) else {
                return;
            };

            let mut inner = inner_clone.lock().unwrap();

            if inner.last_metadata.as_ref() == Some(&metadata) {
                return;
            }

            // TODO: Workaround to generate expected `paused` state change event.
            // <https://github.com/servo/servo/issues/40740>
            let mut send_pause_event = false;

            if inner.last_metadata.is_none() {
                if metadata.is_seekable && inner.playback_rate.get() != DEFAULT_PLAYBACK_RATE {
                    // The `paused` state change event will be fired after the
                    // seek initiated by the playback rate change has
                    // completed.
                    inner.player.set_rate(inner.playback_rate.get());
                } else if inner.play_state == gstreamer_play::PlayState::Paused {
                    // We need to send the paused event for non seekable push streams.
                    send_pause_event = true;
                }
            }

            inner.last_metadata = Some(metadata.clone());
            gstreamer::info!(
                inner.cat,
                obj = &inner.player,
                "Metadata updated: {:?}",
                metadata
            );
            let _ = notify!(observer, PlayerEvent::MetadataUpdated(metadata));

            if send_pause_event {
                let _ = notify!(observer, PlayerEvent::StateChanged(PlaybackState::Paused));
            }
        });

        // Handle `duration-changed` signal.
        let inner_clone = inner.clone();
        let observer = self.observer.clone();
        signal_adapter.connect_duration_changed(move |_, duration| {
            let duration = duration.map(|duration| {
                time::Duration::new(
                    duration.seconds(),
                    (duration.nseconds() % 1_000_000_000) as u32,
                )
            });

            let mut inner = inner_clone.lock().unwrap();
            if let Some(ref mut metadata) = inner.last_metadata &&
                metadata.duration != duration
            {
                metadata.duration = duration;
                gstreamer::info!(
                    inner.cat,
                    obj = &inner.player,
                    "Duration changed: {:?}",
                    duration
                );
                let _ = notify!(observer, PlayerEvent::DurationChanged(duration));
            }
        });

        if let Some(video_renderer) = self.video_renderer.clone() {
            // Creates a closure that renders a frame using the video_renderer
            // Used in the preroll and sample callbacks
            let render_sample = {
                let render = self.render.clone();
                let observer = self.observer.clone();
                let weak_video_renderer = Arc::downgrade(&video_renderer);
                let video_origin = origin.clone();

                move |sample: gstreamer::Sample, present_at: Option<u64>| {
                    video_origin.lock().unwrap().note(&sample, true);
                    let Some(frame) = render.lock().unwrap().get_frame_from_sample(sample) else {
                        return Err(gstreamer::FlowError::Error);
                    };
                    let frame = frame.with_present_at(present_at);

                    match weak_video_renderer.upgrade() {
                        Some(video_renderer) => {
                            video_renderer.lock().unwrap().render(frame);
                        },
                        _ => {
                            return Err(gstreamer::FlowError::Flushing);
                        },
                    };

                    let _ = notify!(observer, PlayerEvent::VideoFrameUpdated);
                    Ok(gstreamer::FlowSuccess::Ok)
                }
            };

            // When a sample is meant to be shown: pipeline base time plus its
            // running time, on the system (monotonic) clock, placed on the
            // host's cycle grid.
            let preroll_alignment = alignment.clone();
            let present_at = move |sink: &gstreamer_app::AppSink, sample: &gstreamer::Sample| {
                let buffer = sample.buffer()?;
                let running = running_time(sample, buffer.pts())?;
                let base = sink.base_time()?.nseconds();
                let at = base + running.nseconds();
                let natural = natural_start(sample, buffer)
                    .map_or(at, |start| base.saturating_add_signed(start));
                let duration = frame_duration(sample, buffer).unwrap_or(0);
                Some(alignment.place(base, at, natural, true, duration))
            };

            // Set video_sink callbacks.
            inner.lock().unwrap().video_sink.set_callbacks(
                gstreamer_app::AppSinkCallbacks::builder()
                    .new_preroll({
                        let render_sample = render_sample.clone();
                        let present_at = present_at.clone();
                        let preroll_alignment = preroll_alignment.clone();
                        move |video_sink| {
                            let sample = video_sink
                                .pull_preroll()
                                .map_err(|_| gstreamer::FlowError::Eos)?;
                            // Prerolled while playing (a seek or a loop): the
                            // sink hands the frame again as the new run's
                            // first sample, on its time. Shown now, on the
                            // old run's time, it would cut the frames still
                            // to show before it.
                            if preroll_alignment.playing.load(Ordering::Relaxed) {
                                return Ok(gstreamer::FlowSuccess::Ok);
                            }
                            let at = present_at(video_sink, &sample);
                            render_sample(sample, at)
                        }
                    })
                    .new_sample(move |video_sink| {
                        let sample = video_sink
                            .pull_sample()
                            .map_err(|_| gstreamer::FlowError::Eos)?;
                        let at = present_at(video_sink, &sample);
                        render_sample(sample, at)
                    })
                    .build(),
            );
        };

        let (receiver, error_handler_id) = {
            let inner_clone = inner.clone();
            let inner = inner.lock().unwrap();
            let pipeline = inner.player.pipeline();

            let (sender, receiver) = mpsc::channel();

            let sender = Arc::new(Mutex::new(sender));
            let sender_clone = sender.clone();
            let is_ready_clone = self.is_ready.clone();
            let observer = self.observer.clone();
            pipeline.connect("source-setup", false, move |args| {
                let source = args[1].get::<gstreamer::Element>().unwrap();

                let mut inner = inner_clone.lock().unwrap();
                let source = match inner.stream_type {
                    StreamType::Seekable => {
                        let servosrc = source
                            .dynamic_cast::<ServoSrc>()
                            .expect("Source element is expected to be a ServoSrc!");

                        if inner.input_size > 0 {
                            servosrc.set_size(inner.input_size as i64);
                        }
                        servosrc.set_seekable(inner.seekable);

                        let sender_clone = sender.clone();
                        let is_ready = is_ready_clone.clone();
                        let observer_ = observer.clone();
                        let observer__ = observer.clone();
                        let observer___ = observer.clone();
                        let servosrc_ = servosrc.clone();
                        let enough_data_ = inner.enough_data.clone();
                        let enough_data__ = inner.enough_data.clone();
                        let seek_channel = Arc::new(Mutex::new(SeekChannel::new()));
                        servosrc.set_callbacks(
                            gstreamer_app::AppSrcCallbacks::builder()
                                .need_data(move |_, _| {
                                    // We block the caller of the setup method until we get
                                    // the first need-data signal, so we ensure that we
                                    // don't miss any data between the moment the client
                                    // calls setup and the player is actually ready to
                                    // get any data.
                                    is_ready.call_once(|| {
                                        let _ = sender_clone.lock().unwrap().send(Ok(()));
                                    });

                                    enough_data_.store(false, Ordering::Relaxed);
                                    let _ = notify!(observer_, PlayerEvent::NeedData);
                                })
                                .enough_data(move |_| {
                                    enough_data__.store(true, Ordering::Relaxed);
                                    let _ = notify!(observer__, PlayerEvent::EnoughData);
                                })
                                .seek_data(move |_, offset| {
                                    let (ret, ack_channel) = if servosrc_.set_seek_offset(offset) {
                                        let _ = notify!(
                                            observer___,
                                            PlayerEvent::SeekData(
                                                offset,
                                                seek_channel.lock().unwrap().sender()
                                            )
                                        );
                                        let (ret, ack_channel) =
                                            seek_channel.lock().unwrap()._await();
                                        (ret, Some(ack_channel))
                                    } else {
                                        (true, None)
                                    };

                                    servosrc_.set_seek_done();
                                    if let Some(ack_channel) = ack_channel {
                                        ack_channel.send(()).unwrap();
                                    }
                                    ret
                                })
                                .build(),
                        );

                        PlayerSource::Seekable(servosrc)
                    },
                    StreamType::Stream => {
                        let media_stream_src = source
                            .dynamic_cast::<ServoMediaStreamSrc>()
                            .expect("Source element is expected to be a ServoMediaStreamSrc!");
                        let sender_clone = sender.clone();
                        is_ready_clone.call_once(|| {
                            let _ = notify!(sender_clone, Ok(()));
                        });
                        PlayerSource::Stream(media_stream_src)
                    },
                };

                inner.set_src(source);

                None
            });

            let error_handler_id =
                signal_adapter.connect_error(move |signal_adapter, error, _details| {
                    let _ = notify!(sender_clone, Err(PlayerError::Backend(error.to_string())));
                    signal_adapter.play().stop();
                });

            inner.player.pause();

            (receiver, error_handler_id)
        };

        let result = receiver.recv().unwrap();
        glib::signal::signal_handler_disconnect(
            &inner.lock().unwrap().signal_adapter,
            error_handler_id,
        );
        result
    }
}

macro_rules! inner_player_proxy_getter {
    ($fn_name:ident, $return_type:ty, $default_value:expr_2021) => {
        fn $fn_name(&self) -> $return_type {
            if self.setup().is_err() {
                return $default_value;
            }

            let inner = self.inner.borrow();
            let inner = inner.as_ref().unwrap().lock().unwrap();
            inner.$fn_name()
        }
    };
}

macro_rules! inner_player_proxy {
    ($fn_name:ident, $return_type:ty) => {
        fn $fn_name(&self) -> Result<$return_type, PlayerError> {
            self.setup()?;
            let inner = self.inner.borrow();
            let mut inner = inner.as_ref().unwrap().lock().unwrap();
            inner.$fn_name()
        }
    };

    ($fn_name:ident, $arg1:ident, $arg1_type:ty) => {
        fn $fn_name(&self, $arg1: $arg1_type) -> Result<(), PlayerError> {
            self.setup()?;
            let inner = self.inner.borrow();
            let mut inner = inner.as_ref().unwrap().lock().unwrap();
            inner.$fn_name($arg1)
        }
    };

    ($fn_name:ident, $arg1:ident, $arg1_type:ty, $arg2:ident, $arg2_type:ty) => {
        fn $fn_name(&self, $arg1: $arg1_type, $arg2: $arg2_type) -> Result<(), PlayerError> {
            self.setup()?;
            let inner = self.inner.borrow();
            let mut inner = inner.as_ref().unwrap().lock().unwrap();
            inner.$fn_name($arg1, $arg2)
        }
    };
}

impl Player for GStreamerPlayer {
    inner_player_proxy!(play, ());
    inner_player_proxy!(pause, ());
    inner_player_proxy_getter!(paused, bool, DEFAULT_PAUSED);
    inner_player_proxy_getter!(can_resume, bool, DEFAULT_CAN_RESUME);
    inner_player_proxy_getter!(position, Option<f64>, None);
    inner_player_proxy!(stop, ());
    inner_player_proxy!(end_of_stream, ());
    inner_player_proxy!(set_input_size, size, u64);
    inner_player_proxy!(set_seekable, seekable, bool);
    inner_player_proxy!(set_mute, muted, bool);
    inner_player_proxy_getter!(muted, bool, DEFAULT_MUTED);
    inner_player_proxy!(set_playback_rate, playback_rate, f64);
    inner_player_proxy_getter!(playback_rate, f64, DEFAULT_PLAYBACK_RATE);
    inner_player_proxy!(push_data, data, Vec<u8>);
    inner_player_proxy!(seek, time, f64);
    inner_player_proxy!(set_volume, volume, f64);
    inner_player_proxy_getter!(volume, f64, DEFAULT_VOLUME);
    inner_player_proxy_getter!(buffered, Vec<Range<f64>>, DEFAULT_TIME_RANGES);
    inner_player_proxy_getter!(seekable, Vec<Range<f64>>, DEFAULT_TIME_RANGES);
    inner_player_proxy!(set_stream, stream, &MediaStreamId, only_stream, bool);
    inner_player_proxy!(set_audio_track, stream_index, i32, enabled, bool);
    inner_player_proxy!(set_video_track, stream_index, i32, enabled, bool);

    fn set_download_buffering_enabled(&self, enable: bool) -> Result<(), PlayerError> {
        // We can't use the default implementation because we need to set the
        // download_buffering_enabled flag before calling setup, which is where
        // the "download" flag is set on the pipeline. That has to happen before
        // the URI is set.
        self.download_buffering_enabled
            .store(enable, Ordering::Relaxed);
        self.setup()?;
        Ok(())
    }

    fn render_use_gl(&self) -> bool {
        self.render.lock().unwrap().is_gl()
    }
}

impl MediaInstance for GStreamerPlayer {
    fn get_id(&self) -> usize {
        self.id
    }

    fn mute(&self, val: bool) -> Result<(), MediaInstanceError> {
        self.set_mute(val).map_err(|_| MediaInstanceError)
    }

    fn suspend(&self) -> Result<(), MediaInstanceError> {
        self.pause().map_err(|_| MediaInstanceError)
    }

    fn resume(&self) -> Result<(), MediaInstanceError> {
        if !self.can_resume() {
            return Ok(());
        }

        self.play().map_err(|_| MediaInstanceError)
    }
}

impl Drop for GStreamerPlayer {
    fn drop(&mut self) {
        let _ = self.stop();
        let (tx_ack, rx_ack) = mpsc::channel();
        let _ = self
            .backend_chan
            .lock()
            .unwrap()
            .send(BackendMsg::Shutdown {
                context: self.context_id,
                id: self.id,
                tx_ack,
            });
        let _ = rx_ack.recv();
    }
}

/// The running time of a timestamp in the sample's segment: what the pipeline
/// clock (minus base time) reads when it is due.  Timestamps alone are not
/// enough: an MPEG-TS cut from a longer stream starts at its original
/// timestamps (tens of seconds, say), with a segment starting there too.
fn running_time(
    sample: &gstreamer::Sample,
    pts: Option<gstreamer::ClockTime>,
) -> Option<gstreamer::ClockTime> {
    let pts = pts?;
    match sample
        .segment()
        .and_then(|segment| segment.downcast_ref::<gstreamer::ClockTime>())
    {
        Some(segment) => segment.to_running_time(pts),
        None => Some(pts),
    }
}

/// The running time a video frame would start at had it not been clipped:
/// a seek to a time between two frames' times leaves the first frame
/// starting at the seek's time, with the time it shows for. Its end, less a
/// frame's duration at the stream's frame rate, is where the frames of the
/// stream fall. Negative when that is before the segment.
fn natural_start(sample: &gstreamer::Sample, buffer: &gstreamer::BufferRef) -> Option<i64> {
    let end = running_time(sample, Some(buffer.pts()? + buffer.duration()?))?;
    let rate = sample
        .caps()?
        .structure(0)?
        .get::<gstreamer::Fraction>("framerate")
        .ok()?;
    let (num, den) = (i64::from(rate.numer()), i64::from(rate.denom()));
    if num <= 0 || den <= 0 {
        return None;
    }
    Some(end.nseconds() as i64 - den * 1_000_000_000 / num)
}

/// How long a video frame lasts: its buffer's duration, or one frame at the
/// stream's frame rate.
fn frame_duration(sample: &gstreamer::Sample, buffer: &gstreamer::BufferRef) -> Option<u64> {
    if let Some(duration) = buffer.duration() {
        return Some(duration.nseconds());
    }
    let rate = sample
        .caps()?
        .structure(0)?
        .get::<gstreamer::Fraction>("framerate")
        .ok()?;
    let (num, den) = (u64::try_from(rate.numer()).ok()?, u64::try_from(rate.denom()).ok()?);
    (num > 0).then(|| den * 1_000_000_000 / num)
}

/// Where a player's media timeline starts.  GStreamer reports positions in
/// stream time, which for many files does not start at 0: an MPEG-TS
/// demuxer counts from the first PCR, so the first frame of a file can be at
/// 0.125 s or 1.4 s.  The element's timeline starts at its first frame, as
/// other browsers have it: positions are reported less this origin and
/// seeks are made plus it.
#[derive(Default)]
struct Origin {
    time: Option<gstreamer::ClockTime>,
    from_video: bool,
}

impl Origin {
    /// A sample is about to be shown or heard: the first one sets the
    /// origin.  The first video frame wins over the first audio block (a
    /// video element's frames are what must land on their times).
    fn note(&mut self, sample: &gstreamer::Sample, video: bool) {
        if self.from_video || (self.time.is_some() && !video) {
            return;
        }
        let Some(pts) = sample.buffer().and_then(|buffer| buffer.pts()) else {
            return;
        };
        let stream_time = match sample
            .segment()
            .and_then(|segment| segment.downcast_ref::<gstreamer::ClockTime>())
        {
            Some(segment) => segment.to_stream_time(pts),
            None => Some(pts),
        };
        if let Some(stream_time) = stream_time {
            self.time = Some(stream_time);
            self.from_video = video;
        }
    }

    fn time(&self) -> gstreamer::ClockTime {
        self.time.unwrap_or(gstreamer::ClockTime::ZERO)
    }

    /// A stream time as a position on the element's timeline, in seconds.
    fn media_time(&self, position: gstreamer::ClockTime) -> f64 {
        position.saturating_sub(self.time()).seconds_f64()
    }
}
