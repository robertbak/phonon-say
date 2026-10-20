// SPDX-License-Identifier: MIT OR Apache-2.0

//! Streaming playback of mono f32 PCM through cpal.
//!
//! The synthesis thread pushes samples into a lock-free ring buffer; the
//! audio callback drains it. Nothing in the callback allocates, locks or
//! blocks, so the model can generate ahead of real time without glitching.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};
use rtrb::{Consumer, Producer, RingBuffer};

/// How much audio the ring buffer holds, in seconds.
const BUFFER_SECS: usize = 2;

#[derive(Default)]
struct Counters {
    pushed: AtomicUsize,
    popped: AtomicUsize,
}

pub struct Speaker {
    // Dropping the stream stops playback; keep it alive for as long as we play.
    _stream: cpal::Stream,
    writer: Writer,
    counters: Arc<Counters>,
    pub sample_rate: u32,
    pub channels: u16,
    pub device_name: String,
    pub sample_format: SampleFormat,
}

/// Producer side of the speaker, moved to the synthesis thread.
pub struct Writer {
    producer: Producer<f32>,
    counters: Arc<Counters>,
}

impl Writer {
    /// Push mono samples, waiting for the device to make room.
    ///
    /// Returns false if the output stream went away, or stopped draining for
    /// long enough that playback is clearly dead.
    pub fn write(&mut self, samples: &[f32]) -> bool {
        let mut stalled = Instant::now();
        for &sample in samples {
            loop {
                match self.producer.push(sample) {
                    Ok(()) => {
                        self.counters.pushed.fetch_add(1, Ordering::Release);
                        stalled = Instant::now();
                        break;
                    }
                    Err(_) => {
                        if self.producer.is_abandoned() {
                            return false;
                        }
                        if stalled.elapsed() > Duration::from_secs(5) {
                            return false;
                        }
                        // The ring is full: generation is ahead of playback.
                        std::thread::sleep(Duration::from_millis(2));
                    }
                }
            }
        }
        true
    }
}

impl Speaker {
    /// Open an output stream, preferring the model's own sample rate.
    ///
    /// `requested` is a case-insensitive substring of a device name; without
    /// it the host's default output device is used.
    pub fn open(requested: Option<&str>, wanted_rate: u32) -> Result<Self> {
        let host = cpal::default_host();
        let device = match requested {
            Some(query) => find_device(&host, query)?,
            None => host
                .default_output_device()
                .context("no default audio output device; see --list-devices")?,
        };
        let device_name = device.to_string();

        let candidates = candidate_configs(&device, wanted_rate);
        if candidates.is_empty() {
            bail!("{device_name}: no usable output configuration (need f32/i16/u16/i32/u8)");
        }

        let mut last_err = None;
        for (format, rate, channels) in candidates {
            let config = StreamConfig {
                channels,
                sample_rate: rate,
                buffer_size: cpal::BufferSize::Default,
            };
            let (producer, consumer) =
                RingBuffer::new((rate as usize) * (channels as usize) * BUFFER_SECS);
            let counters = Arc::new(Counters::default());
            let built = match format {
                SampleFormat::F32 => {
                    build_stream::<f32>(&device, &config, consumer, counters.clone())
                }
                SampleFormat::I16 => {
                    build_stream::<i16>(&device, &config, consumer, counters.clone())
                }
                SampleFormat::U16 => {
                    build_stream::<u16>(&device, &config, consumer, counters.clone())
                }
                SampleFormat::I32 => {
                    build_stream::<i32>(&device, &config, consumer, counters.clone())
                }
                SampleFormat::U8 => {
                    build_stream::<u8>(&device, &config, consumer, counters.clone())
                }
                other => {
                    last_err = Some(anyhow::anyhow!("unsupported sample format {other:?}"));
                    continue;
                }
            };
            match built {
                Ok(stream) => {
                    stream
                        .play()
                        .with_context(|| format!("starting playback on {device_name}"))?;
                    return Ok(Self {
                        _stream: stream,
                        writer: Writer {
                            producer,
                            counters: counters.clone(),
                        },
                        counters,
                        sample_rate: rate,
                        channels,
                        device_name,
                        sample_format: format,
                    });
                }
                Err(err) => last_err = Some(err),
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no output configuration worked")))
            .with_context(|| format!("opening {device_name} ({wanted_rate} Hz preferred)"))
    }

    /// Push mono samples at the speaker's rate, waiting for the device.
    pub fn write(&mut self, samples: &[f32]) -> bool {
        self.writer.write(samples)
    }

    /// Wait for everything pushed so far to reach the device, plus a short
    /// tail so the last period is not cut when the stream is dropped.
    pub fn finish(&self, timeout: Duration) -> bool {
        let drained = self.wait_drained(timeout);
        std::thread::sleep(Duration::from_millis(150));
        drained
    }

    /// Wait until every pushed sample has been handed to the device.
    ///
    /// Returns false on timeout (for example when the device disappeared).
    pub fn wait_drained(&self, timeout: Duration) -> bool {
        let start = Instant::now();
        loop {
            let pushed = self.counters.pushed.load(Ordering::Acquire);
            let popped = self.counters.popped.load(Ordering::Acquire);
            if popped >= pushed {
                return true;
            }
            if start.elapsed() > timeout {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// Names and default formats of the host's output devices.
pub fn print_devices() -> Result<()> {
    let host = cpal::default_host();
    let default = host.default_output_device();
    for device in host.output_devices().context("listing output devices")? {
        let is_default = default.as_ref() == Some(&device);
        match device.default_output_config() {
            Ok(config) => println!(
                "{}{}  [{} ch, {:?}, {} Hz]",
                if is_default { "* " } else { "  " },
                device,
                config.channels(),
                config.sample_format(),
                config.sample_rate(),
            ),
            Err(_) => println!("{}{}", if is_default { "* " } else { "  " }, device),
        }
    }
    Ok(())
}

fn find_device(host: &cpal::Host, query: &str) -> Result<cpal::Device> {
    let needle = query.to_ascii_lowercase();
    let mut names = Vec::new();
    for device in host.output_devices().context("listing output devices")? {
        let name = device.to_string();
        if name.to_ascii_lowercase().contains(&needle) {
            return Ok(device);
        }
        names.push(name);
    }
    bail!(
        "no output device matching '{query}'. Available:\n  {}",
        names.join("\n  ")
    )
}

/// Configurations worth trying, best first: the model's sample rate, then a
/// standard one, with f32 preferred and stereo over mono.
fn candidate_configs(device: &cpal::Device, wanted_rate: u32) -> Vec<(SampleFormat, u32, u16)> {
    let ranges: Vec<_> = device
        .supported_output_configs()
        .map(|iter| iter.collect())
        .unwrap_or_default();
    let formats = [
        SampleFormat::F32,
        SampleFormat::I16,
        SampleFormat::U16,
        SampleFormat::I32,
        SampleFormat::U8,
    ];
    let rates = [wanted_rate, 48_000, 44_100];
    let mut candidates: Vec<(SampleFormat, u32, u16)> = Vec::new();

    for format in formats {
        for rate in rates {
            if let Some(channels) = channels_for(&ranges, format, rate) {
                let candidate = (format, rate, channels);
                if !candidates.contains(&candidate) {
                    candidates.push(candidate);
                }
                // One rate per format is enough: the resampler covers the rest.
                break;
            }
        }
    }
    if let Ok(default) = device.default_output_config() {
        let candidate = (
            default.sample_format(),
            default.sample_rate(),
            default.channels(),
        );
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }
    candidates
}

fn channels_for(
    ranges: &[cpal::SupportedStreamConfigRange],
    format: SampleFormat,
    rate: u32,
) -> Option<u16> {
    for wanted in [2u16, 1] {
        if ranges
            .iter()
            .any(|r| r.sample_format() == format && r.channels() == wanted && r.contains_rate(rate))
        {
            return Some(wanted);
        }
    }
    ranges
        .iter()
        .filter(|r| r.sample_format() == format && r.contains_rate(rate))
        .map(|r| r.channels())
        .min()
}

fn build_stream<T: OutSample>(
    device: &cpal::Device,
    config: &StreamConfig,
    mut consumer: Consumer<f32>,
    counters: Arc<Counters>,
) -> Result<cpal::Stream> {
    let channels = config.channels as usize;
    let stream = device.build_output_stream(
        *config,
        move |data: &mut [T], _| {
            let mut popped = 0usize;
            for frame in data.chunks_mut(channels) {
                let sample = match consumer.pop() {
                    Ok(sample) => {
                        popped += 1;
                        sample
                    }
                    Err(_) => 0.0,
                };
                frame.fill(T::from_f32(sample));
            }
            if popped > 0 {
                counters.popped.fetch_add(popped, Ordering::Release);
            }
        },
        move |err| eprintln!("audio output error: {err}"),
        None,
    )?;
    Ok(stream)
}

trait OutSample: cpal::SizedSample + Copy {
    fn from_f32(sample: f32) -> Self;
}

impl OutSample for f32 {
    fn from_f32(sample: f32) -> Self {
        sample.clamp(-1.0, 1.0)
    }
}

impl OutSample for i16 {
    fn from_f32(sample: f32) -> Self {
        (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
    }
}

impl OutSample for u16 {
    fn from_f32(sample: f32) -> Self {
        ((sample.clamp(-1.0, 1.0) * 0.5 + 0.5) * u16::MAX as f32) as u16
    }
}

impl OutSample for i32 {
    fn from_f32(sample: f32) -> Self {
        (sample.clamp(-1.0, 1.0) * i32::MAX as f32) as i32
    }
}

impl OutSample for u8 {
    fn from_f32(sample: f32) -> Self {
        ((sample.clamp(-1.0, 1.0) * 0.5 + 0.5) * u8::MAX as f32) as u8
    }
}

/// Streaming linear resampler for mono f32, stateful across chunks.
pub struct Resampler {
    step: f64,
    phase: f64,
    prev: f32,
    primed: bool,
}

impl Resampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        Self {
            step: in_rate as f64 / out_rate as f64,
            phase: 0.0,
            prev: 0.0,
            primed: false,
        }
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let n = input.len();
        if n == 0 {
            return;
        }
        if !self.primed {
            // Virtual index 0 is the last sample of the previous chunk. On the
            // first chunk, duplicate the first sample there and start at 1.0,
            // so the first output is exactly input[0].
            self.prev = input[0];
            self.phase = 1.0;
            self.primed = true;
        }
        // Virtual buffer: [prev, input[0], ..., input[n-1]], length n + 1.
        let total = (n + 1) as f64;
        let mut p = self.phase;
        while p < total - 1.0 {
            let i = p.floor() as isize;
            let frac = (p - i as f64) as f32;
            let a = if i <= 0 {
                self.prev
            } else {
                input[i as usize - 1]
            };
            let j = i + 1;
            let b = if j <= 0 {
                self.prev
            } else if (j as usize) <= n {
                input[j as usize - 1]
            } else {
                a
            };
            out.push(a + (b - a) * frac);
            p += self.step;
        }
        self.phase = p - (total - 1.0);
        self.prev = input[n - 1];
    }

    /// Emit the final sample, which interpolation alone leaves pending.
    pub fn flush(&mut self, out: &mut Vec<f32>) {
        if self.primed {
            out.push(self.prev);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Resampler;

    fn sine(n: usize, rate: f64) -> Vec<f32> {
        (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / rate as f32).sin())
            .collect()
    }

    #[test]
    fn upsampling_preserves_length() {
        let input = sine(24_000, 24_000.0);
        let mut r = Resampler::new(24_000, 48_000);
        let mut out = Vec::new();
        r.process(&input, &mut out);
        r.flush(&mut out);
        assert!(
            (out.len() as i64 - 48_000).abs() <= 2,
            "got {} samples",
            out.len()
        );
        // The waveform itself should match at the same time positions.
        for i in (1000..47_000).step_by(1000) {
            let expected = (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48_000.0).sin();
            assert!((out[i] - expected).abs() < 0.02, "sample {i}");
        }
    }

    #[test]
    fn downsampling_preserves_length() {
        let input = sine(48_000, 48_000.0);
        let mut r = Resampler::new(48_000, 24_000);
        let mut out = Vec::new();
        r.process(&input, &mut out);
        r.flush(&mut out);
        assert!(
            (out.len() as i64 - 24_000).abs() <= 2,
            "got {} samples",
            out.len()
        );
    }

    #[test]
    fn chunk_boundaries_do_not_click() {
        let input = sine(24_000, 24_000.0);
        let mut whole = Vec::new();
        let mut r = Resampler::new(24_000, 48_000);
        r.process(&input, &mut whole);
        r.flush(&mut whole);

        let mut split = Vec::new();
        let mut r = Resampler::new(24_000, 48_000);
        for chunk in input.chunks(1920) {
            r.process(chunk, &mut split);
        }
        r.flush(&mut split);
        assert_eq!(whole.len(), split.len());
        for (i, (a, b)) in whole.iter().zip(&split).enumerate() {
            assert!((a - b).abs() < 1e-6, "sample {i}: {a} vs {b}");
        }
    }
}
