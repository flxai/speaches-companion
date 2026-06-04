use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use speaches_companion::config::{
    default_config_path_with_env, load_file_config_at, realtime_ws_url, resolve_config,
    resolve_config_path_with_env, resolve_tts_config, ConfigInput, TtsConfigInput,
    DEFAULT_BASE_URL, DEFAULT_MODEL, DEFAULT_TTS_MODEL, DEFAULT_TTS_PLAYER,
    DEFAULT_TTS_RESPONSE_FORMAT, DEFAULT_TTS_SPEED, DEFAULT_TTS_VOICE,
};
use speaches_companion::wakeword::{OpenWakewordStockModel, WakewordEngine};
use tempfile::tempdir;

#[test]
fn defaults_are_derived_from_cfg() {
    let config = resolve_config(ConfigInput::default());

    assert_eq!(config.base_url, DEFAULT_BASE_URL);
    assert_eq!(config.model, DEFAULT_MODEL);
    assert_eq!(config.language, None);
    assert_eq!(config.duration_seconds, 10);
    assert!(config
        .trace_path
        .starts_with("target/speaches-companion-traces"));
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
        file_base_url: Some("http://file.example:8000".to_string()),
        file_model: Some("file-model".to_string()),
        file_language: Some("fr".to_string()),
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
fn file_values_override_defaults() {
    let config = resolve_config(ConfigInput {
        file_base_url: Some("http://file.example:8000".to_string()),
        file_model: Some("file-model".to_string()),
        file_language: Some("it".to_string()),
        ..ConfigInput::default()
    });

    assert_eq!(config.base_url, "http://file.example:8000");
    assert_eq!(config.model, "file-model");
    assert_eq!(config.language.as_deref(), Some("it"));
}

#[test]
fn tts_defaults_use_speaches_audio_speech_values() {
    let config = resolve_tts_config(TtsConfigInput::default());

    assert_eq!(config.base_url, DEFAULT_BASE_URL);
    assert_eq!(config.model, DEFAULT_TTS_MODEL);
    assert_eq!(config.voice, DEFAULT_TTS_VOICE);
    assert_eq!(config.speed, DEFAULT_TTS_SPEED);
    assert_eq!(config.response_format, DEFAULT_TTS_RESPONSE_FORMAT);
    assert_eq!(config.player, DEFAULT_TTS_PLAYER);
    assert!(config.player_args.is_empty());
}

#[test]
fn tts_cli_values_override_environment_values() {
    let config = resolve_tts_config(TtsConfigInput {
        cli_base_url: Some("http://cli.example:9000".to_string()),
        cli_model: Some("cli-tts".to_string()),
        cli_voice: Some("cli-voice".to_string()),
        cli_speed: Some(1.2),
        cli_response_format: Some("mp3".to_string()),
        cli_player: Some("cli-player".to_string()),
        cli_player_args: vec!["--cli".to_string()],
        env_base_url: Some("http://env.example:8000".to_string()),
        env_model: Some("env-tts".to_string()),
        env_voice: Some("env-voice".to_string()),
        env_speed: Some(0.9),
        env_response_format: Some("wav".to_string()),
        env_player: Some("env-player".to_string()),
        env_player_args: Some(vec!["--env".to_string()]),
        file_base_url: Some("http://file.example:8000".to_string()),
        file_model: Some("file-tts".to_string()),
        file_voice: Some("file-voice".to_string()),
        file_speed: Some(1.1),
        file_response_format: Some("opus".to_string()),
        file_player: Some("file-player".to_string()),
        file_player_args: vec!["--file".to_string()],
    });

    assert_eq!(config.base_url, "http://cli.example:9000");
    assert_eq!(config.model, "cli-tts");
    assert_eq!(config.voice, "cli-voice");
    assert_eq!(config.speed, 1.2);
    assert_eq!(config.response_format, "mp3");
    assert_eq!(config.player, "cli-player");
    assert_eq!(config.player_args, ["--cli"]);
}

#[test]
fn file_tts_values_override_defaults() {
    let config = resolve_tts_config(TtsConfigInput {
        file_base_url: Some("http://file.example:8000".to_string()),
        file_model: Some("file-tts".to_string()),
        file_voice: Some("file-voice".to_string()),
        file_speed: Some(1.15),
        file_response_format: Some("opus".to_string()),
        file_player: Some("file-player".to_string()),
        file_player_args: vec![
            "--raw".to_string(),
            "--rate".to_string(),
            "24000".to_string(),
        ],
        ..TtsConfigInput::default()
    });

    assert_eq!(config.base_url, "http://file.example:8000");
    assert_eq!(config.model, "file-tts");
    assert_eq!(config.voice, "file-voice");
    assert_eq!(config.speed, 1.15);
    assert_eq!(config.response_format, "opus");
    assert_eq!(config.player, "file-player");
    assert_eq!(config.player_args, ["--raw", "--rate", "24000"]);
}

#[test]
fn loads_toml_file_config() {
    let dir = tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"
[speaches]
base_url = "http://speaches.example:8000"

[stt]
model = "stt-model"
language = "de"

[tts]
model = "tts-model"
voice = "lessac"
speed = 1.2
response_format = "wav"
player = "pw-play"
player_args = ["--raw", "--rate", "24000"]

[dictation]
transcript_dir = "transcripts"
stream_response = true
realtime_partials = true
final_pass = false
listening_marker = "..."
inline_partials = false
partial_chunking = false
partial_chunk_delay_ms = 40
partial_chunk_max_delay_ms = 120
append_space = false
inject_delay_microsecs = 3000
paste_settle_delay_ms = 450
paste_in_terminals = true
leading_silence_ms = 400
preroll_ms = 1000
denoise = true

[wakeword]
name = "hey_computer"
engine = "openwakeword"
stock_model = "weather"
assets_dir = "openwakeword-assets"
root_dir = "wakewords"
threshold = 0.7
frame_ms = 80
silence_timeout_ms = 900
activation_grace_ms = 5000
max_recording_ms = 30000
press_enter = true
notify_on_detect = true
realtime_partials = true
stop_words = ["full stop", "cancel dictation"]
"#,
    )
    .unwrap();

    let config = load_file_config_at(&config_path).unwrap();

    assert_eq!(
        config.speaches.base_url.as_deref(),
        Some("http://speaches.example:8000")
    );
    assert_eq!(config.stt.model.as_deref(), Some("stt-model"));
    assert_eq!(config.stt.language.as_deref(), Some("de"));
    assert_eq!(config.tts.model.as_deref(), Some("tts-model"));
    assert_eq!(config.tts.voice.as_deref(), Some("lessac"));
    assert_eq!(config.tts.speed, Some(1.2));
    assert_eq!(config.tts.response_format.as_deref(), Some("wav"));
    assert_eq!(config.tts.player.as_deref(), Some("pw-play"));
    assert_eq!(config.tts.player_args, ["--raw", "--rate", "24000"]);
    assert_eq!(
        config.dictation.transcript_dir.as_deref(),
        Some(Path::new("transcripts"))
    );
    assert_eq!(config.dictation.stream_response, Some(true));
    assert_eq!(config.dictation.realtime_partials, Some(true));
    assert_eq!(config.dictation.final_pass, Some(false));
    assert_eq!(config.dictation.listening_marker.as_deref(), Some("..."));
    assert_eq!(config.dictation.inline_partials, Some(false));
    assert_eq!(config.dictation.partial_chunking, Some(false));
    assert_eq!(config.dictation.partial_chunk_delay_ms, Some(40));
    assert_eq!(config.dictation.partial_chunk_max_delay_ms, Some(120));
    assert_eq!(config.dictation.append_space, Some(false));
    assert_eq!(config.dictation.inject_delay_microsecs, Some(3_000));
    assert_eq!(config.dictation.paste_settle_delay_ms, Some(450));
    assert_eq!(config.dictation.paste_in_terminals, Some(true));
    assert_eq!(config.dictation.leading_silence_ms, Some(400));
    assert_eq!(config.dictation.preroll_ms, Some(1000));
    assert_eq!(config.dictation.denoise, Some(true));
    assert_eq!(
        config.wakeword.root_dir.as_deref(),
        Some(Path::new("wakewords"))
    );
    assert_eq!(config.wakeword.name.as_deref(), Some("hey_computer"));
    assert_eq!(config.wakeword.engine, Some(WakewordEngine::Openwakeword));
    assert_eq!(
        config.wakeword.stock_model,
        Some(OpenWakewordStockModel::Weather)
    );
    assert_eq!(
        config.wakeword.assets_dir.as_deref(),
        Some(Path::new("openwakeword-assets"))
    );
    assert_eq!(config.wakeword.threshold, Some(0.7));
    assert_eq!(config.wakeword.frame_ms, Some(80));
    assert_eq!(config.wakeword.silence_timeout_ms, Some(900));
    assert_eq!(config.wakeword.activation_grace_ms, Some(5000));
    assert_eq!(config.wakeword.max_recording_ms, Some(30000));
    assert_eq!(config.wakeword.press_enter, Some(true));
    assert_eq!(config.wakeword.notify_on_detect, Some(true));
    assert_eq!(config.wakeword.realtime_partials, Some(true));
    assert_eq!(
        config.wakeword.stop_words,
        ["full stop", "cancel dictation"]
    );
}

#[test]
fn example_config_parses() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("config.toml.example");

    let config = load_file_config_at(&path).unwrap();

    assert_eq!(
        config.speaches.base_url.as_deref(),
        Some("http://localhost:8000")
    );
}

#[cfg(feature = "debug-recordings")]
#[test]
fn loads_record_dir_from_toml_file_config() {
    let dir = tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"
[dictation]
record_dir = "recordings"
"#,
    )
    .unwrap();

    let config = load_file_config_at(&config_path).unwrap();

    assert_eq!(
        config.dictation.record_dir.as_deref(),
        Some(Path::new("recordings"))
    );
}

#[test]
fn missing_toml_config_uses_defaults() {
    let dir = tempdir().unwrap();
    let config = load_file_config_at(&dir.path().join("missing.toml")).unwrap();

    assert_eq!(config.speaches.base_url, None);
    assert_eq!(config.stt.model, None);
    assert_eq!(config.tts.voice, None);
}

#[test]
fn default_config_path_uses_xdg_then_home() {
    let mut env = BTreeMap::new();
    env.insert("XDG_CONFIG_HOME".to_string(), "/tmp/xdg".to_string());
    env.insert("HOME".to_string(), "/home/example".to_string());

    assert_eq!(
        default_config_path_with_env(&env).unwrap(),
        PathBuf::from("/tmp/xdg/speaches-companion/config.toml")
    );

    env.remove("XDG_CONFIG_HOME");
    assert_eq!(
        default_config_path_with_env(&env).unwrap(),
        PathBuf::from("/home/example/.config/speaches-companion/config.toml")
    );
}

#[test]
fn config_path_prefers_cli_then_environment() {
    let mut env = BTreeMap::new();
    env.insert(
        "SPEACHES_COMPANION_CONFIG".to_string(),
        "/tmp/env-config.toml".to_string(),
    );
    env.insert("HOME".to_string(), "/home/example".to_string());

    assert_eq!(
        resolve_config_path_with_env(Some(PathBuf::from("/tmp/cli-config.toml")), &env).unwrap(),
        PathBuf::from("/tmp/cli-config.toml")
    );
    assert_eq!(
        resolve_config_path_with_env(None, &env).unwrap(),
        PathBuf::from("/tmp/env-config.toml")
    );
}

#[test]
fn realtime_url_converts_http_endpoint_to_ws_endpoint() {
    let url = realtime_ws_url(
        "http://localhost:8000",
        "Systran/faster-whisper-large-v3",
        None,
    )
    .expect("valid url");

    assert_eq!(
        url,
        "ws://localhost:8000/v1/realtime?intent=transcription&model=Systran%2Ffaster-whisper-large-v3"
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

#[test]
fn realtime_url_preserves_base_path() {
    let url = realtime_ws_url(
        "https://speaches.example/speaches?ignored=true",
        "Systran/faster-whisper-large-v3",
        None,
    )
    .expect("valid url");

    assert_eq!(
        url,
        "wss://speaches.example/speaches/v1/realtime?intent=transcription&model=Systran%2Ffaster-whisper-large-v3"
    );
}
