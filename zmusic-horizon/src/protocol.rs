#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ProtocolCommand {
    Play(String),
    Stop,
}

pub(crate) fn parse(data: &[u8]) -> Option<ProtocolCommand> {
    let message = std::str::from_utf8(data.get(1..)?).ok()?;
    if let Some(url) = message.strip_prefix("[Play]") {
        if url.trim().is_empty() {
            log::warn!("忽略空的 ZMusic 播放地址");
            return None;
        }
        Some(ProtocolCommand::Play(url.to_owned()))
    } else if message == "[Stop]" {
        Some(ProtocolCommand::Stop)
    } else {
        log::warn!("忽略未知的 ZMusic 消息");
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_the_plugin_channel_prefix_and_commands() {
        assert_eq!(
            parse(b"\0[Play]https://example.com/a.mp3"),
            Some(ProtocolCommand::Play(
                "https://example.com/a.mp3".to_owned()
            ))
        );
        assert_eq!(parse(b"\xff[Stop]"), Some(ProtocolCommand::Stop));
    }

    #[test]
    fn rejects_incomplete_empty_and_unknown_messages() {
        assert_eq!(parse(b""), None);
        assert_eq!(parse(b"\0[Play] \t"), None);
        assert_eq!(parse(b"\0[Stop]extra"), None);
        assert_eq!(parse(b"\0[Unknown]"), None);
        assert_eq!(parse(b"\0\xff"), None);
    }
}
