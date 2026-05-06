use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context};
use url::Url;

pub const DEFAULT_BASE_URL: &str = "http://ono.tail:8000";
pub const DEFAULT_MODEL: &str = "Systran/faster-whisper-large-v3";
pub const DEFAULT_TTS_MODEL: &str = "tts-1";
pub const DEFAULT_TTS_RESPONSE_FORMAT: &str = "wav";
pub const DEFAULT_TTS_VOICE: &str = "en_US-lessac-medium";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DictateLiveConfig {
    pub base_url: String,
    pub model: String,
    pub language: Option<String>,
    pub duration_seconds: u64,
    pub trace_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TtsConfig {
    pub base_url: String,
    pub model: String,
    pub voice: String,
    pub response_format: String,
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
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TtsConfigInput {
    pub cli_base_url: Option<String>,
    pub cli_model: Option<String>,
    pub cli_voice: Option<String>,
    pub cli_response_format: Option<String>,
    pub env_base_url: Option<String>,
    pub env_model: Option<String>,
    pub env_voice: Option<String>,
    pub env_response_format: Option<String>,
}

pub fn resolve_config(input: ConfigInput) -> DictateLiveConfig {
    let base_url = choose(input.cli_base_url, input.env_base_url, DEFAULT_BASE_URL);
    let model = choose(input.cli_model, input.env_model, DEFAULT_MODEL);
    let language = input
        .cli_language
        .or(input.env_language)
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
        base_url: choose(input.cli_base_url, input.env_base_url, DEFAULT_BASE_URL),
        model: choose(input.cli_model, input.env_model, DEFAULT_TTS_MODEL),
        voice: choose(input.cli_voice, input.env_voice, DEFAULT_TTS_VOICE),
        response_format: choose(
            input.cli_response_format,
            input.env_response_format,
            DEFAULT_TTS_RESPONSE_FORMAT,
        ),
    }
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

fn choose(cli: Option<String>, env: Option<String>, default: &str) -> String {
    cli.and_then(non_empty_string)
        .or_else(|| env.and_then(non_empty_string))
        .unwrap_or_else(|| default.to_string())
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
        .join("speaches-scribe-traces")
        .join(format!("dictate-live-{millis}.jsonl"))
}
