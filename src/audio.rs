use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::AsyncReadExt;
#[cfg(feature = "debug-recordings")]
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::Instant;

pub const SAMPLE_RATE: u32 = 24_000;
pub const CHANNELS: u16 = 1;
pub const SAMPLE_FORMAT: &str = "s16";
pub const CHUNK_BYTES: usize = 4_096;
pub const STT_SAMPLE_RATE: u32 = 16_000;
const STREAMING_CAPTURE_READY_TIMEOUT: Duration = Duration::from_secs(2);
const STREAMING_CAPTURE_READY_POLL: Duration = Duration::from_millis(10);

pub struct RawPcmRecording {
    child: Child,
    read_task: JoinHandle<Result<Vec<u8>>>,
}

pub type SharedPcmBuffer = Arc<Mutex<StreamingPcmBuffer>>;

pub struct StreamingPcmBuffer {
    pcm: Vec<u8>,
    idle_retain_bytes: usize,
    active_sessions: usize,
    bytes_seen: usize,
}

impl StreamingPcmBuffer {
    fn new(sample_rate: u32, idle_retain: Duration) -> Self {
        Self {
            pcm: Vec::new(),
            idle_retain_bytes: pcm_bytes_for_duration(sample_rate, idle_retain),
            active_sessions: 0,
            bytes_seen: 0,
        }
    }

    fn append(&mut self, chunk: &[u8]) {
        self.pcm.extend_from_slice(chunk);
        self.bytes_seen = self.bytes_seen.saturating_add(chunk.len());
        self.trim_if_idle();
    }

    fn has_seen_audio(&self) -> bool {
        self.bytes_seen > 0
    }

    fn len(&self) -> usize {
        self.pcm.len()
    }

    fn trim_if_idle(&mut self) {
        if self.active_sessions == 0 {
            self.trim_to_recent(self.idle_retain_bytes);
        }
    }

    fn trim_to_recent(&mut self, retain_bytes: usize) {
        let trim_count = self.pcm.len().saturating_sub(retain_bytes);
        if trim_count > 0 {
            self.pcm.drain(..trim_count);
        }
    }
}

pub struct StreamingPcmCapture {
    child: Option<Child>,
    pcm: SharedPcmBuffer,
    read_task: Option<JoinHandle<Result<()>>>,
}

impl StreamingPcmCapture {
    pub fn shared_pcm(&self) -> SharedPcmBuffer {
        Arc::clone(&self.pcm)
    }
}

impl Drop for StreamingPcmCapture {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
        if let Some(read_task) = self.read_task.as_ref() {
            read_task.abort();
        }
    }
}

#[derive(Clone)]
pub struct StreamingPcmSession {
    pcm: SharedPcmBuffer,
    preroll_pcm: Arc<Vec<u8>>,
    available_at_start_bytes: usize,
    finished: Arc<AtomicBool>,
}

impl StreamingPcmSession {
    pub async fn start(pcm: SharedPcmBuffer, sample_rate: u32, preroll: Duration) -> Result<Self> {
        let preroll_bytes = pcm_bytes_for_duration(sample_rate, preroll);
        let mut pcm_buffer = pcm.lock().await;
        if pcm_buffer.active_sessions > 0 {
            bail!("a recording is already active");
        }
        let available_at_start_bytes = pcm_buffer.len();
        let preroll_start = available_at_start_bytes.saturating_sub(preroll_bytes);
        let preroll_pcm = Arc::new(pcm_buffer.pcm[preroll_start..].to_vec());
        pcm_buffer.pcm.clear();
        pcm_buffer.active_sessions += 1;
        drop(pcm_buffer);

        Ok(Self {
            pcm,
            preroll_pcm,
            available_at_start_bytes,
            finished: Arc::new(AtomicBool::new(false)),
        })
    }

    pub async fn snapshot(&self) -> Vec<u8> {
        let pcm = self.pcm.lock().await;
        let mut snapshot = Vec::with_capacity(self.preroll_pcm.len() + pcm.pcm.len());
        snapshot.extend_from_slice(&self.preroll_pcm);
        snapshot.extend_from_slice(&pcm.pcm);
        snapshot
    }

    pub async fn snapshot_with_preroll_limit(
        &self,
        sample_rate: u32,
        preroll_limit: Duration,
    ) -> Vec<u8> {
        let preroll_limit_bytes =
            pcm_bytes_for_duration(sample_rate, preroll_limit).min(self.preroll_pcm.len());
        let preroll_start = self.preroll_pcm.len() - preroll_limit_bytes;
        let pcm = self.pcm.lock().await;
        let mut snapshot = Vec::with_capacity(preroll_limit_bytes + pcm.pcm.len());
        snapshot.extend_from_slice(&self.preroll_pcm[preroll_start..]);
        snapshot.extend_from_slice(&pcm.pcm);
        snapshot
    }

    pub fn retained_preroll(&self) -> &[u8] {
        &self.preroll_pcm
    }

    pub fn retained_preroll_bytes(&self) -> usize {
        self.preroll_pcm.len()
    }

    pub fn available_at_start_bytes(&self) -> usize {
        self.available_at_start_bytes
    }

    pub async fn finish(&self) {
        if self.finished.swap(true, Ordering::SeqCst) {
            return;
        }

        let mut pcm = self.pcm.lock().await;
        pcm.active_sessions = pcm.active_sessions.saturating_sub(1);
        pcm.trim_if_idle();
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

pub async fn start_streaming_pcm_capture(
    sample_rate: u32,
    idle_retain: Duration,
) -> Result<StreamingPcmCapture> {
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
    let pcm = Arc::new(Mutex::new(StreamingPcmBuffer::new(
        sample_rate,
        idle_retain,
    )));
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
            task_pcm.lock().await.append(&chunk[..read]);
        }
        Ok(())
    });

    let mut capture = StreamingPcmCapture {
        child: Some(child),
        pcm,
        read_task: Some(read_task),
    };
    if let Err(error) = wait_for_streaming_pcm(&capture.pcm, STREAMING_CAPTURE_READY_TIMEOUT).await
    {
        if let Some(child) = capture.child.as_mut() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        if let Some(read_task) = capture.read_task.take() {
            read_task.abort();
        }
        return Err(error);
    }

    Ok(capture)
}

async fn wait_for_streaming_pcm(pcm: &SharedPcmBuffer, timeout: Duration) -> Result<()> {
    tokio::time::timeout(timeout, async {
        loop {
            if pcm.lock().await.has_seen_audio() {
                return;
            }
            tokio::time::sleep(STREAMING_CAPTURE_READY_POLL).await;
        }
    })
    .await
    .context("timed out waiting for pw-record to produce audio")?;
    Ok(())
}

fn pcm_bytes_for_duration(sample_rate: u32, duration: Duration) -> usize {
    let samples = duration.as_secs_f64() * f64::from(sample_rate);
    samples.ceil() as usize * usize::from(CHANNELS) * 2
}

pub async fn stop_streaming_pcm_capture(mut capture: StreamingPcmCapture) -> Result<Vec<u8>> {
    if let Some(child) = capture.child.as_mut() {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    let read_task = capture
        .read_task
        .take()
        .context("pw-record reader task is unavailable")?;
    read_task
        .await
        .context("pw-record reader task panicked")??;

    let pcm = capture.pcm.lock().await.pcm.clone();
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

#[cfg(feature = "debug-recordings")]
pub async fn write_pcm_mp3(path: &Path, pcm: &[u8], sample_rate: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "s16le",
            "-ar",
            &sample_rate.to_string(),
            "-ac",
            &CHANNELS.to_string(),
            "-i",
            "pipe:0",
            "-codec:a",
            "libmp3lame",
            "-q:a",
            "2",
            "-y",
        ])
        .arg(path)
        .stdin(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("failed to start ffmpeg for MP3 recording snapshot")?;

    let mut stdin = child.stdin.take().context("ffmpeg did not expose stdin")?;
    stdin
        .write_all(pcm)
        .await
        .context("failed to stream PCM into ffmpeg")?;
    drop(stdin);

    let status = child
        .wait()
        .await
        .context("failed to wait for ffmpeg MP3 encoder")?;
    if !status.success() {
        bail!("ffmpeg failed to encode MP3 recording snapshot: {status}");
    }

    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wait_for_streaming_pcm_returns_after_audio_arrives() {
        let pcm = Arc::new(Mutex::new(StreamingPcmBuffer::new(
            4,
            Duration::from_secs(1),
        )));
        let task_pcm = Arc::clone(&pcm);

        tokio::spawn(async move {
            tokio::task::yield_now().await;
            task_pcm.lock().await.append(&[1, 2]);
        });

        wait_for_streaming_pcm(&pcm, Duration::from_secs(1))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn streaming_pcm_session_keeps_preroll_and_new_audio() {
        let pcm = Arc::new(Mutex::new(StreamingPcmBuffer::new(
            4,
            Duration::from_secs(1),
        )));
        pcm.lock().await.append(&[1, 2, 3, 4]);
        let session = StreamingPcmSession::start(Arc::clone(&pcm), 4, Duration::from_millis(250))
            .await
            .unwrap();

        pcm.lock().await.append(&[5, 6]);

        assert_eq!(session.snapshot().await, vec![3, 4, 5, 6]);
    }

    #[tokio::test]
    async fn streaming_pcm_session_can_limit_preroll_snapshot() {
        let pcm = Arc::new(Mutex::new(StreamingPcmBuffer::new(
            4,
            Duration::from_secs(1),
        )));
        pcm.lock().await.append(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let session = StreamingPcmSession::start(Arc::clone(&pcm), 4, Duration::from_secs(1))
            .await
            .unwrap();

        pcm.lock().await.append(&[9, 10]);

        assert_eq!(
            session
                .snapshot_with_preroll_limit(4, Duration::from_millis(250))
                .await,
            vec![7, 8, 9, 10]
        );
    }

    #[tokio::test]
    async fn streaming_pcm_buffer_trims_only_while_idle() {
        let pcm = Arc::new(Mutex::new(StreamingPcmBuffer::new(
            4,
            Duration::from_millis(250),
        )));
        pcm.lock().await.append(&[1, 2, 3, 4]);

        assert_eq!(pcm.lock().await.pcm.clone(), vec![3, 4]);

        let session = StreamingPcmSession::start(Arc::clone(&pcm), 4, Duration::from_millis(250))
            .await
            .unwrap();
        pcm.lock().await.append(&[5, 6, 7, 8]);

        assert_eq!(session.snapshot().await, vec![3, 4, 5, 6, 7, 8]);
        session.finish().await;
        assert_eq!(pcm.lock().await.pcm.clone(), vec![7, 8]);
    }

    #[tokio::test]
    async fn streaming_pcm_session_rejects_overlapping_recordings() {
        let pcm = Arc::new(Mutex::new(StreamingPcmBuffer::new(
            4,
            Duration::from_secs(1),
        )));
        pcm.lock().await.append(&[1, 2, 3, 4]);
        let session = StreamingPcmSession::start(Arc::clone(&pcm), 4, Duration::from_millis(250))
            .await
            .unwrap();

        let error =
            match StreamingPcmSession::start(Arc::clone(&pcm), 4, Duration::from_millis(250)).await
            {
                Ok(_) => panic!("overlapping recording should be rejected"),
                Err(error) => error,
            };

        assert!(error.to_string().contains("recording is already active"));
        session.finish().await;
        let next_session = StreamingPcmSession::start(pcm, 4, Duration::from_millis(250))
            .await
            .unwrap();
        next_session.finish().await;
    }

    #[tokio::test]
    async fn wait_for_streaming_pcm_accepts_zero_idle_retention() {
        let pcm = Arc::new(Mutex::new(StreamingPcmBuffer::new(4, Duration::ZERO)));
        let task_pcm = Arc::clone(&pcm);

        tokio::spawn(async move {
            tokio::task::yield_now().await;
            task_pcm.lock().await.append(&[1, 2]);
        });

        wait_for_streaming_pcm(&pcm, Duration::from_secs(1))
            .await
            .unwrap();
        assert!(pcm.lock().await.pcm.is_empty());
    }
}
