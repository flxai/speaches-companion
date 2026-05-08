use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context};
use serde::Deserialize;
use url::Url;

use crate::wakeword::{OpenWakewordStockModel, WakewordEngine};

pub const DEFAULT_BASE_URL: &str = "http://ono.tail:8000";
pub const DEFAULT_MODEL: &str = "Systran/faster-whisper-large-v3";
pub const DEFAULT_TTS_MODEL: &str = "tts-1";
pub const DEFAULT_TTS_PLAYER: &str = "pw-play";
pub const DEFAULT_TTS_RESPONSE_FORMAT: &str = "wav";
pub const DEFAULT_TTS_SPEED: f32 = 1.0;
pub const DEFAULT_TTS_VOICE: &str = "en_US-lessac-medium";

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct FileConfig {
    #[serde(default)]
    pub speaches: SpeachesFileConfig,
    #[serde(default)]
    pub stt: SttFileConfig,
    #[serde(default)]
    pub tts: TtsFileConfig,
    #[serde(default)]
    pub dictation: DictationFileConfig,
    #[serde(default)]
    pub wakeword: WakewordFileConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct SpeachesFileConfig {
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct SttFileConfig {
    pub model: Option<String>,
    pub language: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct TtsFileConfig {
    pub model: Option<String>,
    pub voice: Option<String>,
    pub speed: Option<f32>,
    pub response_format: Option<String>,
    pub player: Option<String>,
    #[serde(default)]
    pub player_args: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct DictationFileConfig {
    pub transcript_dir: Option<PathBuf>,
    #[cfg(feature = "debug-recordings")]
    pub record_dir: Option<PathBuf>,
    pub stream_response: Option<bool>,
    pub realtime_partials: Option<bool>,
    pub final_pass: Option<bool>,
    pub listening_marker: Option<String>,
    pub inline_partials: Option<bool>,
    pub partial_chunking: Option<bool>,
    pub partial_chunk_delay_ms: Option<u64>,
    pub partial_chunk_max_delay_ms: Option<u64>,
    pub append_space: Option<bool>,
    pub inject_delay_microsecs: Option<u32>,
    pub leading_silence_ms: Option<u64>,
    pub preroll_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct WakewordFileConfig {
    pub engine: Option<WakewordEngine>,
    pub stock_model: Option<OpenWakewordStockModel>,
    pub assets_dir: Option<PathBuf>,
    pub root_dir: Option<PathBuf>,
    pub threshold: Option<f32>,
    pub frame_ms: Option<u64>,
    pub silence_timeout_ms: Option<u64>,
    pub activation_grace_ms: Option<u64>,
    pub max_recording_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoadedFileConfig {
    pub path: PathBuf,
    pub config: FileConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DictateLiveConfig {
    pub base_url: String,
    pub model: String,
    pub language: Option<String>,
    pub duration_seconds: u64,
    pub trace_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TtsConfig {
    pub base_url: String,
    pub model: String,
    pub voice: String,
    pub speed: f32,
    pub response_format: String,
    pub player: String,
    pub player_args: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigInput {
    pub cli_base_url: Option<String>,
    pub cli_model: Option<String>,
    pub cli_language: Option<String>,
    pub cli_duration_seconds: Option<u64>,
    pub cli_trace_path: Option<PathBuf>,
    pub env_base_url: Option<String>,
    pub env_model: Option<String>,
    pub env_language: Option<String>,
    pub file_base_url: Option<String>,
    pub file_model: Option<String>,
    pub file_language: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TtsConfigInput {
    pub cli_base_url: Option<String>,
    pub cli_model: Option<String>,
    pub cli_voice: Option<String>,
    pub cli_speed: Option<f32>,
    pub cli_response_format: Option<String>,
    pub cli_player: Option<String>,
    pub cli_player_args: Vec<String>,
    pub env_base_url: Option<String>,
    pub env_model: Option<String>,
    pub env_voice: Option<String>,
    pub env_speed: Option<f32>,
    pub env_response_format: Option<String>,
    pub env_player: Option<String>,
    pub env_player_args: Option<Vec<String>>,
    pub file_base_url: Option<String>,
    pub file_model: Option<String>,
    pub file_voice: Option<String>,
    pub file_speed: Option<f32>,
    pub file_response_format: Option<String>,
    pub file_player: Option<String>,
    pub file_player_args: Vec<String>,
}

pub fn resolve_config(input: ConfigInput) -> DictateLiveConfig {
    let base_url = choose(
        input.cli_base_url,
        input.env_base_url,
        input.file_base_url,
        DEFAULT_BASE_URL,
    );
    let model = choose(
        input.cli_model,
        input.env_model,
        input.file_model,
        DEFAULT_MODEL,
    );
    let language = input
        .cli_language
        .or(input.env_language)
        .or(input.file_language)
        .and_then(non_empty_string);
    let duration_seconds = input.cli_duration_seconds.unwrap_or(10);
    let trace_path = input.cli_trace_path.unwrap_or_else(default_trace_path);

    DictateLiveConfig {
        base_url,
        model,
        language,
        duration_seconds,
        trace_path,
    }
}

pub fn resolve_tts_config(input: TtsConfigInput) -> TtsConfig {
    TtsConfig {
        base_url: choose(
            input.cli_base_url,
            input.env_base_url,
            input.file_base_url,
            DEFAULT_BASE_URL,
        ),
        model: choose(
            input.cli_model,
            input.env_model,
            input.file_model,
            DEFAULT_TTS_MODEL,
        ),
        voice: choose(
            input.cli_voice,
            input.env_voice,
            input.file_voice,
            DEFAULT_TTS_VOICE,
        ),
        speed: choose_speed(input.cli_speed, input.env_speed, input.file_speed),
        response_format: choose(
            input.cli_response_format,
            input.env_response_format,
            input.file_response_format,
            DEFAULT_TTS_RESPONSE_FORMAT,
        ),
        player: choose(
            input.cli_player,
            input.env_player,
            input.file_player,
            DEFAULT_TTS_PLAYER,
        ),
        player_args: choose_vec(
            input.cli_player_args,
            input.env_player_args,
            input.file_player_args,
        ),
    }
}

pub fn load_file_config(config_path: Option<PathBuf>) -> anyhow::Result<LoadedFileConfig> {
    let env = std::env::vars().collect::<BTreeMap<_, _>>();
    let path = resolve_config_path_with_env(config_path, &env)?;
    let config = load_file_config_at(&path)?;
    Ok(LoadedFileConfig { path, config })
}

pub fn load_file_config_at(path: &Path) -> anyhow::Result<FileConfig> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(FileConfig::default()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read config {}", path.display()));
        }
    };

    toml::from_str(&contents).with_context(|| format!("failed to parse config {}", path.display()))
}

pub fn resolve_config_path_with_env(
    cli_config_path: Option<PathBuf>,
    env: &BTreeMap<String, String>,
) -> anyhow::Result<PathBuf> {
    if let Some(path) = cli_config_path {
        return Ok(path);
    }
    if let Some(path) = env
        .get("SPEACHES_COMPANION_CONFIG")
        .and_then(|value| non_empty_str(value))
    {
        return Ok(PathBuf::from(path));
    }
    default_config_path_with_env(env)
}

pub fn default_config_path_with_env(env: &BTreeMap<String, String>) -> anyhow::Result<PathBuf> {
    if let Some(path) = env
        .get("XDG_CONFIG_HOME")
        .and_then(|value| non_empty_str(value))
    {
        return Ok(PathBuf::from(path)
            .join("speaches-companion")
            .join("config.toml"));
    }
    if let Some(home) = env.get("HOME").and_then(|value| non_empty_str(value)) {
        return Ok(PathBuf::from(home)
            .join(".config")
            .join("speaches-companion")
            .join("config.toml"));
    }
    bail!("failed to resolve speaches-companion config path: XDG_CONFIG_HOME and HOME are unset")
}

pub fn realtime_ws_url(
    base_url: &str,
    model: &str,
    language: Option<&str>,
) -> anyhow::Result<String> {
    let mut url =
        Url::parse(base_url).with_context(|| format!("invalid Speaches base URL: {base_url}"))?;
    let scheme = match url.scheme() {
        "http" => "ws",
        "https" => "wss",
        scheme => bail!("unsupported Speaches base URL scheme: {scheme}"),
    };
    url.set_scheme(scheme)
        .map_err(|_| anyhow::anyhow!("failed to set websocket URL scheme"))?;
    url.set_path("/v1/realtime");
    url.set_query(None);
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("intent", "transcription");
        query.append_pair("model", model);
        if let Some(language) = language.and_then(non_empty_str) {
            query.append_pair("language", language);
        }
    }
    Ok(url.to_string())
}

pub fn health_url(base_url: &str) -> anyhow::Result<String> {
    let mut url =
        Url::parse(base_url).with_context(|| format!("invalid Speaches base URL: {base_url}"))?;
    url.set_path("/health");
    url.set_query(None);
    Ok(url.to_string())
}

fn choose(cli: Option<String>, env: Option<String>, file: Option<String>, default: &str) -> String {
    cli.and_then(non_empty_string)
        .or_else(|| env.and_then(non_empty_string))
        .or_else(|| file.and_then(non_empty_string))
        .unwrap_or_else(|| default.to_string())
}

fn choose_speed(cli: Option<f32>, env: Option<f32>, file: Option<f32>) -> f32 {
    cli.or(env)
        .or(file)
        .filter(|speed| speed.is_finite() && *speed > 0.0)
        .unwrap_or(DEFAULT_TTS_SPEED)
}

fn choose_vec(cli: Vec<String>, env: Option<Vec<String>>, file: Vec<String>) -> Vec<String> {
    if !cli.is_empty() {
        return cli;
    }
    if let Some(env) = env.filter(|args| !args.is_empty()) {
        return env;
    }
    if !file.is_empty() {
        return file;
    }
    Vec::new()
}

fn non_empty_string(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn non_empty_str(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn default_trace_path() -> PathBuf {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    PathBuf::from("target")
        .join("speaches-companion-traces")
        .join(format!("dictate-live-{millis}.jsonl"))
}
