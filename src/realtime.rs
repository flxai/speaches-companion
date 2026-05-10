#[cfg(feature = "debug-recordings")]
use std::path::PathBuf;
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
    capture_with_pw_record, pcm_bytes_for_duration, pcm_duration, start_streaming_pcm_capture,
    SharedPcmBuffer, StreamingPcmCapture, StreamingPcmSession, CHUNK_BYTES, SAMPLE_RATE,
};
use crate::config::{health_url, realtime_ws_url, DictateLiveConfig};
use crate::event::{classify_event, RealtimeEvent, RealtimeHypothesis};
use crate::phase::{PhaseGate, PhaseResult};
use crate::speech::{pcm_rms_s16le, DEFAULT_SPEECH_ANALYSIS_FRAME, DEFAULT_SPEECH_RMS_FLOOR};
#[cfg(feature = "debug-recordings")]
use crate::streaming::preserve_recording_snapshot;
use crate::streaming::{LiveTranscriber, LiveTranscriptUpdate, LiveTranscriptionSession};
use crate::trace::TraceWriter;

const REALTIME_WARMUP_DURATION: Duration = Duration::from_millis(500);
const REALTIME_COMPLETION_TIMEOUT: Duration = Duration::from_secs(30);
const REALTIME_NO_FINAL_DRAIN_TIMEOUT: Duration = Duration::from_millis(500);
const REALTIME_AUDIO_POLL: Duration = Duration::from_millis(40);
const REALTIME_SPEECH_GATE_PREROLL: Duration = Duration::from_millis(300);
const REALTIME_SPEECH_GATE_RMS_THRESHOLD: f64 = DEFAULT_SPEECH_RMS_FLOOR;
const REALTIME_SPEECH_GATE_FRAME: Duration = DEFAULT_SPEECH_ANALYSIS_FRAME;
const REALTIME_SPEECH_GATE_MIN_SPEECH: Duration = Duration::from_millis(160);

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
    #[cfg(feature = "debug-recordings")]
    record_dir: Option<PathBuf>,
    preroll: Duration,
    sample_rate: u32,
    final_pass: bool,
}

pub struct RealtimeSession {
    stop_tx: watch::Sender<bool>,
    sender_task: JoinHandle<Result<usize>>,
    receiver_task: JoinHandle<Result<String>>,
}

#[derive(Debug, Clone)]
struct RealtimeSpeechGate {
    speech_started: bool,
    pending: Vec<u8>,
    max_pending_bytes: usize,
}

impl RealtimeSpeechGate {
    fn new(sample_rate: u32) -> Self {
        Self {
            speech_started: false,
            pending: Vec::new(),
            max_pending_bytes: pcm_bytes_for_duration(sample_rate, REALTIME_SPEECH_GATE_PREROLL)
                .max(2),
        }
    }

    fn filter_new_audio(&mut self, new_audio: &[u8]) -> Vec<Vec<u8>> {
        let mut outgoing = Vec::new();
        for chunk in new_audio.chunks(CHUNK_BYTES) {
            if self.speech_started {
                outgoing.push(chunk.to_vec());
                continue;
            }

            self.pending.extend_from_slice(chunk);
            trim_vec_to_recent(&mut self.pending, self.max_pending_bytes);

            if pending_contains_sustained_speech(&self.pending) {
                self.speech_started = true;
                outgoing.extend(
                    self.pending
                        .chunks(CHUNK_BYTES)
                        .map(ToOwned::to_owned)
                        .collect::<Vec<_>>(),
                );
                self.pending.clear();
            }
        }
        outgoing
    }
}

#[derive(Debug, Default)]
struct RealtimeTranscriptAccumulator {
    completed_segments: Vec<String>,
    current_confirmed: String,
    current_text: String,
}

impl RealtimeTranscriptAccumulator {
    fn observe_delta(&mut self, delta: &str) -> Option<String> {
        if delta.trim().is_empty() {
            return None;
        }
        self.current_confirmed = append_transcript_text(&self.current_confirmed, delta);
        self.current_text = self.current_confirmed.clone();
        self.current()
    }

    fn observe_hypothesis(&mut self, hypothesis: &RealtimeHypothesis) -> Option<String> {
        let transcript = hypothesis.transcript.trim();
        if transcript.is_empty() {
            return None;
        }
        self.current_text = transcript.to_string();
        self.current().or_else(|| {
            combine_transcript_segments(self.completed_segments.iter().map(String::as_str))
        })
    }

    fn observe_completion(&mut self, final_transcript: &str) -> Option<String> {
        if let Some(segment) = normalized_transcript_segment(final_transcript) {
            self.completed_segments.push(segment);
        }
        self.current_confirmed.clear();
        self.current_text.clear();
        self.current()
    }

    fn current(&self) -> Option<String> {
        combine_transcript_segments(
            self.completed_segments
                .iter()
                .map(String::as_str)
                .chain(std::iter::once(self.current_text.as_str())),
        )
    }
}

fn normalized_transcript_segment(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

fn combine_transcript_segments<'a>(segments: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let mut combined = String::new();
    for segment in segments {
        let Some(segment) = normalized_transcript_segment(segment) else {
            continue;
        };
        push_transcript_segment(&mut combined, &segment);
    }
    if combined.is_empty() {
        None
    } else {
        Some(combined)
    }
}

fn push_transcript_segment(combined: &mut String, segment: &str) {
    if combined.is_empty()
        || combined.ends_with(char::is_whitespace)
        || segment.chars().next().is_some_and(is_leading_punctuation)
    {
        combined.push_str(segment);
    } else {
        combined.push(' ');
        combined.push_str(segment);
    }
}

fn append_transcript_text(prefix: &str, suffix: &str) -> String {
    let Some(suffix) = normalized_transcript_segment(suffix) else {
        return prefix.to_string();
    };
    let mut combined = prefix.to_string();
    push_transcript_segment(&mut combined, &suffix);
    combined
}

fn is_leading_punctuation(character: char) -> bool {
    matches!(
        character,
        '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '%' | '\'' | '"'
    )
}

fn is_realtime_commit_buffer_size_error(error: &str) -> bool {
    error.contains("buffer too small") || error.contains("buffer is empty")
}

fn current_or_empty(transcript: &RealtimeTranscriptAccumulator) -> String {
    transcript.current().unwrap_or_default()
}

fn completed_or_live_after_stop(
    transcript: &RealtimeTranscriptAccumulator,
    error: &str,
    stop_requested: bool,
) -> Option<String> {
    if stop_requested && is_realtime_commit_buffer_size_error(error) {
        transcript.current()
    } else {
        None
    }
}

async fn send_live_update(
    updates_tx: &mpsc::Sender<LiveTranscriptUpdate>,
    transcript: String,
) -> bool {
    updates_tx
        .send(LiveTranscriptUpdate { transcript })
        .await
        .is_ok()
}

impl RealtimeTranscriber {
    pub fn new(base_url: String, model: String, language: Option<String>) -> Self {
        Self {
            base_url,
            model,
            language,
            capture: Arc::new(Mutex::new(None)),
            #[cfg(feature = "debug-recordings")]
            record_dir: None,
            preroll: Duration::from_millis(750),
            sample_rate: SAMPLE_RATE,
            final_pass: true,
        }
    }

    pub fn with_preroll(mut self, preroll: Duration) -> Self {
        self.preroll = preroll;
        self
    }

    pub fn with_final_pass(mut self, final_pass: bool) -> Self {
        self.final_pass = final_pass;
        self
    }

    #[cfg(feature = "debug-recordings")]
    pub fn with_record_dir(mut self, record_dir: Option<PathBuf>) -> Self {
        self.record_dir = record_dir;
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

    async fn start_on_shared_pcm(
        &self,
        shared_pcm: SharedPcmBuffer,
        preroll: Duration,
        label: &str,
    ) -> Result<LiveTranscriptionSession<RealtimeSession>> {
        let pcm_session = StreamingPcmSession::start(shared_pcm, self.sample_rate, preroll).await?;
        eprintln!(
            "speaches-companion {label} audio buffer: {:.2}s available; retained {:.2}s pre-roll (requested {}ms)",
            pcm_duration(self.sample_rate, pcm_session.available_at_start_bytes()).as_secs_f64(),
            pcm_duration(self.sample_rate, pcm_session.retained_preroll_bytes()).as_secs_f64(),
            preroll.as_millis()
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
        let receiver_stop_rx = stop_rx.clone();
        #[cfg(feature = "debug-recordings")]
        let record_dir = self.record_dir.clone();
        let sample_rate = self.sample_rate;
        let final_pass = self.final_pass;

        let sender_task = tokio::spawn(async move {
            stream_realtime_session_audio(
                pcm_session,
                sample_rate,
                stop_rx,
                &mut sink,
                final_pass,
                #[cfg(feature = "debug-recordings")]
                record_dir,
            )
            .await
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
                            if !send_live_update(&updates_tx, transcript).await {
                                break;
                            }
                        }
                    }
                    RealtimeEvent::LiveHypothesis(hypothesis) => {
                        if let Some(transcript) = transcript.observe_hypothesis(&hypothesis) {
                            if !send_live_update(&updates_tx, transcript).await {
                                break;
                            }
                        }
                    }
                    RealtimeEvent::Completed(final_transcript) => {
                        if let Some(transcript) = transcript.observe_completion(&final_transcript) {
                            if !send_live_update(&updates_tx, transcript).await {
                                break;
                            }
                        }
                        if *receiver_stop_rx.borrow() {
                            return Ok(current_or_empty(&transcript));
                        }
                    }
                    RealtimeEvent::Failed(error) | RealtimeEvent::Error(error) => {
                        if let Some(transcript) = completed_or_live_after_stop(
                            &transcript,
                            &error,
                            *receiver_stop_rx.borrow(),
                        ) {
                            return Ok(transcript);
                        }
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
}

#[async_trait::async_trait]
impl LiveTranscriber for RealtimeTranscriber {
    type Session = RealtimeSession;

    async fn start(&self) -> Result<LiveTranscriptionSession<Self::Session>> {
        let shared_pcm = self.ensure_capture_started().await?;
        self.start_on_shared_pcm(shared_pcm, self.preroll, "realtime hotkey-down")
            .await
    }

    async fn stop(&self, session: Self::Session) -> Result<String> {
        let RealtimeSession {
            stop_tx,
            sender_task,
            receiver_task,
        } = session;
        let _ = stop_tx.send(true);
        let total_audio_bytes = sender_task
            .await
            .context("realtime audio sender task panicked")??;
        if total_audio_bytes == 0 {
            drain_realtime_receiver(receiver_task).await;
            return Ok(String::new());
        }
        if !self.final_pass {
            drain_realtime_receiver(receiver_task).await;
            return Ok(String::new());
        }
        timeout(REALTIME_COMPLETION_TIMEOUT, receiver_task)
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

pub async fn check_realtime(base_url: &str, model: &str, language: Option<&str>) -> Result<()> {
    preflight_health(base_url).await?;
    RealtimeTranscriber::new(
        base_url.to_string(),
        model.to_string(),
        language.map(str::to_string),
    )
    .warm_up()
    .await
}

async fn stream_realtime_session_audio<S>(
    pcm_session: StreamingPcmSession,
    sample_rate: u32,
    mut stop_rx: watch::Receiver<bool>,
    sink: &mut S,
    final_pass: bool,
    #[cfg(feature = "debug-recordings")] record_dir: Option<PathBuf>,
) -> Result<usize>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::error::Error + Send + Sync + 'static,
{
    let result = async {
        let mut cursor = 0usize;
        let mut total_sent = 0usize;
        let mut speech_gate = RealtimeSpeechGate::new(sample_rate);
        loop {
            total_sent +=
                send_available_realtime_audio(&pcm_session, sample_rate, &mut cursor, sink, &mut speech_gate).await?;
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
            send_available_realtime_audio(&pcm_session, sample_rate, &mut cursor, sink, &mut speech_gate).await?;
        if total_sent == 0 {
            eprintln!(
                "speaches-companion realtime audio gate detected no speech; skipping websocket commit"
            );
            sink.close()
                .await
                .context("failed to close realtime websocket")?;
        } else if final_pass {
            send_realtime_commit(sink).await?;
        } else {
            sink.close()
                .await
                .context("failed to close realtime websocket")?;
        }
        Result::<usize>::Ok(total_sent)
    }
    .await;
    #[cfg(feature = "debug-recordings")]
    {
        if let Some(record_dir) = record_dir.as_deref() {
            let snapshot = pcm_session.snapshot().await;
            match preserve_recording_snapshot(record_dir, &snapshot, sample_rate, "realtime").await
            {
                Ok(path) => eprintln!(
                    "speaches-companion preserved MP3 recording at {}",
                    path.display()
                ),
                Err(error) => {
                    eprintln!("speaches-companion failed to preserve MP3 recording: {error:#}")
                }
            }
        }
    }
    pcm_session.finish().await;
    result
}

async fn send_available_realtime_audio<S>(
    pcm_session: &StreamingPcmSession,
    sample_rate: u32,
    cursor: &mut usize,
    sink: &mut S,
    speech_gate: &mut RealtimeSpeechGate,
) -> Result<usize>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::error::Error + Send + Sync + 'static,
{
    let snapshot = pcm_session
        .snapshot_with_preroll_limit(sample_rate, Duration::from_secs(30))
        .await;
    let chunks = realtime_audio_chunks_since(&snapshot, cursor, speech_gate);
    let byte_count = chunks.iter().map(Vec::len).sum();
    for chunk in chunks {
        send_realtime_audio_chunk(sink, chunk).await?;
    }
    Ok(byte_count)
}

fn realtime_audio_chunks_since(
    snapshot: &[u8],
    cursor: &mut usize,
    speech_gate: &mut RealtimeSpeechGate,
) -> Vec<Vec<u8>> {
    if *cursor >= snapshot.len() {
        *cursor = snapshot.len();
        return Vec::new();
    }

    let chunks = speech_gate.filter_new_audio(&snapshot[*cursor..]);
    *cursor = snapshot.len();
    chunks
}

async fn drain_realtime_receiver(mut receiver_task: JoinHandle<Result<String>>) {
    tokio::select! {
        result = &mut receiver_task => {
            match result {
                Ok(Ok(_)) | Ok(Err(_)) | Err(_) => {}
            }
        }
        _ = sleep(REALTIME_NO_FINAL_DRAIN_TIMEOUT) => {
            receiver_task.abort();
            let _ = receiver_task.await;
        }
    }
}

fn pending_contains_sustained_speech(pcm: &[u8]) -> bool {
    let frame_bytes = pcm_bytes_for_duration(SAMPLE_RATE, REALTIME_SPEECH_GATE_FRAME).max(2);
    let min_speech_bytes =
        pcm_bytes_for_duration(SAMPLE_RATE, REALTIME_SPEECH_GATE_MIN_SPEECH).max(2);
    let mut sustained_bytes = 0usize;

    for frame in pcm.chunks(frame_bytes) {
        if pcm_rms_s16le(frame) >= REALTIME_SPEECH_GATE_RMS_THRESHOLD {
            sustained_bytes += frame.len();
            if sustained_bytes >= min_speech_bytes {
                return true;
            }
        } else {
            sustained_bytes = 0;
        }
    }

    false
}

fn trim_vec_to_recent(buffer: &mut Vec<u8>, retain_bytes: usize) {
    let trim_count = buffer.len().saturating_sub(retain_bytes);
    if trim_count > 0 {
        buffer.drain(..trim_count);
    }
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
    use futures_util::{SinkExt, StreamExt};
    use serde_json::Value;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    #[test]
    fn realtime_transcript_accumulator_emits_full_running_text() {
        let mut transcript = RealtimeTranscriptAccumulator::default();

        assert_eq!(transcript.observe_delta("the "), Some("the".to_string()));
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
    fn realtime_transcript_accumulator_replaces_live_hypothesis() {
        let mut transcript = RealtimeTranscriptAccumulator::default();

        assert_eq!(
            transcript.observe_hypothesis(&hypothesis("hello")),
            Some("hello".to_string())
        );
        assert_eq!(
            transcript.observe_hypothesis(&hypothesis("the front")),
            Some("the front".to_string())
        );
        assert_eq!(transcript.observe_hypothesis(&hypothesis("")), None);
    }

    #[test]
    fn realtime_transcript_accumulator_combines_confirmed_and_hypothesis_text() {
        let mut transcript = RealtimeTranscriptAccumulator::default();

        assert_eq!(
            transcript.observe_hypothesis(&hypothesis("the front fell")),
            Some("the front fell".to_string())
        );
        assert_eq!(
            transcript.observe_delta("the front"),
            Some("the front".to_string())
        );
        assert_eq!(
            transcript.observe_hypothesis(&hypothesis("the front fell off")),
            Some("the front fell off".to_string())
        );
    }

    #[test]
    fn realtime_transcript_accumulator_treats_completion_as_segment_boundary() {
        let mut transcript = RealtimeTranscriptAccumulator::default();

        assert_eq!(
            transcript.observe_delta("the front"),
            Some("the front".to_string())
        );
        assert_eq!(
            transcript.observe_completion("the front fell off"),
            Some("the front fell off".to_string())
        );
        assert_eq!(
            transcript.observe_delta("and sank"),
            Some("the front fell off and sank".to_string())
        );
    }

    #[test]
    fn realtime_transcript_accumulator_joins_completed_segments() {
        let mut transcript = RealtimeTranscriptAccumulator::default();

        assert_eq!(
            transcript.observe_completion("Also ich sehe das."),
            Some("Also ich sehe das.".to_string())
        );
        assert_eq!(
            transcript.observe_completion("Das funktioniert."),
            Some("Also ich sehe das. Das funktioniert.".to_string())
        );
    }

    #[test]
    fn realtime_transcript_accumulator_keeps_leading_punctuation_attached() {
        let mut transcript = RealtimeTranscriptAccumulator::default();

        assert_eq!(
            transcript.observe_completion("hello"),
            Some("hello".to_string())
        );
        assert_eq!(
            transcript.observe_delta(", world"),
            Some("hello, world".to_string())
        );
    }

    fn hypothesis(transcript: &str) -> RealtimeHypothesis {
        RealtimeHypothesis {
            transcript: transcript.to_string(),
            confirmed_prefix: String::new(),
            provisional: transcript.to_string(),
        }
    }

    #[test]
    fn realtime_buffer_size_error_after_stop_can_return_existing_text() {
        let mut transcript = RealtimeTranscriptAccumulator::default();
        transcript.observe_completion("the front fell off");

        assert_eq!(
            completed_or_live_after_stop(
                &transcript,
                "Error committing input audio buffer: buffer too small. Expected at least 100ms of audio, but buffer only has 0.00ms of audio.",
                true,
            ),
            Some("the front fell off".to_string())
        );
        assert_eq!(
            completed_or_live_after_stop(
                &transcript,
                "Error committing input audio buffer: buffer too small.",
                false,
            ),
            None
        );
    }

    #[test]
    fn realtime_audio_chunks_since_returns_only_new_audio() {
        let mut cursor = 0;
        let snapshot = vec![1u8; CHUNK_BYTES + 3];
        let mut speech_gate = RealtimeSpeechGate::new(SAMPLE_RATE);
        speech_gate.speech_started = true;

        let chunks = realtime_audio_chunks_since(&snapshot, &mut cursor, &mut speech_gate);

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), CHUNK_BYTES);
        assert_eq!(chunks[1].len(), 3);
        assert_eq!(cursor, snapshot.len());
        assert!(realtime_audio_chunks_since(&snapshot, &mut cursor, &mut speech_gate).is_empty());
    }

    #[test]
    fn realtime_audio_chunks_since_suppresses_noise_until_speech() {
        let mut cursor = 0;
        let mut speech_gate = RealtimeSpeechGate::new(SAMPLE_RATE);
        let noise = vec![0u8; CHUNK_BYTES];
        let speech = vec![0x20u8; CHUNK_BYTES];
        let snapshot = [noise, speech.clone(), speech].concat();

        let chunks = realtime_audio_chunks_since(&snapshot, &mut cursor, &mut speech_gate);

        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].len(), CHUNK_BYTES);
        assert_eq!(chunks[1].len(), CHUNK_BYTES);
        assert_eq!(chunks[2].len(), CHUNK_BYTES);
    }

    #[test]
    fn realtime_audio_chunks_since_ignores_single_loud_chunk() {
        let mut cursor = 0;
        let mut speech_gate = RealtimeSpeechGate::new(SAMPLE_RATE);
        let noise = vec![0u8; CHUNK_BYTES];
        let speech = vec![0x20u8; CHUNK_BYTES];
        let snapshot = [noise, speech].concat();

        let chunks = realtime_audio_chunks_since(&snapshot, &mut cursor, &mut speech_gate);

        assert!(chunks.is_empty());
    }

    #[tokio::test]
    async fn realtime_warmup_sends_silence_and_commit() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            let append = ws.next().await.unwrap().unwrap().into_text().unwrap();
            let append: Value = serde_json::from_str(&append).unwrap();
            assert_eq!(append["type"], "input_audio_buffer.append");
            assert!(append["audio"].as_str().unwrap().len() > 100);

            let commit = ws.next().await.unwrap().unwrap().into_text().unwrap();
            let commit: Value = serde_json::from_str(&commit).unwrap();
            assert_eq!(commit["type"], "input_audio_buffer.commit");

            ws.send(Message::Text(
                json!({
                    "type": "conversation.item.input_audio_transcription.completed",
                    "transcript": ""
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        });
        let transcriber = RealtimeTranscriber::new(
            format!("http://{address}"),
            "model/name".to_string(),
            Some("de".to_string()),
        );

        transcriber.warm_up().await.unwrap();

        server.await.unwrap();
    }
}
