use std::time::{SystemTime, UNIX_EPOCH};

use crate::state::{PlayerError, RepeatMode};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub url: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub duration_ms: Option<u64>,
    pub lyrics_url: Option<String>,
}

impl Track {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            title: None,
            artist: None,
            duration_ms: None,
            lyrics_url: None,
        }
    }
}

#[derive(Debug)]
pub struct Playlist {
    tracks: Vec<Track>,
    current_index: usize,
    repeat_mode: RepeatMode,
    shuffle: bool,
    shuffle_order: Vec<usize>,
    rng_state: u64,
}

impl Default for Playlist {
    fn default() -> Self {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0x9e37_79b9_7f4a_7c15, |duration| {
                duration.as_secs() ^ u64::from(duration.subsec_nanos())
            });
        Self::with_seed(seed)
    }
}

impl Playlist {
    pub fn with_seed(seed: u64) -> Self {
        Self {
            tracks: Vec::new(),
            current_index: 0,
            repeat_mode: RepeatMode::None,
            shuffle: false,
            shuffle_order: Vec::new(),
            rng_state: seed.max(1),
        }
    }

    pub fn add(&mut self, track: Track) {
        self.tracks.push(track);
        if self.shuffle {
            self.regenerate_shuffle_order();
        }
    }

    pub fn add_next(&mut self, track: Track) {
        let position = (self.real_index() + 1).min(self.tracks.len());
        self.tracks.insert(position, track);
        if self.shuffle {
            self.regenerate_shuffle_order();
        }
    }

    pub fn remove(&mut self, index: usize) -> Result<(), PlayerError> {
        if index >= self.tracks.len() {
            return Err(PlayerError::IndexOutOfBounds);
        }
        self.tracks.remove(index);
        if !self.tracks.is_empty() && self.current_index >= self.tracks.len() {
            self.current_index = self.tracks.len() - 1;
        }
        if self.tracks.is_empty() {
            self.current_index = 0;
        }
        if self.shuffle {
            self.regenerate_shuffle_order();
        }
        Ok(())
    }

    pub fn clear(&mut self) {
        self.tracks.clear();
        self.shuffle_order.clear();
        self.current_index = 0;
    }

    pub fn current(&self) -> Option<&Track> {
        self.tracks.get(self.real_index())
    }

    pub fn next_track(&mut self) -> Option<&Track> {
        if self.tracks.is_empty() {
            return None;
        }
        match self.repeat_mode {
            RepeatMode::One => {}
            RepeatMode::All => self.current_index = (self.current_index + 1) % self.tracks.len(),
            RepeatMode::None => {
                if self.current_index + 1 >= self.tracks.len() {
                    return None;
                }
                self.current_index += 1;
            }
        }
        self.current()
    }

    pub fn previous_track(&mut self) -> Option<&Track> {
        if self.tracks.is_empty() {
            return None;
        }
        match self.repeat_mode {
            RepeatMode::One => {}
            RepeatMode::All => {
                if self.current_index == 0 {
                    self.current_index = self.tracks.len() - 1;
                } else {
                    self.current_index -= 1;
                }
            }
            RepeatMode::None if self.current_index > 0 => {
                self.current_index -= 1;
            }
            RepeatMode::None => return None,
        }
        self.current()
    }

    pub fn jump_to(&mut self, index: usize) -> Result<&Track, PlayerError> {
        if index >= self.tracks.len() {
            return Err(PlayerError::IndexOutOfBounds);
        }
        if self.shuffle {
            self.current_index = self
                .shuffle_order
                .iter()
                .position(|track_index| *track_index == index)
                .unwrap_or(0);
        } else {
            self.current_index = index;
        }
        Ok(&self.tracks[index])
    }

    pub fn set_repeat_mode(&mut self, mode: RepeatMode) {
        self.repeat_mode = mode;
    }

    pub fn set_shuffle(&mut self, enabled: bool) {
        let current_track = self.real_index();
        self.shuffle = enabled;
        if enabled {
            self.regenerate_shuffle_order();
            self.current_index = self
                .shuffle_order
                .iter()
                .position(|track_index| *track_index == current_track)
                .unwrap_or(0);
        } else {
            self.shuffle_order.clear();
            self.current_index = current_track.min(self.tracks.len().saturating_sub(1));
        }
    }

    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    pub fn current_index(&self) -> usize {
        self.real_index()
    }

    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    fn real_index(&self) -> usize {
        if self.shuffle {
            self.shuffle_order
                .get(self.current_index)
                .copied()
                .unwrap_or(self.current_index)
        } else {
            self.current_index
        }
    }

    fn regenerate_shuffle_order(&mut self) {
        let current_track = self.real_index();
        self.shuffle_order = (0..self.tracks.len()).collect();
        for index in (1..self.shuffle_order.len()).rev() {
            let swap_with = (self.next_random() as usize) % (index + 1);
            self.shuffle_order.swap(index, swap_with);
        }
        self.current_index = self
            .shuffle_order
            .iter()
            .position(|track_index| *track_index == current_track)
            .unwrap_or(0);
    }

    fn next_random(&mut self) -> u64 {
        let mut value = self.rng_state;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.rng_state = value;
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(url: &str) -> Track {
        Track::new(url)
    }

    #[test]
    fn add_insert_remove_and_clear() {
        let mut playlist = Playlist::with_seed(1);
        playlist.add(track("a.mp3"));
        playlist.add(track("c.mp3"));
        playlist.add_next(track("b.mp3"));
        assert_eq!(playlist.tracks()[1].url, "b.mp3");
        playlist.remove(1).unwrap();
        assert_eq!(playlist.len(), 2);
        playlist.clear();
        assert!(playlist.is_empty());
    }

    #[test]
    fn repeat_modes_match_contract() {
        let mut playlist = Playlist::with_seed(1);
        playlist.add(track("a"));
        playlist.add(track("b"));
        assert_eq!(playlist.next_track().unwrap().url, "b");
        assert!(playlist.next_track().is_none());
        playlist.set_repeat_mode(RepeatMode::All);
        assert_eq!(playlist.next_track().unwrap().url, "a");
        playlist.set_repeat_mode(RepeatMode::One);
        assert_eq!(playlist.next_track().unwrap().url, "a");
    }

    #[test]
    fn shuffle_visits_each_track_and_keeps_current_track() {
        let mut playlist = Playlist::with_seed(42);
        for url in ["a", "b", "c", "d"] {
            playlist.add(track(url));
        }
        playlist.jump_to(1).unwrap();
        playlist.set_shuffle(true);
        assert_eq!(playlist.current().unwrap().url, "b");

        playlist.set_repeat_mode(RepeatMode::All);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..4 {
            seen.insert(playlist.current().unwrap().url.clone());
            playlist.next_track();
        }
        assert_eq!(seen.len(), 4);
        playlist.set_shuffle(false);
        assert!(playlist.current_index() < 4);
    }

    #[test]
    fn enabling_shuffle_again_regenerates_order_without_changing_track() {
        let mut playlist = Playlist::with_seed(42);
        for url in ["a", "b", "c", "d"] {
            playlist.add(track(url));
        }
        playlist.jump_to(1).unwrap();
        playlist.set_shuffle(true);
        let rng_state = playlist.rng_state;

        playlist.set_shuffle(true);

        assert_ne!(playlist.rng_state, rng_state);
        assert_eq!(playlist.current().unwrap().url, "b");
    }

    #[test]
    fn out_of_bounds_operations_fail() {
        let mut playlist = Playlist::with_seed(1);
        assert_eq!(playlist.remove(0), Err(PlayerError::IndexOutOfBounds));
        assert_eq!(playlist.jump_to(0), Err(PlayerError::IndexOutOfBounds));
    }
}
