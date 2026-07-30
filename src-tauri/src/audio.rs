use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SizedSample};
use rustfft::{num_complex::Complex, FftPlanner};
use std::sync::{Arc, Mutex};

const SPECTRUM_FFT_SIZE: usize = 2048;
const SPECTRUM_BANDS: usize = 20;
pub const TRANSCRIBE_SAMPLE_RATE: u32 = 16_000;

pub struct AudioRecorder {
    device: cpal::Device,
    config: cpal::SupportedStreamConfig,
    output_sample_rate: u32,
}

struct SpectrumRing {
    data: [f32; SPECTRUM_FFT_SIZE],
    next: usize,
    len: usize,
}

impl SpectrumRing {
    fn new() -> Self {
        Self {
            data: [0.0; SPECTRUM_FFT_SIZE],
            next: 0,
            len: 0,
        }
    }

    fn push(&mut self, sample: f32) {
        self.data[self.next] = sample;
        self.next = (self.next + 1) % SPECTRUM_FFT_SIZE;
        self.len = (self.len + 1).min(SPECTRUM_FFT_SIZE);
    }

    fn copy_ordered_into(&self, out: &mut [f32; SPECTRUM_FFT_SIZE]) -> bool {
        if self.len < SPECTRUM_FFT_SIZE {
            return false;
        }

        let tail = SPECTRUM_FFT_SIZE - self.next;
        out[..tail].copy_from_slice(&self.data[self.next..]);
        out[tail..].copy_from_slice(&self.data[..self.next]);
        true
    }
}

struct CaptureState {
    samples: Vec<f32>,
    spectrum: SpectrumRing,
}

impl CaptureState {
    fn with_capacity(sample_capacity: usize) -> Self {
        Self {
            samples: Vec::with_capacity(sample_capacity),
            spectrum: SpectrumRing::new(),
        }
    }

    fn push(&mut self, sample: f32) {
        self.samples.push(sample);
        self.spectrum.push(sample);
    }
}

/// Enumerate currently-connected input devices by name (whatever cpal /
/// WASAPI reports). Cheap to call but order can drift between calls if
/// the user (un)plugs hardware — UI should re-fetch on demand.
pub fn list_input_devices() -> Vec<String> {
    let host = cpal::default_host();
    match host.input_devices() {
        Ok(iter) => iter.filter_map(|d| d.name().ok()).collect(),
        Err(e) => {
            tracing::warn!("input_devices enumeration failed: {}", e);
            Vec::new()
        }
    }
}

impl AudioRecorder {
    pub fn new(target_sample_rate: u32, preferred_device: Option<&str>) -> Result<Self> {
        let host = cpal::default_host();
        let device = match preferred_device.filter(|s| !s.is_empty()) {
            Some(name) => {
                let found = host
                    .input_devices()
                    .ok()
                    .and_then(|mut iter| iter.find(|d| d.name().ok().as_deref() == Some(name)));
                match found {
                    Some(d) => d,
                    None => {
                        tracing::warn!(
                            "preferred input '{}' not found, falling back to default",
                            name
                        );
                        host.default_input_device()
                            .context("No input device available")?
                    }
                }
            }
            None => host
                .default_input_device()
                .context("No input device available")?,
        };

        tracing::info!("Using input device: {}", device.name().unwrap_or_default());

        let config = device
            .default_input_config()
            .context("Failed to get default input config")?;

        tracing::info!(
            "Audio config: {} Hz, {} channels, {:?}",
            config.sample_rate().0,
            config.channels(),
            config.sample_format()
        );

        if target_sample_rate != TRANSCRIBE_SAMPLE_RATE {
            tracing::warn!(
                "configured audio.sample_rate={} ignored; whisper sidecars require {} Hz PCM",
                target_sample_rate,
                TRANSCRIBE_SAMPLE_RATE
            );
        }

        Ok(Self {
            device,
            config,
            output_sample_rate: TRANSCRIBE_SAMPLE_RATE,
        })
    }

    pub fn output_sample_rate(&self) -> u32 {
        self.output_sample_rate
    }

    /// Record while condition is true, with frequency spectrum callback (20 bands)
    pub fn record_while_with_spectrum<F, C>(
        &self,
        should_continue: F,
        on_spectrum: C,
    ) -> Result<Vec<f32>>
    where
        F: Fn() -> bool + Send + 'static,
        C: Fn(Vec<f32>) + Send + 'static,
    {
        let channels = self.config.channels() as usize;
        let native_rate = self.config.sample_rate().0;
        let initial_capacity = (native_rate as usize)
            .saturating_mul(10)
            .min(48_000usize * 30);
        let capture = Arc::new(Mutex::new(CaptureState::with_capacity(initial_capacity)));

        let stream = match self.config.sample_format() {
            cpal::SampleFormat::I8 => {
                build_input_stream::<i8>(&self.device, &self.config, channels, capture.clone())?
            }
            cpal::SampleFormat::I16 => {
                build_input_stream::<i16>(&self.device, &self.config, channels, capture.clone())?
            }
            cpal::SampleFormat::I32 => {
                build_input_stream::<i32>(&self.device, &self.config, channels, capture.clone())?
            }
            cpal::SampleFormat::I64 => {
                build_input_stream::<i64>(&self.device, &self.config, channels, capture.clone())?
            }
            cpal::SampleFormat::U8 => {
                build_input_stream::<u8>(&self.device, &self.config, channels, capture.clone())?
            }
            cpal::SampleFormat::U16 => {
                build_input_stream::<u16>(&self.device, &self.config, channels, capture.clone())?
            }
            cpal::SampleFormat::U32 => {
                build_input_stream::<u32>(&self.device, &self.config, channels, capture.clone())?
            }
            cpal::SampleFormat::U64 => {
                build_input_stream::<u64>(&self.device, &self.config, channels, capture.clone())?
            }
            cpal::SampleFormat::F32 => {
                build_input_stream::<f32>(&self.device, &self.config, channels, capture.clone())?
            }
            cpal::SampleFormat::F64 => {
                build_input_stream::<f64>(&self.device, &self.config, channels, capture.clone())?
            }
            format => return Err(anyhow::anyhow!("Unsupported sample format: {format}")),
        };

        stream.play()?;

        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(SPECTRUM_FFT_SIZE);
        let hann_window: Vec<f32> = (0..SPECTRUM_FFT_SIZE)
            .map(|i| {
                0.5 * (1.0
                    - (2.0 * std::f32::consts::PI * i as f32 / SPECTRUM_FFT_SIZE as f32).cos())
            })
            .collect();
        let mut fft_samples = [0.0f32; SPECTRUM_FFT_SIZE];
        let mut complex_buf = vec![Complex::new(0.0, 0.0); SPECTRUM_FFT_SIZE];

        while should_continue() {
            let has_full_window = {
                let capture = capture.lock().unwrap();
                capture.spectrum.copy_ordered_into(&mut fft_samples)
            };

            if has_full_window {
                for (slot, (&sample, &window)) in complex_buf
                    .iter_mut()
                    .zip(fft_samples.iter().zip(hann_window.iter()))
                {
                    *slot = Complex::new(sample * window, 0.0);
                }
                fft.process(&mut complex_buf);

                let mut bands = [0.0f32; SPECTRUM_BANDS];
                let bin_count = SPECTRUM_FFT_SIZE / 2;
                for (i, c) in complex_buf[..bin_count].iter().enumerate() {
                    let mag = c.norm() / SPECTRUM_FFT_SIZE as f32;
                    let freq_ratio = (i as f32 + 1.0) / bin_count as f32;
                    let band_idx = ((freq_ratio.ln() + 5.0) / 5.0 * SPECTRUM_BANDS as f32) as usize;
                    let band_idx = band_idx.min(SPECTRUM_BANDS - 1);
                    bands[band_idx] = bands[band_idx].max(mag);
                }

                let max_val = bands.iter().cloned().fold(0.0f32, f32::max).max(0.001);
                let spectrum: Vec<f32> = bands
                    .iter()
                    .map(|&b| (b / max_val * 3.0).min(1.0))
                    .collect();

                on_spectrum(spectrum);
            }
            // ~16 Hz emit rate. The wave looks smooth at this cadence,
            // and over a multi-hour session it halves the IPC traffic
            // toward the webview — relevant because Tauri's event
            // channel doesn't shed load and can back up if the JS
            // listener falls behind.
            std::thread::sleep(std::time::Duration::from_millis(60));
        }

        drop(stream);

        let samples = Arc::try_unwrap(capture)
            .map_err(|_| anyhow::anyhow!("Failed to unwrap samples"))?
            .into_inner()
            .unwrap()
            .samples;

        if native_rate != self.output_sample_rate {
            Ok(resample(&samples, native_rate, self.output_sample_rate))
        } else {
            Ok(samples)
        }
    }
}

fn build_input_stream<T>(
    device: &cpal::Device,
    supported: &cpal::SupportedStreamConfig,
    channels: usize,
    capture: Arc<Mutex<CaptureState>>,
) -> Result<cpal::Stream>
where
    T: SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    let config: cpal::StreamConfig = supported.clone().into();
    let stream = device.build_input_stream(
        &config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            let mut capture = capture.lock().unwrap();
            for frame in data.chunks(channels) {
                let mono = frame
                    .iter()
                    .copied()
                    .map(<f32 as Sample>::from_sample)
                    .sum::<f32>()
                    / channels as f32;
                capture.push(mono);
            }
        },
        |err| tracing::error!("audio stream error: {}", err),
        None,
    )?;
    Ok(stream)
}

// Simple linear resampling
pub(crate) fn resample(samples: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if samples.is_empty() || from_rate == 0 || to_rate == 0 {
        return Vec::new();
    }
    let ratio = from_rate as f64 / to_rate as f64;
    let new_len = (samples.len() as f64 / ratio) as usize;
    let mut result = Vec::with_capacity(new_len);

    for i in 0..new_len {
        let src_idx = i as f64 * ratio;
        let idx = src_idx as usize;
        let frac = src_idx - idx as f64;

        let sample = if idx + 1 < samples.len() {
            samples[idx] * (1.0 - frac as f32) + samples[idx + 1] * frac as f32
        } else if idx < samples.len() {
            samples[idx]
        } else {
            0.0
        };

        result.push(sample);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_keeps_samples_when_rates_match() {
        let samples = [0.0, 0.25, -0.5, 1.0];

        assert_eq!(resample(&samples, 16_000, 16_000), samples);
    }

    #[test]
    fn resample_downsamples_linearly() {
        let samples = [0.0, 0.25, 0.5, 0.75];

        assert_eq!(resample(&samples, 4, 2), vec![0.0, 0.5]);
    }

    #[test]
    fn resample_upsamples_with_last_sample_padding() {
        let samples = [0.0, 1.0];

        assert_eq!(resample(&samples, 2, 4), vec![0.0, 0.5, 1.0, 1.0]);
    }

    #[test]
    fn spectrum_ring_requires_full_window_before_copy() {
        let mut ring = SpectrumRing::new();
        let mut out = [0.0; SPECTRUM_FFT_SIZE];

        for i in 0..SPECTRUM_FFT_SIZE - 1 {
            ring.push(i as f32);
        }

        assert!(!ring.copy_ordered_into(&mut out));
    }

    #[test]
    fn spectrum_ring_copies_most_recent_window_in_order() {
        let mut ring = SpectrumRing::new();
        let mut out = [0.0; SPECTRUM_FFT_SIZE];

        for i in 0..SPECTRUM_FFT_SIZE + 3 {
            ring.push(i as f32);
        }

        assert!(ring.copy_ordered_into(&mut out));
        assert_eq!(out[0], 3.0);
        assert_eq!(out[SPECTRUM_FFT_SIZE - 1], (SPECTRUM_FFT_SIZE + 2) as f32);
    }
}
