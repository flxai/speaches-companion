use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::Instant;

pub const SAMPLE_RATE: u32 = 24_000;
pub const CHANNELS: u16 = 1;
pub const SAMPLE_FORMAT: &str = "s16";
pub const CHUNK_BYTES: usize = 4_096;
pub const STT_SAMPLE_RATE: u32 = 16_000;

pub struct RawPcmRecording {
    child: Child,
    read_task: JoinHandle<Result<Vec<u8>>>,
}

pub type SharedPcmBuffer = Arc<Mutex<Vec<u8>>>;

pub struct StreamingPcmCapture {
    child: Child,
    pcm: SharedPcmBuffer,
    read_task: JoinHandle<Result<()>>,
}

impl StreamingPcmCapture {
    pub fn shared_pcm(&self) -> SharedPcmBuffer {
        Arc::clone(&self.pcm)
    }
}

pub async fn capture_with_pw_record<F, Fut>(duration: Duration, mut on_chunk: F) -> Result<usize>
where
    F: FnMut(Vec<u8>) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let mut child = Command::new("pw-record")
        .args([
            "--raw",
            "--rate",
            &SAMPLE_RATE.to_string(),
            "--channels",
            &CHANNELS.to_string(),
            "--format",
            SAMPLE_FORMAT,
            "-",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("failed to start pw-record")?;

    let mut stdout = child
        .stdout
        .take()
        .context("pw-record did not expose stdout")?;
    let deadline = Instant::now() + duration;
    let mut total_bytes = 0usize;

    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }

        let remaining = deadline.saturating_duration_since(now);
        let mut chunk = vec![0u8; CHUNK_BYTES];
        let read = match tokio::time::timeout(remaining, stdout.read(&mut chunk)).await {
            Ok(read) => read.context("failed to read from pw-record")?,
            Err(_) => break,
        };
        if read == 0 {
            break;
        }

        chunk.truncate(read);
        total_bytes += read;
        on_chunk(chunk).await?;
    }

    let _ = child.start_kill();
    let _ = child.wait().await;
    Ok(total_bytes)
}

pub async fn start_raw_pcm_recording(sample_rate: u32) -> Result<RawPcmRecording> {
    let mut child = Command::new("pw-record")
        .args([
            "--raw",
            "--rate",
            &sample_rate.to_string(),
            "--channels",
            &CHANNELS.to_string(),
            "--format",
            SAMPLE_FORMAT,
            "-",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("failed to start pw-record")?;

    let mut stdout = child
        .stdout
        .take()
        .context("pw-record did not expose stdout")?;
    let read_task = tokio::spawn(async move {
        let mut pcm = Vec::new();
        let mut chunk = vec![0u8; CHUNK_BYTES];
        loop {
            let read = stdout
                .read(&mut chunk)
                .await
                .context("failed to read from pw-record")?;
            if read == 0 {
                break;
            }
            pcm.extend_from_slice(&chunk[..read]);
        }
        Ok(pcm)
    });

    Ok(RawPcmRecording { child, read_task })
}

pub async fn start_streaming_pcm_capture(sample_rate: u32) -> Result<StreamingPcmCapture> {
    let mut child = Command::new("pw-record")
        .args([
            "--raw",
            "--rate",
            &sample_rate.to_string(),
            "--channels",
            &CHANNELS.to_string(),
            "--format",
            SAMPLE_FORMAT,
            "-",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("failed to start pw-record")?;

    let mut stdout = child
        .stdout
        .take()
        .context("pw-record did not expose stdout")?;
    let pcm = Arc::new(Mutex::new(Vec::new()));
    let task_pcm = Arc::clone(&pcm);
    let read_task = tokio::spawn(async move {
        let mut chunk = vec![0u8; CHUNK_BYTES];
        loop {
            let read = stdout
                .read(&mut chunk)
                .await
                .context("failed to read from pw-record")?;
            if read == 0 {
                break;
            }
            task_pcm.lock().await.extend_from_slice(&chunk[..read]);
        }
        Ok(())
    });

    Ok(StreamingPcmCapture {
        child,
        pcm,
        read_task,
    })
}

pub async fn stop_streaming_pcm_capture(mut capture: StreamingPcmCapture) -> Result<Vec<u8>> {
    let _ = capture.child.start_kill();
    let _ = capture.child.wait().await;
    capture
        .read_task
        .await
        .context("pw-record reader task panicked")??;

    let pcm = capture.pcm.lock().await.clone();
    if pcm.is_empty() {
        bail!("recording stopped before any audio was captured");
    }
    Ok(pcm)
}

pub async fn stop_raw_pcm_recording(
    mut recording: RawPcmRecording,
    output_path: &Path,
    sample_rate: u32,
) -> Result<()> {
    let _ = recording.child.start_kill();
    let _ = recording.child.wait().await;

    let pcm = recording
        .read_task
        .await
        .context("pw-record reader task panicked")??;
    if pcm.is_empty() {
        bail!("recording stopped before any audio was captured");
    }

    write_pcm_wav(output_path, &pcm, sample_rate).await
}

pub async fn write_pcm_wav(path: &Path, pcm: &[u8], sample_rate: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let data_len = u32::try_from(pcm.len()).context("recording is too large for WAV")?;
    let channels = CHANNELS;
    let bits_per_sample = 16u16;
    let byte_rate = sample_rate * u32::from(channels) * u32::from(bits_per_sample / 8);
    let block_align = channels * (bits_per_sample / 8);

    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36u32 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&bits_per_sample.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(pcm);

    tokio::fs::write(path, wav)
        .await
        .with_context(|| format!("failed to write WAV file {}", path.display()))
}

pub async fn record_wav_with_pw_record(
    path: &Path,
    duration: Duration,
    sample_rate: u32,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let _ = tokio::fs::remove_file(path).await;

    let sample_count = duration
        .as_secs()
        .saturating_mul(sample_rate as u64)
        .saturating_add((duration.subsec_nanos() as u64 * sample_rate as u64) / 1_000_000_000)
        .max(1);

    let status = Command::new("pw-record")
        .args([
            "--rate",
            &sample_rate.to_string(),
            "--channels",
            "1",
            "--format",
            "s16",
            "--sample-count",
            &sample_count.to_string(),
        ])
        .arg(path)
        .status()
        .await
        .context("failed to start pw-record")?;

    if !status.success() {
        if tokio::fs::metadata(path)
            .await
            .map(|metadata| metadata.len() > 44)
            .unwrap_or(false)
        {
            return Ok(());
        }
        anyhow::bail!("pw-record failed with {status}");
    }
    Ok(())
}
