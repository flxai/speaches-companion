use std::path::Path;

use anyhow::{bail, Context};
use reqwest::multipart::{Form, Part};
use reqwest::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseFormat {
    Json,
    Text,
    Srt,
    VerboseJson,
    Vtt,
}

impl ResponseFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            ResponseFormat::Json => "json",
            ResponseFormat::Text => "text",
            ResponseFormat::Srt => "srt",
            ResponseFormat::VerboseJson => "verbose_json",
            ResponseFormat::Vtt => "vtt",
        }
    }
}

impl std::str::FromStr for ResponseFormat {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "json" => Ok(ResponseFormat::Json),
            "text" => Ok(ResponseFormat::Text),
            "srt" => Ok(ResponseFormat::Srt),
            "verbose_json" => Ok(ResponseFormat::VerboseJson),
            "vtt" => Ok(ResponseFormat::Vtt),
            _ => bail!("unsupported response format: {value}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscribeOptions {
    pub model: String,
    pub response_format: ResponseFormat,
    pub language: Option<String>,
    pub prompt: Option<String>,
    pub hotwords: Option<String>,
    pub without_timestamps: bool,
}

pub async fn transcribe_file(
    base_url: &str,
    audio_path: &Path,
    options: &TranscribeOptions,
) -> anyhow::Result<String> {
    let url = transcription_url(base_url)?;
    let audio = tokio::fs::read(audio_path)
        .await
        .with_context(|| format!("failed to read audio file {}", audio_path.display()))?;
    let file_name = audio_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("audio.wav")
        .to_string();
    let file_part = Part::bytes(audio).file_name(file_name);

    let mut form = Form::new()
        .text("model", options.model.clone())
        .text(
            "response_format",
            options.response_format.as_str().to_string(),
        )
        .text("without_timestamps", options.without_timestamps.to_string())
        .part("file", file_part);

    if let Some(language) = options.language.as_deref().and_then(non_empty_str) {
        form = form.text("language", language.to_string());
    }
    if let Some(prompt) = options.prompt.as_deref().and_then(non_empty_str) {
        form = form.text("prompt", prompt.to_string());
    }
    if let Some(hotwords) = options.hotwords.as_deref().and_then(non_empty_str) {
        form = form.text("hotwords", hotwords.to_string());
    }

    let response = reqwest::Client::new()
        .post(url.clone())
        .multipart(form)
        .send()
        .await
        .with_context(|| format!("failed to POST {url}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("failed to read transcription response")?;

    if !status.is_success() {
        bail!("transcription failed with HTTP {status}: {body}");
    }

    Ok(body)
}

fn transcription_url(base_url: &str) -> anyhow::Result<Url> {
    let mut url =
        Url::parse(base_url).with_context(|| format!("invalid Speaches base URL: {base_url}"))?;
    url.set_path("/v1/audio/transcriptions");
    url.set_query(None);
    Ok(url)
}

fn non_empty_str(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}
