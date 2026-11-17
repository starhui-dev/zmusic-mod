use crate::audio::{Engine, Sound};
use crate::event::{Event, EventMailbox};
use crate::lyrics::{LyricLine, Lyrics};
use crate::queue::{Playlist, Track};
use crate::state::{PlaybackState, PlayerError, RepeatMode};
use crate::stream::{HttpStream, HttpStreamControl};

pub struct Player {
    engine: Option<Engine>,
    session: Option<PlaybackSession>,
    playlist: Playlist,
    lyrics: Option<Lyrics>,
    mailbox: EventMailbox,
    state: PlaybackState,
    last_error: Option<PlayerError>,
    volume: f32,
}

struct PlaybackSession {
    sound: Option<Sound>,
    stream: Option<HttpStreamControl>,
    ended_reported: bool,
}

impl PlaybackSession {
    fn from_file(engine: &Engine, path: &str) -> Result<Self, PlayerError> {
        Ok(Self {
            sound: Some(engine.sound_from_file(path)?),
            stream: None,
            ended_reported: false,
        })
    }

    fn from_url(engine: &Engine, url: &str, stream: HttpStream) -> Result<Self, PlayerError> {
        let byte_len = stream.total_size();
        let control = stream.control();
        let sound = engine
            .sound_from_stream(stream, url, byte_len)
            .map_err(|error| control.error().unwrap_or(error))?;
        Ok(Self {
            sound: Some(sound),
            stream: Some(control),
            ended_reported: false,
        })
    }

    fn sound(&self) -> &Sound {
        self.sound.as_ref().expect("播放会话必须持有 Sound")
    }
}

impl Drop for PlaybackSession {
    fn drop(&mut self) {
        if let Some(stream) = &self.stream {
            stream.cancel();
        }
        if let Some(sound) = self.sound.take() {
            drop(sound);
        }
        drop(self.stream.take());
    }
}

impl Default for Player {
    fn default() -> Self {
        Self::new()
    }
}

impl Player {
    pub fn new() -> Self {
        Self {
            engine: None,
            session: None,
            playlist: Playlist::default(),
            lyrics: None,
            mailbox: EventMailbox::default(),
            state: PlaybackState::Stopped,
            last_error: None,
            volume: 1.0,
        }
    }

    pub fn play(&mut self, source: &str) -> Result<(), PlayerError> {
        if source.is_empty() {
            return self.fail(PlayerError::InvalidArgument);
        }

        self.stop();
        self.last_error = None;
        self.set_state(PlaybackState::Loading);
        let result = self.create_session(source).inspect(|session| {
            session.sound().set_volume(self.volume);
            session.sound().play();
        });

        match result {
            Ok(session) => {
                self.session = Some(session);
                self.set_state(PlaybackState::Playing);
                Ok(())
            }
            Err(error) => self.fail(error),
        }
    }

    pub fn pause(&mut self) -> Result<(), PlayerError> {
        if self.state != PlaybackState::Playing {
            return Ok(());
        }
        let session = self.session.as_ref().ok_or(PlayerError::InvalidState)?;
        session.sound().pause();
        self.set_state(PlaybackState::Paused);
        Ok(())
    }

    pub fn resume(&mut self) -> Result<(), PlayerError> {
        if self.state != PlaybackState::Paused {
            return Ok(());
        }
        let session = self.session.as_ref().ok_or(PlayerError::InvalidState)?;
        session.sound().play();
        self.set_state(PlaybackState::Playing);
        Ok(())
    }

    pub fn stop(&mut self) {
        drop(self.session.take());
        if self.state != PlaybackState::Stopped {
            self.set_state(PlaybackState::Stopped);
        }
    }

    pub fn seek(&mut self, position_ms: u64) -> Result<(), PlayerError> {
        let session = self.session.as_ref().ok_or(PlayerError::InvalidState)?;
        session.sound().seek(position_ms)
    }

    pub fn state(&self) -> PlaybackState {
        self.state
    }

    pub fn error(&self) -> Option<PlayerError> {
        self.last_error
    }

    pub fn position_ms(&self) -> u64 {
        self.session
            .as_ref()
            .map_or(0, |session| session.sound().position_ms())
    }

    pub fn duration_ms(&self) -> u64 {
        self.session
            .as_ref()
            .map_or(0, |session| session.sound().duration_ms())
    }

    pub fn volume(&self) -> f32 {
        self.volume
    }

    pub fn set_volume(&mut self, volume: f32) -> Result<(), PlayerError> {
        self.volume = if volume.is_nan() {
            0.0
        } else {
            volume.clamp(0.0, 1.0)
        };
        if let Some(session) = &self.session {
            session.sound().set_volume(self.volume);
        }
        Ok(())
    }

    pub fn enqueue(&mut self, track: Track) {
        self.playlist.add(track);
    }

    pub fn enqueue_next(&mut self, track: Track) {
        self.playlist.add_next(track);
    }

    pub fn remove_from_queue(&mut self, index: usize) -> Result<(), PlayerError> {
        self.playlist.remove(index)
    }

    pub fn clear_queue(&mut self) {
        self.playlist.clear();
    }

    pub fn play_next(&mut self) -> Result<(), PlayerError> {
        let source = self
            .playlist
            .next_track()
            .ok_or(PlayerError::QueueEmpty)?
            .url
            .clone();
        self.play(&source)
    }

    pub fn play_previous(&mut self) -> Result<(), PlayerError> {
        let source = self
            .playlist
            .previous_track()
            .ok_or(PlayerError::QueueEmpty)?
            .url
            .clone();
        self.play(&source)
    }

    pub fn play_at_index(&mut self, index: usize) -> Result<(), PlayerError> {
        let source = self.playlist.jump_to(index)?.url.clone();
        self.play(&source)
    }

    pub fn queue_len(&self) -> usize {
        self.playlist.len()
    }

    pub fn current_index(&self) -> usize {
        self.playlist.current_index()
    }

    pub fn set_repeat_mode(&mut self, mode: RepeatMode) {
        self.playlist.set_repeat_mode(mode);
    }

    pub fn set_shuffle(&mut self, enabled: bool) {
        self.playlist.set_shuffle(enabled);
    }

    pub fn load_lyrics(&mut self, content: &str) {
        self.lyrics = Some(Lyrics::parse(content));
    }

    pub fn current_lyric(&self) -> Option<&LyricLine> {
        self.lyric_line_at(self.position_ms())
    }

    pub fn lyric_line_at(&self, time_ms: u64) -> Option<&LyricLine> {
        self.lyrics.as_ref()?.line_at(time_ms)
    }

    pub fn poll_event(&mut self) -> Event {
        self.tick();
        self.mailbox.poll()
    }

    pub fn report_error(&mut self) {
        let _ = self.mailbox.post(Event::Error);
    }

    pub fn tick(&mut self) -> bool {
        let stream_error = self
            .session
            .as_ref()
            .and_then(|session| session.stream.as_ref())
            .and_then(HttpStreamControl::error);
        if let Some(error) = stream_error {
            let _: Result<(), PlayerError> = self.fail(error);
            return false;
        }
        if self.state != PlaybackState::Playing {
            return false;
        }
        let _ = self.mailbox.post(Event::ProgressUpdate);
        let Some(session) = &mut self.session else {
            return false;
        };
        let position_ms = session.sound().position_ms();
        let duration_ms = session.sound().duration_ms();
        if !session.ended_reported
            && session.sound().at_end()
            && duration_ms > 0
            && position_ms.saturating_add(50) >= duration_ms
        {
            session.ended_reported = true;
            let _ = self.mailbox.post(Event::TrackEnded);
            return true;
        }
        false
    }

    fn create_session(&mut self, source: &str) -> Result<PlaybackSession, PlayerError> {
        if source.starts_with("http://") || source.starts_with("https://") {
            let stream = HttpStream::start(source)?;
            PlaybackSession::from_url(self.engine()?, source, stream)
        } else {
            PlaybackSession::from_file(self.engine()?, source)
        }
    }

    fn engine(&mut self) -> Result<&Engine, PlayerError> {
        if self.engine.is_none() {
            self.engine = Some(Engine::new()?);
        }
        Ok(self.engine.as_ref().expect("音频引擎已初始化"))
    }

    fn set_state(&mut self, state: PlaybackState) {
        self.state = state;
        let _ = self.mailbox.post(Event::StateChanged);
    }

    fn fail<T>(&mut self, error: PlayerError) -> Result<T, PlayerError> {
        self.session = None;
        self.last_error = Some(error);
        self.set_state(PlaybackState::Error);
        let _ = self.mailbox.post(Event::Error);
        Err(error)
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_FILE_ID: AtomicU64 = AtomicU64::new(1);

    fn test_wav() -> PathBuf {
        let file_id = NEXT_FILE_ID.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("zmusic-{}-{file_id}.wav", std::process::id()));
        let sample_rate = 8_000_u32;
        let sample_count = sample_rate;
        let data_size = sample_count * 2;
        let mut file = File::create(&path).unwrap();
        file.write_all(b"RIFF").unwrap();
        file.write_all(&(36 + data_size).to_le_bytes()).unwrap();
        file.write_all(b"WAVEfmt ").unwrap();
        file.write_all(&16_u32.to_le_bytes()).unwrap();
        file.write_all(&1_u16.to_le_bytes()).unwrap();
        file.write_all(&1_u16.to_le_bytes()).unwrap();
        file.write_all(&sample_rate.to_le_bytes()).unwrap();
        file.write_all(&(sample_rate * 2).to_le_bytes()).unwrap();
        file.write_all(&2_u16.to_le_bytes()).unwrap();
        file.write_all(&16_u16.to_le_bytes()).unwrap();
        file.write_all(b"data").unwrap();
        file.write_all(&data_size.to_le_bytes()).unwrap();
        file.write_all(&vec![0; data_size as usize]).unwrap();
        path
    }

    #[test]
    fn local_playback_controls_follow_state_contract() {
        let path = test_wav();
        let mut player = Player::new();
        player.play(path.to_str().unwrap()).unwrap();
        assert_eq!(player.state(), PlaybackState::Playing);
        assert!(player.duration_ms() >= 900);
        player.pause().unwrap();
        assert_eq!(player.state(), PlaybackState::Paused);
        player.seek(500).unwrap();
        player.resume().unwrap();
        assert_eq!(player.state(), PlaybackState::Playing);
        player.stop();
        assert_eq!(player.state(), PlaybackState::Stopped);
        assert_eq!(player.position_ms(), 0);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn volume_is_clamped_and_decode_errors_are_reported() {
        let mut player = Player::new();
        player.set_volume(2.0).unwrap();
        assert_eq!(player.volume(), 1.0);
        assert_eq!(
            player.play("/path/that/does/not/exist.mp3"),
            Err(PlayerError::DecodeFailed)
        );
        assert_eq!(player.state(), PlaybackState::Error);
        assert_eq!(player.poll_event(), Event::StateChanged);
        assert_eq!(player.poll_event(), Event::StateChanged);
        assert_eq!(player.poll_event(), Event::Error);
    }

    #[test]
    fn queue_and_lyrics_are_available_through_player() {
        let mut player = Player::new();
        player.enqueue(Track::new("a"));
        player.enqueue_next(Track::new("b"));
        assert_eq!(player.queue_len(), 2);
        player.load_lyrics("[00:01.00]line");
        assert_eq!(player.lyric_line_at(1_000).unwrap().text, "line");
    }

    #[test]
    fn short_track_end_is_reported_once() {
        let path = test_wav();
        let mut player = Player::new();
        player.play(path.to_str().unwrap()).unwrap();
        player
            .seek(player.duration_ms().saturating_sub(50))
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        let mut ended = player.tick();
        while !ended && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
            ended = player.tick();
        }
        assert!(ended, "短音频未报告结束");
        assert!(!player.tick());
        fs::remove_file(path).unwrap();
    }
}
