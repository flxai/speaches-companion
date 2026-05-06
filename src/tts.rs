use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context};
use reqwest::Client;
use serde::Serialize;
use tokio::process::Command;
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeechOptions {
    pub model: String,
    pub voice: String,
    pub response_format: String,
}

#[derive(Serialize)]
struct SpeechRequest<'a> {
    input: &'a str,
    model: &'a str,
    voice: &'a str,
    response_format: &'a str,
}

pub async fn synthesize_speech(
    base_url: &str,
    input: &str,
    options: &SpeechOptions,
) -> anyhow::Result<Vec<u8>> {
    let input = normalize_read_aloud_text(input).context("read-aloud input is empty")?;
    let url = speech_url(base_url)?;
    let response = Client::new()
        .post(url)
        .json(&SpeechRequest {
            input: &input,
            model: &options.model,
            voice: &options.voice,
            response_format: &options.response_format,
        })
        .send()
        .await
        .context("failed to send speech request")?;

    let status = response.status();
    let audio = response
        .bytes()
        .await
        .context("failed to read speech response")?;
    if !status.is_success() {
        let body = String::from_utf8_lossy(&audio);
        bail!("speech request failed with HTTP {status}: {body}");
    }
    if audio.is_empty() {
        bail!("speech request returned an empty audio response");
    }

    Ok(audio.to_vec())
}

pub fn speech_url(base_url: &str) -> anyhow::Result<Url> {
    let mut url =
        Url::parse(base_url).with_context(|| format!("invalid Speaches base URL: {base_url}"))?;
    url.set_path("/v1/audio/speech");
    url.set_query(None);
    Ok(url)
}

pub async fn selected_or_clipboard_text() -> anyhow::Result<String> {
    let primary_error = match read_xclip_selection("primary").await {
        Ok(Some(text)) => return Ok(text),
        Ok(None) => None,
        Err(error) => Some(error),
    };

    match read_xclip_selection("clipboard").await {
        Ok(Some(text)) => Ok(text),
        Ok(None) => match primary_error {
            Some(error) => Err(error),
            None => bail!("no selected text or clipboard text available for read-aloud"),
        },
        Err(error) => Err(error),
    }
}

pub fn normalize_read_aloud_text(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub async fn write_speech_temp_file(
    audio: &[u8],
    response_format: &str,
) -> anyhow::Result<PathBuf> {
    if audio.is_empty() {
        bail!("refusing to write empty speech audio");
    }

    let path = std::env::temp_dir().join(format!(
        "speaches-scribe-tts-{}-{}.{}",
        std::process::id(),
        current_millis(),
        speech_file_extension(response_format)
    ));
    tokio::fs::write(&path, audio)
        .await
        .with_context(|| format!("failed to write speech audio to {}", path.display()))?;
    Ok(path)
}

pub async fn play_audio_file(path: &Path, player: &str) -> anyhow::Result<()> {
    let status = Command::new(player)
        .arg(path)
        .status()
        .await
        .with_context(|| format!("failed to run audio player {player}"))?;
    ensure_player_success(player, status)
}

async fn read_xclip_selection(selection: &str) -> anyhow::Result<Option<String>> {
    let output = Command::new("xclip")
        .args(["-o", "-selection", selection])
        .output()
        .await
        .with_context(|| format!("failed to read X11 {selection} selection with xclip"))?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(String::from_utf8(output.stdout)
        .ok()
        .and_then(|text| normalize_read_aloud_text(&text)))
}

fn speech_file_extension(response_format: &str) -> String {
    let normalized = response_format
        .trim()
        .trim_start_matches('.')
        .to_lowercase();
    let sanitized: String = normalized
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .take(12)
        .collect();
    if sanitized.is_empty() {
        "audio".to_string()
    } else {
        sanitized
    }
}

fn ensure_player_success(player: &str, status: ExitStatus) -> anyhow::Result<()> {
    if status.success() {
        Ok(())
    } else {
        bail!("audio player {player} failed with {status}")
    }
}

fn current_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}
