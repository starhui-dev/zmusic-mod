use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Duration;

use zmusic::Player;
use zmusic::state::PlaybackState;

enum Command {
    Play(String),
    Stop,
    Volume(f32),
}

pub(crate) struct PlayerHandle {
    commands: Sender<Command>,
    state: std::sync::Arc<AtomicU8>,
    volume_bits: AtomicU32,
}

impl Default for PlayerHandle {
    fn default() -> Self {
        let (commands, receiver) = mpsc::channel();
        let state = std::sync::Arc::new(AtomicU8::new(PlaybackState::Stopped as u8));
        let thread_state = std::sync::Arc::clone(&state);
        std::thread::Builder::new()
            .name("zmusic-player".into())
            .spawn(move || {
                let mut player = Player::new();
                loop {
                    match receiver.recv_timeout(Duration::from_millis(50)) {
                        Ok(Command::Play(url)) => {
                            if let Err(error) = player.play(&url) {
                                log::error!("ZMusic 播放失败: {error}");
                            }
                        }
                        Ok(Command::Stop) => player.stop(),
                        Ok(Command::Volume(volume)) => {
                            if let Err(error) = player.set_volume(volume) {
                                log::warn!("ZMusic 音量设置失败: {error}");
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                    let _ = player.poll_event();
                    thread_state.store(player.state() as u8, Ordering::Release);
                }
                player.stop();
                thread_state.store(PlaybackState::Stopped as u8, Ordering::Release);
            })
            .expect("无法创建 ZMusic 播放器线程");
        Self {
            commands,
            state,
            volume_bits: AtomicU32::new(f32::to_bits(-1.0)),
        }
    }
}

impl PlayerHandle {
    pub(crate) fn play(&self, url: String) {
        self.state
            .store(PlaybackState::Loading as u8, Ordering::Release);
        if self.commands.send(Command::Play(url)).is_err() {
            self.state
                .store(PlaybackState::Stopped as u8, Ordering::Release);
        }
    }

    pub(crate) fn stop(&self) {
        self.state
            .store(PlaybackState::Stopped as u8, Ordering::Release);
        let _ = self.commands.send(Command::Stop);
    }

    pub(crate) fn set_volume(&self, volume: f32) {
        let bits = volume.to_bits();
        if self.volume_bits.load(Ordering::Relaxed) == bits {
            return;
        }
        if self.commands.send(Command::Volume(volume)).is_ok() {
            self.volume_bits.store(bits, Ordering::Relaxed);
        }
    }

    pub(crate) fn suppress_vanilla_music(&self) -> bool {
        matches!(
            self.state.load(Ordering::Acquire),
            state if state == PlaybackState::Loading as u8 || state == PlaybackState::Playing as u8
        )
    }
}
