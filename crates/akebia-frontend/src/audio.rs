//! Audio adapter: from the APU to the sound card.
//!
//! # The two-clock problem
//!
//! The emulation produces samples in bursts —about 800 at once when each frame
//! ends— and the sound card consumes them at a steady trickle, from a thread of
//! its own that the system wakes every few milliseconds. The two rates are
//! similar but never identical: the card's crystal clock is not the same as the
//! main loop's `sleep`.
//!
//! Between the two there is a ring buffer:
//!
//! ```text
//!   emulation thread                     audio thread (callback)
//!   ────────────────                     ───────────────────────
//!   run_frame()  ──► push(≈800) ──►  ┌────────────┐  ──► pop()  ──► speaker
//!                                    │  ~3 frames │
//!                                    └────────────┘
//!                                    absorbs the drift
//! ```
//!
//! When the buffer runs dry —because the machine is just barely keeping up— the
//! callback fills with silence instead of repeating the last thing: a 2 ms gap
//! sounds like a click, but repeating a burst sounds like a warble, which is
//! worse. When it fills up, the new samples are dropped: we would rather lose
//! audio than let the latency grow without end.

use std::sync::{Arc, Mutex};

use akebia_core::StereoSample;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig};

/// Audio frames that fit in the buffer.
///
/// Three frames are about 50 ms: enough to absorb the drift between the two
/// clocks and a `sleep` that overshoots, and little enough that the latency is
/// not noticeable when pressing a button.
const BUFFER_FRAMES: usize = 3;

/// Stereo samples per emulated frame, at 48 kHz. It is an upper bound, not an
/// exact value: the frame lasts 16.74 ms and not a round number of samples.
const SAMPLES_PER_FRAME: usize = 850;

/// Buffer shared between the emulation thread and the audio one.
///
/// A `Mutex` in an audio callback is not the canonical choice —the orthodox
/// thing would be a lock-free queue— but the critical section here is a memory
/// copy of a few kilobytes with no allocation or deallocation. In practice it
/// produces no audio glitches, and swapping it for an SPSC queue is an
/// optimisation that can be done later without touching anything outside this
/// file.
type SharedBuffer = Arc<Mutex<std::collections::VecDeque<f32>>>;

pub struct AudioOutput {
    /// The stream must be kept alive: dropping it makes `cpal` close it.
    _stream: Stream,
    buffer: SharedBuffer,
    sample_rate: u32,
    capacity: usize,
}

impl AudioOutput {
    /// Opens the default output device.
    ///
    /// Returns `Err` with a readable message if there is no sound card
    /// available; the frontend can carry on without audio in that case.
    pub fn new() -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or("there is no audio output device")?;
        let supported = device
            .default_output_config()
            .map_err(|e| format!("could not query the audio configuration: {e}"))?;

        let sample_rate = supported.sample_rate();
        let channels = supported.channels() as usize;
        let config = StreamConfig {
            channels: supported.channels(),
            sample_rate: supported.sample_rate(),
            buffer_size: cpal::BufferSize::Default,
        };

        let capacity = BUFFER_FRAMES * SAMPLES_PER_FRAME * 2;
        let buffer: SharedBuffer =
            Arc::new(Mutex::new(std::collections::VecDeque::with_capacity(capacity)));

        // Only f32 is supported: it is the APU's native format and the one any
        // modern card offers. Converting to i16 here would only add
        // quantisation noise in exchange for nothing.
        if supported.sample_format() != SampleFormat::F32 {
            return Err(format!(
                "the audio device asks for {:?} and only f32 is supported",
                supported.sample_format()
            ));
        }

        let consumer = Arc::clone(&buffer);
        let stream = device
            .build_output_stream(
                config,
                move |out: &mut [f32], _| fill(out, channels, &consumer),
                |err| eprintln!("audio: {err}"),
                None,
            )
            .map_err(|e| format!("could not open the audio stream: {e}"))?;

        stream.play().map_err(|e| format!("could not start the audio: {e}"))?;

        Ok(Self { _stream: stream, buffer, sample_rate, capacity })
    }

    /// Rate the card asks for. It has to be configured on the APU so that it
    /// generates exactly that many samples.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Hands a frame's samples over to the buffer.
    pub fn push(&self, samples: &[StereoSample]) {
        let Ok(mut buffer) = self.buffer.lock() else {
            // The audio thread panicked. Carrying on without sound is
            // preferable to taking the emulation down.
            return;
        };

        for sample in samples {
            if buffer.len() + 2 > self.capacity {
                // Buffer full: the new data is dropped. Letting it grow would
                // only add delay between what is seen and what is heard.
                break;
            }
            buffer.push_back(sample.left);
            buffer.push_back(sample.right);
        }
    }
}

/// Fills the buffer `cpal` asks for.
///
/// `channels` can be greater than 2 on multichannel cards: the stereo is
/// duplicated into the first two and the rest are silenced.
fn fill(out: &mut [f32], channels: usize, buffer: &SharedBuffer) {
    let mut buffer = match buffer.lock() {
        Ok(b) => b,
        Err(_) => {
            out.fill(0.0);
            return;
        }
    };

    for frame in out.chunks_mut(channels) {
        // With no data, silence is emitted rather than the last sample repeated:
        // a gap sounds like a brief click and repeating sounds like a warble.
        let left = buffer.pop_front().unwrap_or(0.0);
        let right = buffer.pop_front().unwrap_or(0.0);

        for (i, channel) in frame.iter_mut().enumerate() {
            *channel = match i {
                0 => left,
                1 => right,
                _ => 0.0,
            };
        }
    }
}
