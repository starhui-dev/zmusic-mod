use std::fs::File;
use std::io::{Read, Seek};
use std::num::{NonZeroU16, NonZeroU32};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(all(target_os = "android", not(feature = "test-headless")))]
use rodio::cpal::traits::HostTrait;
use rodio::mixer::{self, Mixer, MixerSource};
use rodio::{Decoder, Player as RodioPlayer, Source};
#[cfg(not(feature = "test-headless"))]
use rodio::{DeviceSinkBuilder, MixerDeviceSink};

use crate::state::PlayerError;

const HEADLESS_CHANNELS: u16 = 2;
const HEADLESS_SAMPLE_RATE: u32 = 48_000;
const HEADLESS_TICK: Duration = Duration::from_millis(10);

struct EngineInner {
    mixer: Mixer,
    _output: AudioOutput,
}

enum AudioOutput {
    #[cfg(not(feature = "test-headless"))]
    Device {
        _sink: MixerDeviceSink,
    },
    Headless {
        _output: HeadlessOutput,
    },
}

struct HeadlessOutput {
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl HeadlessOutput {
    fn start(source: MixerSource) -> Result<Self, PlayerError> {
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_stopped = stopped.clone();
        let worker = thread::Builder::new()
            .name("zmusic-audio-clock".to_owned())
            .spawn(move || run_headless_output(source, worker_stopped))
            .map_err(|_| PlayerError::DeviceInitFailed)?;
        Ok(Self {
            stopped,
            worker: Some(worker),
        })
    }
}

impl Drop for HeadlessOutput {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[derive(Clone)]
pub struct Engine {
    inner: Arc<EngineInner>,
}

impl Engine {
    pub fn new() -> Result<Self, PlayerError> {
        #[cfg(feature = "test-headless")]
        return Self::new_headless();

        #[cfg(not(feature = "test-headless"))]
        let device = open_device_sink();
        #[cfg(not(feature = "test-headless"))]
        let (mixer, output) = match device {
            Ok(mut device) => {
                device.log_on_drop(false);
                (
                    device.mixer().clone(),
                    AudioOutput::Device { _sink: device },
                )
            }
            Err(_) => return Self::new_headless(),
        };
        #[cfg(not(feature = "test-headless"))]
        Ok(Self {
            inner: Arc::new(EngineInner {
                mixer,
                _output: output,
            }),
        })
    }

    fn new_headless() -> Result<Self, PlayerError> {
        let (mixer, source) = mixer::mixer(
            NonZeroU16::new(HEADLESS_CHANNELS).unwrap(),
            NonZeroU32::new(HEADLESS_SAMPLE_RATE).unwrap(),
        );
        let output = HeadlessOutput::start(source)?;
        Ok(Self {
            inner: Arc::new(EngineInner {
                mixer,
                _output: AudioOutput::Headless { _output: output },
            }),
        })
    }

    pub fn sound_from_file(&self, path: &str) -> Result<Sound, PlayerError> {
        let file = File::open(path).map_err(|_| PlayerError::DecodeFailed)?;
        let byte_len = file
            .metadata()
            .map_err(|_| PlayerError::DecodeFailed)?
            .len();
        self.sound_from_reader(file, Some(byte_len), format_hint(path))
    }

    pub fn sound_from_stream<R>(
        &self,
        stream: R,
        source: &str,
        byte_len: Option<u64>,
    ) -> Result<Sound, PlayerError>
    where
        R: Read + Seek + Send + Sync + 'static,
    {
        self.sound_from_reader(stream, byte_len, format_hint(source))
    }

    fn sound_from_reader<R>(
        &self,
        reader: R,
        byte_len: Option<u64>,
        hint: Option<&str>,
    ) -> Result<Sound, PlayerError>
    where
        R: Read + Seek + Send + Sync + 'static,
    {
        let mut builder = Decoder::builder().with_data(reader).with_seekable(true);
        if let Some(byte_len) = byte_len {
            builder = builder.with_byte_len(byte_len);
        }
        if let Some(hint) = hint {
            builder = builder.with_hint(hint);
        }
        let decoder = builder.build().map_err(|_| PlayerError::DecodeFailed)?;
        let duration = decoder.total_duration().unwrap_or_default();
        let player = RodioPlayer::connect_new(&self.inner.mixer);
        player.pause();
        player.append(decoder);
        Ok(Sound {
            player,
            duration,
            _engine: self.inner.clone(),
        })
    }
}

#[cfg(all(target_os = "android", not(feature = "test-headless")))]
fn open_device_sink() -> Result<MixerDeviceSink, rodio::DeviceSinkError> {
    let device = rodio::cpal::default_host()
        .default_output_device()
        .ok_or(rodio::DeviceSinkError::NoDevice)?;
    DeviceSinkBuilder::default()
        .with_device(device)
        .with_channels(NonZeroU16::new(2).unwrap())
        .with_sample_rate(NonZeroU32::new(48_000).unwrap())
        .with_sample_format(rodio::cpal::SampleFormat::F32)
        .with_buffer_size(rodio::cpal::BufferSize::Fixed(2048))
        .open_stream()
}

#[cfg(all(not(target_os = "android"), not(feature = "test-headless")))]
fn open_device_sink() -> Result<MixerDeviceSink, rodio::DeviceSinkError> {
    DeviceSinkBuilder::from_default_device().and_then(|builder| builder.open_sink_or_fallback())
}

pub struct Sound {
    player: RodioPlayer,
    duration: Duration,
    _engine: Arc<EngineInner>,
}

impl Sound {
    pub fn play(&self) {
        self.player.play();
    }

    pub fn pause(&self) {
        self.player.pause();
    }

    pub fn set_volume(&self, volume: f32) {
        self.player.set_volume(volume);
    }

    pub fn seek(&self, position_ms: u64) -> Result<(), PlayerError> {
        self.player
            .try_seek(Duration::from_millis(position_ms))
            .map_err(|_| PlayerError::DecodeFailed)
    }

    pub fn position_ms(&self) -> u64 {
        self.player.get_pos().as_millis() as u64
    }

    pub fn duration_ms(&self) -> u64 {
        self.duration.as_millis() as u64
    }

    pub fn at_end(&self) -> bool {
        self.player.empty()
    }
}

impl Drop for Sound {
    fn drop(&mut self) {
        self.player.stop();
        self.player.sleep_until_end();
    }
}

fn format_hint(source: &str) -> Option<&str> {
    let path = source.split(['?', '#']).next()?;
    Path::new(path).extension()?.to_str()
}

fn run_headless_output(mut source: MixerSource, stopped: Arc<AtomicBool>) {
    let samples_per_tick = (HEADLESS_SAMPLE_RATE * u32::from(HEADLESS_CHANNELS) / 100) as usize;
    let mut deadline = Instant::now();
    while !stopped.load(Ordering::Acquire) {
        for _ in 0..samples_per_tick {
            if stopped.load(Ordering::Acquire) {
                return;
            }
            let _ = source.next();
        }
        deadline += HEADLESS_TICK;
        if let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            thread::sleep(remaining);
        } else {
            deadline = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_initializes_without_a_physical_device() {
        Engine::new().unwrap();
    }

    #[test]
    fn format_hint_ignores_url_query_and_fragment() {
        assert_eq!(
            format_hint("https://example.com/song.mp3?v=1#play"),
            Some("mp3")
        );
        assert_eq!(format_hint("https://example.com/audio"), None);
    }
}
