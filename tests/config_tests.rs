use std::path::PathBuf;

use speaches_scribe::config::{
    realtime_ws_url, resolve_config, resolve_tts_config, ConfigInput, TtsConfigInput,
    DEFAULT_BASE_URL, DEFAULT_MODEL, DEFAULT_TTS_MODEL, DEFAULT_TTS_RESPONSE_FORMAT,
    DEFAULT_TTS_VOICE,
};

#[test]
fn defaults_are_derived_from_cfg() {
    let config = resolve_config(ConfigInput::default());

    assert_eq!(config.base_url, DEFAULT_BASE_URL);
    assert_eq!(config.model, DEFAULT_MODEL);
    assert_eq!(config.language, None);
    assert_eq!(config.duration_seconds, 10);
    assert!(config
        .trace_path
        .starts_with("target/speaches-scribe-traces"));
}

#[test]
fn cli_values_override_environment_values() {
    let config = resolve_config(ConfigInput {
        cli_base_url: Some("http://cli.example:9000".to_string()),
        cli_model: Some("cli-model".to_string()),
        cli_language: Some("de".to_string()),
        cli_duration_seconds: Some(7),
        cli_trace_path: Some(PathBuf::from("/tmp/trace.jsonl")),
        env_base_url: Some("http://env.example:8000".to_string()),
        env_model: Some("env-model".to_string()),
        env_language: Some("en".to_string()),
    });

    assert_eq!(config.base_url, "http://cli.example:9000");
    assert_eq!(config.model, "cli-model");
    assert_eq!(config.language.as_deref(), Some("de"));
    assert_eq!(config.duration_seconds, 7);
    assert_eq!(config.trace_path, PathBuf::from("/tmp/trace.jsonl"));
}

#[test]
fn env_values_override_defaults() {
    let config = resolve_config(ConfigInput {
        env_base_url: Some("http://env.example:8000".to_string()),
        env_model: Some("env-model".to_string()),
        env_language: Some("fr".to_string()),
        ..ConfigInput::default()
    });

    assert_eq!(config.base_url, "http://env.example:8000");
    assert_eq!(config.model, "env-model");
    assert_eq!(config.language.as_deref(), Some("fr"));
}

#[test]
fn tts_defaults_use_speaches_audio_speech_values() {
    let config = resolve_tts_config(TtsConfigInput::default());

    assert_eq!(config.base_url, DEFAULT_BASE_URL);
    assert_eq!(config.model, DEFAULT_TTS_MODEL);
    assert_eq!(config.voice, DEFAULT_TTS_VOICE);
    assert_eq!(config.response_format, DEFAULT_TTS_RESPONSE_FORMAT);
}

#[test]
fn tts_cli_values_override_environment_values() {
    let config = resolve_tts_config(TtsConfigInput {
        cli_base_url: Some("http://cli.example:9000".to_string()),
        cli_model: Some("cli-tts".to_string()),
        cli_voice: Some("cli-voice".to_string()),
        cli_response_format: Some("mp3".to_string()),
        env_base_url: Some("http://env.example:8000".to_string()),
        env_model: Some("env-tts".to_string()),
        env_voice: Some("env-voice".to_string()),
        env_response_format: Some("wav".to_string()),
    });

    assert_eq!(config.base_url, "http://cli.example:9000");
    assert_eq!(config.model, "cli-tts");
    assert_eq!(config.voice, "cli-voice");
    assert_eq!(config.response_format, "mp3");
}

#[test]
fn realtime_url_converts_http_endpoint_to_ws_endpoint() {
    let url = realtime_ws_url(
        "http://ono.tail:8000",
        "Systran/faster-whisper-large-v3",
        None,
    )
    .expect("valid url");

    assert_eq!(
        url,
        "ws://ono.tail:8000/v1/realtime?intent=transcription&model=Systran%2Ffaster-whisper-large-v3"
    );
}

#[test]
fn realtime_url_encodes_optional_language() {
    let url = realtime_ws_url("https://speaches.example", "model/with spaces", Some("de"))
        .expect("valid url");

    assert_eq!(
        url,
        "wss://speaches.example/v1/realtime?intent=transcription&model=model%2Fwith+spaces&language=de"
    );
}
