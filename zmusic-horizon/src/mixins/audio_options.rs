fn audio_options(game: &Game<'_>) {
    // SoundSource::RECORDS 的固定索引为 2；getSoundSourceVolume 包含主音量。
    zmusic_horizon::set_records_volume(game.settings.master_volume * game.settings.volumes[2]);
}
