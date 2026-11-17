mod backend;
mod protocol;

use std::sync::atomic::{AtomicU32, Ordering};

use backend::PlayerHandle;
use mod_api::{
    self, ClientDisconnectingEvent, ClientHost, ClientPacketEvent, ClientTickPreEvent, Hooks,
    LoopContext, PlaySoundEvent, SoundSource,
};
use protocol::ProtocolCommand;

static RECORDS_VOLUME: AtomicU32 = AtomicU32::new(f32::to_bits(1.0));

/// 由固定 Horizon 构建中的 audio_options mixin 调用。
pub fn set_records_volume(volume: f32) {
    RECORDS_VOLUME.store(volume.to_bits(), Ordering::Relaxed);
}

#[derive(Default)]
struct ZMusic {
    player: PlayerHandle,
}

#[mod_api::horizon_mod]
impl Hooks for ZMusic {
    fn client_packet(&self, _host: &mut dyn ClientHost, event: &mut ClientPacketEvent<'_>) {
        let Some(payload) = &event.packet.custom_payload else {
            return;
        };
        if payload.channel != "zmusic:channel" {
            return;
        }
        match protocol::parse(&payload.data) {
            Some(ProtocolCommand::Play(url)) => self.player.play(url),
            Some(ProtocolCommand::Stop) => self.player.stop(),
            None => {}
        }
    }

    fn client_disconnecting(&self, _host: &dyn ClientHost, _event: &mut ClientDisconnectingEvent) {
        self.player.stop();
    }

    fn client_tick_pre(&self, host: &mut dyn ClientHost, _event: &mut ClientTickPreEvent) {
        // 该读取同时触发 records_volume mixin，得到未持久化的实时滑块值。
        let _ = host.audio_options();
        self.player
            .set_volume(f32::from_bits(RECORDS_VOLUME.load(Ordering::Relaxed)));
    }

    fn play_sound(&self, _context: &dyn LoopContext, event: &mut PlaySoundEvent<'_>) {
        if !self.player.suppress_vanilla_music() {
            return;
        }
        if event
            .sound
            .as_ref()
            .is_some_and(|sound| matches!(sound.source, SoundSource::Music | SoundSource::Records))
        {
            *event.sound = None;
        }
    }
}
