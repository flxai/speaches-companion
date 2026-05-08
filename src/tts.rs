use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context};
use reqwest::Client;
use serde::Serialize;
use tokio::process::Command;
use tokio::time::{sleep, Duration};
use url::Url;

#[derive(Debug, Clone, PartialEq)]
pub struct SpeechOptions {
    pub model: String,
    pub voice: String,
    pub speed: f32,
    pub response_format: String,
}

#[derive(Serialize)]
struct SpeechRequest<'a> {
    input: &'a str,
    model: &'a str,
    voice: &'a str,
    speed: f32,
    response_format: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackState {
    pid_path: PathBuf,
    lock_path: PathBuf,
}

impl PlaybackState {
    pub fn in_dir(dir: impl AsRef<Path>) -> Self {
        let dir = dir.as_ref();
        Self {
            pid_path: dir.join("read-aloud-player.pid"),
            lock_path: dir.join("read-aloud-player.lock"),
        }
    }
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
            speed: options.speed,
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
        "speaches-companion-tts-{}-{}.{}",
        std::process::id(),
        current_millis(),
        speech_file_extension(response_format)
    ));
    tokio::fs::write(&path, audio)
        .await
        .with_context(|| format!("failed to write speech audio to {}", path.display()))?;
    Ok(path)
}

pub async fn play_audio_file(
    path: &Path,
    player: &str,
    player_args: &[String],
) -> anyhow::Result<()> {
    let state = default_playback_state()?;
    play_audio_file_with_state(path, player, player_args, &state).await
}

pub async fn play_audio_file_with_state(
    path: &Path,
    player: &str,
    player_args: &[String],
    state: &PlaybackState,
) -> anyhow::Result<()> {
    let lock = lock_playback_state(state)?;
    stop_recorded_playback(state).await;

    let mut command = Command::new(player);
    command.args(player_args).arg(path);
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to run audio player {player}"))?;
    let child_pid = child.id().context("audio player did not expose a PID")? as i32;
    write_playback_pid(state, child_pid)?;
    drop(lock);

    let status = child
        .wait()
        .await
        .with_context(|| format!("failed to wait for audio player {player}"))?;
    remove_playback_pid_if_current(state, child_pid);
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
    if status.success() || playback_was_cancelled(status) {
        Ok(())
    } else {
        bail!("audio player {player} failed with {status}")
    }
}

fn playback_was_cancelled(status: ExitStatus) -> bool {
    matches!(status.signal(), Some(libc::SIGTERM | libc::SIGKILL))
}

fn default_playback_state() -> anyhow::Result<PlaybackState> {
    let dir = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(runtime_dir) if !runtime_dir.is_empty() => {
            PathBuf::from(runtime_dir).join("speaches-companion")
        }
        _ => {
            std::env::temp_dir().join(format!("speaches-companion-{}", unsafe { libc::geteuid() }))
        }
    };
    Ok(PlaybackState::in_dir(dir))
}

fn lock_playback_state(state: &PlaybackState) -> anyhow::Result<PlaybackLock> {
    if let Some(dir) = state.lock_path.parent() {
        fs::create_dir_all(dir).with_context(|| {
            format!(
                "failed to create playback state directory {}",
                dir.display()
            )
        })?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&state.lock_path)
        .with_context(|| format!("failed to open playback lock {}", state.lock_path.display()))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == -1 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("failed to lock {}", state.lock_path.display()));
    }
    Ok(PlaybackLock(file))
}

struct PlaybackLock(File);

impl Drop for PlaybackLock {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

async fn stop_recorded_playback(state: &PlaybackState) {
    let Some(pid) = read_playback_pid(state) else {
        return;
    };
    let _ = fs::remove_file(&state.pid_path);
    terminate_process_group(pid, libc::SIGTERM);
    sleep(Duration::from_millis(150)).await;
    if process_exists(pid) {
        terminate_process_group(pid, libc::SIGKILL);
    }
}

fn read_playback_pid(state: &PlaybackState) -> Option<i32> {
    fs::read_to_string(&state.pid_path)
        .ok()
        .and_then(|pid| pid.trim().parse::<i32>().ok())
        .filter(|pid| *pid > 0)
}

fn write_playback_pid(state: &PlaybackState, pid: i32) -> anyhow::Result<()> {
    if let Some(dir) = state.pid_path.parent() {
        fs::create_dir_all(dir).with_context(|| {
            format!(
                "failed to create playback state directory {}",
                dir.display()
            )
        })?;
    }
    fs::write(&state.pid_path, format!("{pid}\n")).with_context(|| {
        format!(
            "failed to write playback state {}",
            state.pid_path.display()
        )
    })
}

fn remove_playback_pid_if_current(state: &PlaybackState, pid: i32) {
    if read_playback_pid(state) == Some(pid) {
        let _ = fs::remove_file(&state.pid_path);
    }
}

fn terminate_process_group(pid: i32, signal: i32) {
    let _ = unsafe { libc::kill(-pid, signal) };
}

fn process_exists(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

fn current_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}
