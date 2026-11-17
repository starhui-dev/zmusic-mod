use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum PlaybackState {
    Stopped = 0,
    Loading = 1,
    Playing = 2,
    Paused = 3,
    Error = 4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum RepeatMode {
    None = 0,
    One = 1,
    All = 2,
}

impl TryFrom<i32> for RepeatMode {
    type Error = PlayerError;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::None),
            1 => Ok(Self::One),
            2 => Ok(Self::All),
            _ => Err(PlayerError::InvalidArgument),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerError {
    InvalidArgument,
    InvalidState,
    InvalidHandle,
    NetworkUnavailable,
    HttpError,
    StreamTimeout,
    Cancelled,
    DecodeFailed,
    UnsupportedFormat,
    DeviceInitFailed,
    QueueEmpty,
    IndexOutOfBounds,
    InvalidLrcFormat,
}

impl fmt::Display for PlayerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for PlayerError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_contract_is_stable() {
        assert_eq!(PlaybackState::Stopped as i32, 0);
        assert_eq!(PlaybackState::Error as i32, 4);
        assert_eq!(RepeatMode::None as i32, 0);
        assert_eq!(RepeatMode::All as i32, 2);
    }

    #[test]
    fn repeat_mode_rejects_unknown_values() {
        assert_eq!(RepeatMode::try_from(-1), Err(PlayerError::InvalidArgument));
        assert_eq!(RepeatMode::try_from(3), Err(PlayerError::InvalidArgument));
    }
}
