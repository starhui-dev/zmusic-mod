#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LyricLine {
    pub time_ms: u64,
    pub text: String,
    pub translation: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LrcMetadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub author: Option<String>,
    pub offset_ms: i32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lyrics {
    pub metadata: LrcMetadata,
    pub lines: Vec<LyricLine>,
}

impl Lyrics {
    pub fn parse(content: &str) -> Self {
        let mut metadata = LrcMetadata::default();
        let mut raw_lines = Vec::new();

        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if parse_metadata(line, &mut metadata) {
                continue;
            }

            let (timestamps, text) = parse_timestamps(line);
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            for time_ms in timestamps {
                raw_lines.push((time_ms, text.to_owned()));
            }
        }

        raw_lines.sort_by_key(|line| line.0);
        let mut lines = Vec::with_capacity(raw_lines.len());
        let mut index = 0;
        while index < raw_lines.len() {
            let (time_ms, text) = raw_lines[index].clone();
            let translation = raw_lines
                .get(index + 1)
                .and_then(|next| (next.0.saturating_sub(time_ms) <= 500).then(|| next.1.clone()));
            index += if translation.is_some() { 2 } else { 1 };
            lines.push(LyricLine {
                time_ms: apply_offset(time_ms, metadata.offset_ms),
                text,
                translation,
            });
        }

        Self { metadata, lines }
    }

    pub fn line_at(&self, time_ms: u64) -> Option<&LyricLine> {
        let index = self
            .lines
            .partition_point(|line| line.time_ms <= time_ms)
            .checked_sub(1)?;
        self.lines.get(index)
    }
}

fn parse_metadata(line: &str, metadata: &mut LrcMetadata) -> bool {
    let Some(inner) = line
        .strip_prefix('[')
        .and_then(|value| value.split_once(']'))
    else {
        return false;
    };
    let Some((key, value)) = inner.0.split_once(':') else {
        return false;
    };

    match key {
        "ti" => metadata.title = Some(value.to_owned()),
        "ar" => metadata.artist = Some(value.to_owned()),
        "al" => metadata.album = Some(value.to_owned()),
        "by" => metadata.author = Some(value.to_owned()),
        "offset" => {
            let Ok(offset) = value.trim().parse() else {
                return false;
            };
            metadata.offset_ms = offset;
        }
        _ => return false,
    }
    true
}

fn parse_timestamps(mut line: &str) -> (Vec<u64>, &str) {
    let mut timestamps = Vec::with_capacity(2);
    while timestamps.len() < 8 {
        let Some(after_open) = line.strip_prefix('[') else {
            break;
        };
        let Some(close) = after_open.find(']') else {
            break;
        };
        let Some(timestamp) = parse_timestamp(&after_open[..close]) else {
            break;
        };
        timestamps.push(timestamp);
        line = &after_open[close + 1..];
    }
    (timestamps, line)
}

fn parse_timestamp(value: &str) -> Option<u64> {
    let (minutes, rest) = value.split_once(':')?;
    if value.len() < 4 {
        return None;
    }
    let minutes: u64 = minutes.parse().ok()?;
    let (seconds, fraction_ms) = match rest.split_once('.') {
        Some((seconds, fraction)) => {
            let milliseconds = match fraction.len() {
                0 => 0,
                1 => fraction.parse::<u64>().ok()?.saturating_mul(100),
                _ => fraction[..2].parse::<u64>().ok()?,
            };
            (seconds, milliseconds)
        }
        None => (rest, 0),
    };
    let seconds: u64 = seconds.parse().ok()?;
    minutes
        .checked_mul(60_000)?
        .checked_add(seconds.checked_mul(1_000)?)?
        .checked_add(fraction_ms)
}

fn apply_offset(time_ms: u64, offset_ms: i32) -> u64 {
    if offset_ms >= 0 {
        time_ms.saturating_add(offset_ms as u64)
    } else {
        time_ms.saturating_sub(offset_ms.unsigned_abs() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_metadata_timestamps_translation_and_offset() {
        let lyrics = Lyrics::parse(
            "[ti:Title]\n[ar:Artist]\n[offset:-200]\n[00:01.00]Hello\n[00:01.20]你好\n",
        );
        assert_eq!(lyrics.metadata.title.as_deref(), Some("Title"));
        assert_eq!(lyrics.metadata.artist.as_deref(), Some("Artist"));
        assert_eq!(lyrics.lines.len(), 1);
        assert_eq!(lyrics.lines[0].time_ms, 800);
        assert_eq!(lyrics.lines[0].text, "Hello");
        assert_eq!(lyrics.lines[0].translation.as_deref(), Some("你好"));
    }

    #[test]
    fn parses_multiple_timestamp_formats() {
        assert_eq!(Lyrics::parse("[01:30]A").lines[0].time_ms, 90_000);
        assert_eq!(Lyrics::parse("[01:30.5]B").lines[0].time_ms, 90_500);
        assert_eq!(Lyrics::parse("[01:30.567]C").lines[0].time_ms, 90_056);

        let lyrics = Lyrics::parse("[00:01.00][00:05.00]D");
        let times: Vec<_> = lyrics.lines.iter().map(|line| line.time_ms).collect();
        assert_eq!(times, vec![1_000, 5_000]);
    }

    #[test]
    fn offset_saturates_and_empty_lines_are_filtered() {
        let lyrics = Lyrics::parse("[offset:-2000]\n[00:01.00]   \n[00:01.00]A\n[00:05.00]B");
        assert_eq!(lyrics.lines[0].time_ms, 0);
        assert_eq!(lyrics.lines[1].time_ms, 3_000);
    }

    #[test]
    fn line_at_uses_last_started_line() {
        let lyrics = Lyrics::parse("[00:01.00]A\n[00:05.00]B");
        assert!(lyrics.line_at(500).is_none());
        assert_eq!(lyrics.line_at(1_000).unwrap().text, "A");
        assert_eq!(lyrics.line_at(9_000).unwrap().text, "B");
    }
}
