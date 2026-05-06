use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch, Mutex};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::audio::{
    capture_with_pw_record, start_streaming_pcm_capture, SharedPcmBuffer, StreamingPcmCapture,
    StreamingPcmSession, CHANNELS, CHUNK_BYTES, SAMPLE_RATE,
};
use crate::config::{health_url, realtime_ws_url, DictateLiveConfig};
use crate::event::{classify_event, RealtimeEvent};
use crate::phase::{PhaseGate, PhaseResult};
use crate::streaming::{LiveTranscriber, LiveTranscriptUpdate, LiveTranscriptionSession};
use crate::trace::TraceWriter;

const REALTIME_WARMUP_DURATION: Duration = Duration::from_millis(500);
const REALTIME_COMPLETION_TIMEOUT: Duration = Duration::from_secs(30);
const REALTIME_AUDIO_POLL: Duration = Duration::from_millis(40);

pub struct RunOutcome {
    pub result: PhaseResult,
    pub trace_path: std::path::PathBuf,
    pub total_audio_bytes: usize,
}

#[derive(Clone)]
pub struct RealtimeTranscriber {
    base_url: String,
    model: String,
    language: Option<String>,
    capture: Arc<Mutex<Option<StreamingPcmCapture>>>,
    preroll: Duration,
    sample_rate: u32,
}

pub struct RealtimeSession {
    stop_tx: watch::Sender<bool>,
    sender_task: JoinHandle<Result<usize>>,
    receiver_task: JoinHandle<Result<String>>,
}

#[derive(Debug, Default)]
struct RealtimeTranscriptAccumulator {
    text: String,
}

impl RealtimeTranscriptAccumulator {
    fn observe_delta(&mut self, delta: &str) -> Option<String> {
        if delta.trim().is_empty() {
            return None;
        }
        self.text.push_str(delta);
        Some(self.text.clone())
    }

    fn current(&self) -> Option<String> {
        if self.text.trim().is_empty() {
            None
        } else {
            Some(self.text.clone())
        }
    }
}

impl RealtimeTranscriber {
    pub fn new(base_url: String, model: String, language: Option<String>) -> Self {
        Self {
            base_url,
            model,
            language,
            capture: Arc::new(Mutex::new(None)),
            preroll: Duration::from_millis(750),
            sample_rate: SAMPLE_RATE,
        }
    }

    pub fn with_preroll(mut self, preroll: Duration) -> Self {
        self.preroll = preroll;
        self
    }

    pub async fn prepare_capture(&self) -> Result<()> {
        self.ensure_capture_started().await.map(|_| ())
    }

    pub async fn warm_up(&self) -> Result<()> {
        let ws_url = realtime_ws_url(&self.base_url, &self.model, self.language.as_deref())?;
        let (ws_stream, _) = connect_async(&ws_url).await.with_context(|| {
            format!("failed to connect to Speaches realtime websocket {ws_url}")
        })?;
        let (mut sink, mut stream) = ws_stream.split();
        let silence = vec![0; pcm_bytes_for_duration(self.sample_rate, REALTIME_WARMUP_DURATION)];
        send_realtime_audio_chunk(&mut sink, silence).await?;
        send_realtime_commit(&mut sink).await?;

        timeout(REALTIME_COMPLETION_TIMEOUT, async {
            while let Some(message) = stream.next().await {
                let message = message.context("failed to receive websocket message")?;
                let Some(text) = websocket_text(message)? else {
                    continue;
                };
                let value: Value = serde_json::from_str(&text)
                    .with_context(|| format!("invalid server event JSON: {text}"))?;
                match classify_event(&value) {
                    RealtimeEvent::Completed(_) => return Ok(()),
                    RealtimeEvent::Failed(error) | RealtimeEvent::Error(error) => {
                        bail!("realtime warmup failed: {error}")
                    }
                    _ => {}
                }
            }
            bail!("realtime warmup websocket closed before completion")
        })
        .await
        .context("timed out waiting for realtime warmup completion")?
    }

    async fn ensure_capture_started(&self) -> Result<SharedPcmBuffer> {
        let mut capture = self.capture.lock().await;
        if let Some(capture) = capture.as_ref() {
            return Ok(capture.shared_pcm());
        }

        let started_capture = start_streaming_pcm_capture(self.sample_rate, self.preroll).await?;
        let shared_pcm = started_capture.shared_pcm();
        *capture = Some(started_capture);
        Ok(shared_pcm)
    }
}

#[async_trait::async_trait]
impl LiveTranscriber for RealtimeTranscriber {
    type Session = RealtimeSession;

    async fn start(&self) -> Result<LiveTranscriptionSession<Self::Session>> {
        let shared_pcm = self.ensure_capture_started().await?;
        let pcm_session =
            StreamingPcmSession::start(shared_pcm, self.sample_rate, self.preroll).await?;
        eprintln!(
            "speaches-scribe realtime hotkey-down audio buffer: {:.2}s available; retained {:.2}s pre-roll (requested {}ms)",
            pcm_duration(self.sample_rate, pcm_session.available_at_start_bytes()).as_secs_f64(),
            pcm_duration(self.sample_rate, pcm_session.retained_preroll_bytes()).as_secs_f64(),
            self.preroll.as_millis()
        );
        let ws_url = realtime_ws_url(&self.base_url, &self.model, self.language.as_deref())?;
        let (ws_stream, _) = match connect_async(&ws_url).await {
            Ok(connection) => connection,
            Err(error) => {
                pcm_session.finish().await;
                return Err(error).with_context(|| {
                    format!("failed to connect to Speaches realtime websocket {ws_url}")
                });
            }
        };
        let (mut sink, mut stream) = ws_stream.split();
        let (updates_tx, updates_rx) = mpsc::channel::<LiveTranscriptUpdate>(32);
        let (stop_tx, stop_rx) = watch::channel(false);
        let sample_rate = self.sample_rate;

        let sender_task = tokio::spawn(async move {
            stream_realtime_session_audio(pcm_session, sample_rate, stop_rx, &mut sink).await
        });
        let receiver_task = tokio::spawn(async move {
            let mut transcript = RealtimeTranscriptAccumulator::default();
            while let Some(message) = stream.next().await {
                let message = message.context("failed to receive websocket message")?;
                let Some(text) = websocket_text(message)? else {
                    continue;
                };
                let value: Value = serde_json::from_str(&text)
                    .with_context(|| format!("invalid server event JSON: {text}"))?;
                match classify_event(&value) {
                    RealtimeEvent::LiveDelta(delta) => {
                        if let Some(transcript) = transcript.observe_delta(&delta) {
                            if updates_tx
                                .send(LiveTranscriptUpdate { transcript })
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                    RealtimeEvent::Completed(final_transcript) => return Ok(final_transcript),
                    RealtimeEvent::Failed(error) | RealtimeEvent::Error(error) => {
                        bail!("realtime transcription failed: {error}")
                    }
                    RealtimeEvent::Other(_) => {}
                }
            }
            transcript.current().ok_or_else(|| {
                anyhow::anyhow!("realtime websocket closed before transcription completed")
            })
        });

        Ok(LiveTranscriptionSession {
            session: RealtimeSession {
                stop_tx,
                sender_task,
                receiver_task,
            },
            updates: updates_rx,
        })
    }

    async fn stop(&self, session: Self::Session) -> Result<String> {
        let _ = session.stop_tx.send(true);
        let total_audio_bytes = session
            .sender_task
            .await
            .context("realtime audio sender task panicked")??;
        if total_audio_bytes == 0 {
            bail!("recording stopped before any audio was captured");
        }
        timeout(REALTIME_COMPLETION_TIMEOUT, session.receiver_task)
            .await
            .context("timed out waiting for realtime transcription completion")?
            .context("realtime receiver task panicked")?
    }
}

pub async fn preflight_health(base_url: &str) -> Result<()> {
    let url = health_url(base_url)?;
    let response = reqwest::get(&url)
        .await
        .with_context(|| format!("failed to reach Speaches health endpoint {url}"))?;
    if !response.status().is_success() {
        bail!(
            "Speaches health endpoint {url} returned {}",
            response.status()
        );
    }
    Ok(())
}

async fn stream_realtime_session_audio<S>(
    pcm_session: StreamingPcmSession,
    sample_rate: u32,
    mut stop_rx: watch::Receiver<bool>,
    sink: &mut S,
) -> Result<usize>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::error::Error + Send + Sync + 'static,
{
    let result = async {
        let mut cursor = 0usize;
        let mut total_sent = 0usize;
        loop {
            total_sent +=
                send_available_realtime_audio(&pcm_session, sample_rate, &mut cursor, sink).await?;
            if *stop_rx.borrow() {
                break;
            }
            tokio::select! {
                changed = stop_rx.changed() => {
                    let _ = changed;
                }
                _ = sleep(REALTIME_AUDIO_POLL) => {}
            }
        }
        total_sent +=
            send_available_realtime_audio(&pcm_session, sample_rate, &mut cursor, sink).await?;
        send_realtime_commit(sink).await?;
        Result::<usize>::Ok(total_sent)
    }
    .await;
    pcm_session.finish().await;
    result
}

async fn send_available_realtime_audio<S>(
    pcm_session: &StreamingPcmSession,
    sample_rate: u32,
    cursor: &mut usize,
    sink: &mut S,
) -> Result<usize>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::error::Error + Send + Sync + 'static,
{
    let snapshot = pcm_session
        .snapshot_with_preroll_limit(sample_rate, Duration::from_secs(30))
        .await;
    let chunks = realtime_audio_chunks_since(&snapshot, cursor);
    let byte_count = chunks.iter().map(Vec::len).sum();
    for chunk in chunks {
        send_realtime_audio_chunk(sink, chunk).await?;
    }
    Ok(byte_count)
}

fn realtime_audio_chunks_since(snapshot: &[u8], cursor: &mut usize) -> Vec<Vec<u8>> {
    if *cursor >= snapshot.len() {
        *cursor = snapshot.len();
        return Vec::new();
    }

    let chunks = snapshot[*cursor..]
        .chunks(CHUNK_BYTES)
        .map(ToOwned::to_owned)
        .collect();
    *cursor = snapshot.len();
    chunks
}

async fn send_realtime_audio_chunk<S>(sink: &mut S, chunk: Vec<u8>) -> Result<()>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::error::Error + Send + Sync + 'static,
{
    let event = json!({
        "type": "input_audio_buffer.append",
        "audio": STANDARD.encode(chunk),
    });
    sink.send(Message::Text(event.to_string().into()))
        .await
        .context("failed to send audio chunk")
}

async fn send_realtime_commit<S>(sink: &mut S) -> Result<()>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::error::Error + Send + Sync + 'static,
{
    sink.send(Message::Text(
        json!({"type": "input_audio_buffer.commit"})
            .to_string()
            .into(),
    ))
    .await
    .context("failed to send input_audio_buffer.commit")
}

pub async fn run_dictate_live(config: DictateLiveConfig) -> Result<RunOutcome> {
    preflight_health(&config.base_url).await?;
    let ws_url = realtime_ws_url(&config.base_url, &config.model, config.language.as_deref())?;
    let (ws_stream, _) = connect_async(&ws_url)
        .await
        .with_context(|| format!("failed to connect to Speaches realtime websocket {ws_url}"))?;
    let (mut sink, mut stream) = ws_stream.split();
    let (audio_tx, mut audio_rx) = mpsc::channel::<Vec<u8>>(32);
    let duration = Duration::from_secs(config.duration_seconds);

    let capture_task = tokio::spawn(async move {
        capture_with_pw_record(duration, |chunk| {
            let audio_tx = audio_tx.clone();
            async move {
                audio_tx
                    .send(chunk)
                    .await
                    .context("failed to queue audio chunk for websocket")
            }
        })
        .await
    });

    let sender_task = tokio::spawn(async move {
        while let Some(chunk) = audio_rx.recv().await {
            let event = json!({
                "type": "input_audio_buffer.append",
                "audio": STANDARD.encode(chunk),
            });
            sink.send(Message::Text(event.to_string().into()))
                .await
                .context("failed to send audio chunk")?;
        }

        sink.send(Message::Text(
            json!({"type": "input_audio_buffer.commit"})
                .to_string()
                .into(),
        ))
        .await
        .context("failed to send input_audio_buffer.commit")?;
        Result::<()>::Ok(())
    });

    let mut trace = TraceWriter::default();
    let mut gate = PhaseGate::default();

    while let Some(message) = stream.next().await {
        let message = message.context("failed to receive websocket message")?;
        let Some(text) = websocket_text(message)? else {
            continue;
        };
        println!("{text}");
        let value: Value = serde_json::from_str(&text)
            .with_context(|| format!("invalid server event JSON: {text}"))?;
        let classified = classify_event(&value);
        gate.observe(&classified);
        trace.push(value);

        match classified {
            RealtimeEvent::Completed(_) | RealtimeEvent::Failed(_) | RealtimeEvent::Error(_) => {
                break
            }
            _ => {}
        }
    }

    let total_audio_bytes = capture_task
        .await
        .context("audio capture task panicked")??;
    sender_task
        .await
        .context("websocket sender task panicked")??;
    trace.write_jsonl(&config.trace_path).await?;

    Ok(RunOutcome {
        result: gate.result(),
        trace_path: config.trace_path,
        total_audio_bytes,
    })
}

fn pcm_bytes_for_duration(sample_rate: u32, duration: Duration) -> usize {
    let samples = duration.as_secs_f64() * f64::from(sample_rate);
    samples.ceil() as usize * usize::from(CHANNELS) * 2
}

fn pcm_duration(sample_rate: u32, byte_len: usize) -> Duration {
    let samples = byte_len / 2 / usize::from(CHANNELS);
    Duration::from_secs_f64(samples as f64 / f64::from(sample_rate))
}

fn websocket_text(message: Message) -> Result<Option<String>> {
    match message {
        Message::Text(text) => Ok(Some(text.to_string())),
        Message::Binary(bytes) => String::from_utf8(bytes.to_vec())
            .map(Some)
            .context("received non-UTF8 binary websocket message"),
        Message::Ping(_) | Message::Pong(_) => Ok(None),
        Message::Close(_) => Ok(None),
        Message::Frame(_) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn realtime_transcript_accumulator_emits_full_running_text() {
        let mut transcript = RealtimeTranscriptAccumulator::default();

        assert_eq!(transcript.observe_delta("the "), Some("the ".to_string()));
        assert_eq!(
            transcript.observe_delta("front"),
            Some("the front".to_string())
        );
        assert_eq!(
            transcript.observe_delta(" fell off"),
            Some("the front fell off".to_string())
        );
    }

    #[test]
    fn realtime_transcript_accumulator_ignores_empty_delta() {
        let mut transcript = RealtimeTranscriptAccumulator::default();

        assert_eq!(transcript.observe_delta("   "), None);
        assert_eq!(transcript.current(), None);
    }

    #[test]
    fn realtime_audio_chunks_since_returns_only_new_audio() {
        let mut cursor = 0;
        let snapshot = vec![1u8; CHUNK_BYTES + 3];

        let chunks = realtime_audio_chunks_since(&snapshot, &mut cursor);

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), CHUNK_BYTES);
        assert_eq!(chunks[1].len(), 3);
        assert_eq!(cursor, snapshot.len());
        assert!(realtime_audio_chunks_since(&snapshot, &mut cursor).is_empty());
    }
}
