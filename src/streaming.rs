use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::audio::{
    start_streaming_pcm_capture, stop_streaming_pcm_capture, write_pcm_wav, SharedPcmBuffer,
    STT_SAMPLE_RATE,
};
use crate::daemon::{DaemonResponse, HotkeyHandler};
use crate::inject::{normalize_transcript_for_injection, SpeculativeTextSession, TextInjector};
use crate::ipc::IpcCommand;
use crate::notification::{
    dictation_error_body, ErrorNotifier, NoopErrorNotifier, NoopTranscriptNotifier,
    TranscriptNotifier, DICTATION_ERROR_SUMMARY,
};
use crate::stt::{transcribe_file, TranscribeOptions};

static TEMP_AUDIO_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveTranscriptUpdate {
    pub transcript: String,
}

pub struct LiveTranscriptionSession<S> {
    pub session: S,
    pub updates: mpsc::Receiver<LiveTranscriptUpdate>,
}

#[async_trait]
pub trait LiveTranscriber: Send + Sync {
    type Session: Send + 'static;

    async fn start(&self) -> anyhow::Result<LiveTranscriptionSession<Self::Session>>;
    async fn stop(&self, session: Self::Session) -> anyhow::Result<String>;
}

pub struct StreamingDictationController<L, I, V = NoopTranscriptNotifier, N = NoopErrorNotifier>
where
    L: LiveTranscriber,
    I: TextInjector + Clone + Send + 'static,
    V: TranscriptNotifier,
    N: ErrorNotifier + Clone + Send + 'static,
{
    transcriber: L,
    injector: I,
    transcript_notifier: V,
    error_notifier: N,
    listening_marker: Option<String>,
    inline_partials: bool,
    active: Option<ActiveStreamingSession<L::Session, I>>,
}

struct ActiveStreamingSession<S, I>
where
    I: TextInjector,
{
    session: S,
    partial_task: JoinHandle<PartialTextSession<I>>,
}

struct PartialTextSession<I>
where
    I: TextInjector,
{
    text_session: SpeculativeTextSession<I>,
    latest_partial: Option<String>,
}

impl<L, I> StreamingDictationController<L, I>
where
    L: LiveTranscriber,
    I: TextInjector + Clone + Send + 'static,
{
    pub fn new(transcriber: L, injector: I) -> Self {
        Self {
            transcriber,
            injector,
            transcript_notifier: NoopTranscriptNotifier,
            error_notifier: NoopErrorNotifier,
            listening_marker: None,
            inline_partials: true,
            active: None,
        }
    }
}

impl<L, I, V, N> StreamingDictationController<L, I, V, N>
where
    L: LiveTranscriber,
    I: TextInjector + Clone + Send + 'static,
    V: TranscriptNotifier,
    N: ErrorNotifier + Clone + Send + 'static,
{
    pub fn new_with_notifiers(
        transcriber: L,
        injector: I,
        transcript_notifier: V,
        error_notifier: N,
    ) -> Self {
        Self {
            transcriber,
            injector,
            transcript_notifier,
            error_notifier,
            listening_marker: None,
            inline_partials: true,
            active: None,
        }
    }

    pub fn with_listening_marker(mut self, marker: Option<String>) -> Self {
        self.listening_marker =
            marker.and_then(|marker| normalize_transcript_for_injection(&marker));
        self
    }

    pub fn with_inline_partials(mut self, inline_partials: bool) -> Self {
        self.inline_partials = inline_partials;
        self
    }

    pub fn is_recording(&self) -> bool {
        self.active.is_some()
    }
}

#[async_trait]
impl<L, I, V, N> HotkeyHandler for StreamingDictationController<L, I, V, N>
where
    L: LiveTranscriber,
    I: TextInjector + Clone + Send + 'static,
    V: TranscriptNotifier,
    N: ErrorNotifier + Clone + Send + 'static,
{
    async fn handle_hotkey(&mut self, command: IpcCommand) -> anyhow::Result<DaemonResponse> {
        match command {
            IpcCommand::HotkeyDown => self.start_streaming().await,
            IpcCommand::HotkeyUp => self.stop_streaming_and_inject().await,
        }
    }
}

impl<L, I, V, N> StreamingDictationController<L, I, V, N>
where
    L: LiveTranscriber,
    I: TextInjector + Clone + Send + 'static,
    V: TranscriptNotifier,
    N: ErrorNotifier + Clone + Send + 'static,
{
    async fn start_streaming(&mut self) -> anyhow::Result<DaemonResponse> {
        if self.active.is_some() {
            return Ok(DaemonResponse::AlreadyRecording);
        }

        let mut text_session = match SpeculativeTextSession::start(self.injector.clone()) {
            Ok(session) => session,
            Err(error) => {
                self.notify_failure("Dictation target capture failed", &error);
                return Err(error);
            }
        };
        let live_session = match self.transcriber.start().await {
            Ok(session) => session,
            Err(error) => {
                self.notify_failure("Streaming start failed", &error);
                return Err(error);
            }
        };
        if let Some(marker) = self.listening_marker.as_deref() {
            if let Err(error) = text_session.replace_text(marker) {
                let _ = self.transcriber.stop(live_session.session).await;
                self.notify_failure("Listening marker injection failed", &error);
                return Err(error);
            }
        }
        self.notify_transcript(|notifier| notifier.notify_listening());

        let partial_task = tokio::spawn(consume_live_updates(
            live_session.updates,
            self.error_notifier.clone(),
            text_session,
            self.inline_partials,
        ));
        self.active = Some(ActiveStreamingSession {
            session: live_session.session,
            partial_task,
        });

        Ok(DaemonResponse::Started)
    }

    async fn stop_streaming_and_inject(&mut self) -> anyhow::Result<DaemonResponse> {
        let Some(active) = self.active.take() else {
            return Ok(DaemonResponse::AlreadyIdle);
        };

        let final_transcript = match self.transcriber.stop(active.session).await {
            Ok(transcript) => transcript,
            Err(error) => {
                let mut partial_state = match finish_partial_task(active.partial_task).await {
                    Ok(partial_state) => partial_state,
                    Err(partial_error) => {
                        self.notify_failure("Partial text replacement failed", &partial_error);
                        return Err(partial_error);
                    }
                };
                if let Some(partial) = partial_state.latest_partial.as_deref() {
                    if let Err(replace_error) = partial_state.text_session.replace_text(partial) {
                        self.notify_failure("Partial fallback replacement failed", &replace_error);
                    }
                } else if !partial_state.text_session.inserted_text().is_empty() {
                    if let Err(cleanup_error) = partial_state.text_session.replace_text("") {
                        self.notify_failure("Speculative text cleanup failed", &cleanup_error);
                    }
                }
                self.notify_failure("Streaming transcription failed", &error);
                return Err(error);
            }
        };
        let mut partial_state = match finish_partial_task(active.partial_task).await {
            Ok(partial_state) => partial_state,
            Err(error) => {
                self.notify_failure("Partial text replacement failed", &error);
                return Err(error);
            }
        };

        match normalize_transcript_for_injection(&final_transcript) {
            Some(text) => {
                if let Err(error) = partial_state.text_session.replace_text(&text) {
                    self.notify_failure("Final text replacement failed", &error);
                    return Err(error);
                }
                self.notify_transcript(|notifier| notifier.notify_final(&text));
            }
            None => {
                if let Some(partial) = partial_state.latest_partial.as_deref() {
                    if let Err(error) = partial_state.text_session.replace_text(partial) {
                        self.notify_failure("Partial fallback replacement failed", &error);
                        return Err(error);
                    }
                } else if !partial_state.text_session.inserted_text().is_empty() {
                    if let Err(error) = partial_state.text_session.replace_text("") {
                        self.notify_failure("Speculative text cleanup failed", &error);
                        return Err(error);
                    }
                }
            }
        }

        Ok(DaemonResponse::Stopped)
    }

    fn notify_failure(&self, stage: &str, error: &anyhow::Error) {
        notify_failure(&self.error_notifier, stage, error);
    }

    fn notify_transcript(&self, notify: impl FnOnce(&V) -> anyhow::Result<()>) {
        if let Err(error) = notify(&self.transcript_notifier) {
            eprintln!("trec transcript notification failed: {error:#}");
        }
    }
}

async fn consume_live_updates<I, N>(
    mut updates: mpsc::Receiver<LiveTranscriptUpdate>,
    error_notifier: N,
    mut text_session: SpeculativeTextSession<I>,
    inline_partials: bool,
) -> PartialTextSession<I>
where
    I: TextInjector,
    N: ErrorNotifier,
{
    let mut latest_partial = None;
    while let Some(update) = updates.recv().await {
        let Some(transcript) = normalize_transcript_for_injection(&update.transcript) else {
            continue;
        };
        latest_partial = Some(transcript.clone());
        if inline_partials {
            match text_session.replace_text(&transcript) {
                Ok(_) => {}
                Err(error) => {
                    notify_failure(&error_notifier, "Partial text replacement failed", &error);
                    break;
                }
            }
        }
    }
    PartialTextSession {
        text_session,
        latest_partial,
    }
}

async fn finish_partial_task<I>(
    partial_task: JoinHandle<PartialTextSession<I>>,
) -> anyhow::Result<PartialTextSession<I>>
where
    I: TextInjector,
{
    partial_task
        .await
        .map_err(|error| anyhow::anyhow!("partial transcript task failed: {error:#}"))
}

fn notify_failure<N>(notifier: &N, stage: &str, error: &anyhow::Error)
where
    N: ErrorNotifier,
{
    let body = dictation_error_body(stage, error);
    if let Err(notify_error) = notifier.notify_error(DICTATION_ERROR_SUMMARY, &body) {
        eprintln!("trec notification failed: {notify_error:#}");
    }
}

#[derive(Debug, Clone)]
pub struct RollingHttpTranscriber {
    base_url: String,
    options: TranscribeOptions,
    partial_interval: Duration,
    partial_min_duration: Duration,
    sample_rate: u32,
}

impl RollingHttpTranscriber {
    pub fn new(base_url: String, options: TranscribeOptions) -> Self {
        Self {
            base_url,
            options,
            partial_interval: Duration::from_millis(1_250),
            partial_min_duration: Duration::ZERO,
            sample_rate: STT_SAMPLE_RATE,
        }
    }

    pub fn with_partial_interval(mut self, interval: Duration) -> Self {
        self.partial_interval = interval.max(Duration::from_millis(1));
        self
    }

    pub fn with_partial_min_duration(mut self, duration: Duration) -> Self {
        self.partial_min_duration = duration;
        self
    }
}

pub struct RollingHttpSession {
    capture: crate::audio::StreamingPcmCapture,
    stop_partials: watch::Sender<bool>,
    partial_task: JoinHandle<()>,
}

#[async_trait]
impl LiveTranscriber for RollingHttpTranscriber {
    type Session = RollingHttpSession;

    async fn start(&self) -> anyhow::Result<LiveTranscriptionSession<Self::Session>> {
        let capture = start_streaming_pcm_capture(self.sample_rate).await?;
        let shared_pcm = capture.shared_pcm();
        let (updates_tx, updates_rx) = mpsc::channel(8);
        let (stop_tx, stop_rx) = watch::channel(false);
        let partial_task = tokio::spawn(run_partial_transcriptions(PartialTranscriptionLoop {
            shared_pcm,
            stop_rx,
            updates_tx,
            base_url: self.base_url.clone(),
            options: self.options.clone(),
            sample_rate: self.sample_rate,
            interval: self.partial_interval,
            min_duration: self.partial_min_duration,
        }));

        Ok(LiveTranscriptionSession {
            session: RollingHttpSession {
                capture,
                stop_partials: stop_tx,
                partial_task,
            },
            updates: updates_rx,
        })
    }

    async fn stop(&self, session: Self::Session) -> anyhow::Result<String> {
        let _ = session.stop_partials.send(true);
        drop(session.partial_task);

        let pcm = stop_streaming_pcm_capture(session.capture).await?;
        transcribe_pcm_snapshot(
            &self.base_url,
            &self.options,
            self.sample_rate,
            &pcm,
            "final",
        )
        .await
    }
}

struct PartialTranscriptionLoop {
    shared_pcm: SharedPcmBuffer,
    stop_rx: watch::Receiver<bool>,
    updates_tx: mpsc::Sender<LiveTranscriptUpdate>,
    base_url: String,
    options: TranscribeOptions,
    sample_rate: u32,
    interval: Duration,
    min_duration: Duration,
}

async fn run_partial_transcriptions(loop_config: PartialTranscriptionLoop) {
    let PartialTranscriptionLoop {
        shared_pcm,
        mut stop_rx,
        updates_tx,
        base_url,
        options,
        sample_rate,
        interval,
        min_duration,
    } = loop_config;
    let min_bytes = pcm_bytes_for_duration(sample_rate, min_duration);
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut stabilizer = PartialTranscriptStabilizer::new(min_duration);

    loop {
        tokio::select! {
            changed = stop_rx.changed() => {
                if changed.is_err() || *stop_rx.borrow() {
                    break;
                }
            }
            _ = ticker.tick() => {
                let pcm = shared_pcm.lock().await.clone();
                if pcm.is_empty() || pcm.len() < min_bytes {
                    continue;
                }
                match transcribe_pcm_snapshot_until_stop(
                    &base_url,
                    &options,
                    sample_rate,
                    &pcm,
                    "partial",
                    &mut stop_rx,
                ).await {
                    Ok(Some(transcript)) => {
                        let Some(transcript) = normalize_transcript_for_injection(&transcript) else {
                            continue;
                        };
                        let audio_duration = pcm_duration(sample_rate, pcm.len());
                        let Some(transcript) = stabilizer.observe(audio_duration, &transcript) else {
                            continue;
                        };
                        if updates_tx.send(LiveTranscriptUpdate { transcript }).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => eprintln!("trec partial transcription failed: {error:#}"),
                }
            }
        }
    }
}

async fn transcribe_pcm_snapshot_until_stop(
    base_url: &str,
    options: &TranscribeOptions,
    sample_rate: u32,
    pcm: &[u8],
    label: &str,
    stop_rx: &mut watch::Receiver<bool>,
) -> anyhow::Result<Option<String>> {
    let path = temp_audio_path(label);
    write_pcm_wav(&path, pcm, sample_rate).await?;
    let snapshot_result = {
        let transcribe = transcribe_file(base_url, &path, options);
        tokio::pin!(transcribe);
        tokio::select! {
            result = &mut transcribe => Some(result),
            changed = stop_rx.changed() => {
                let _ = changed;
                None
            }
        }
    };
    let cleanup_result = tokio::fs::remove_file(&path)
        .await
        .with_context(|| format!("failed to remove {}", path.display()));

    match (snapshot_result, cleanup_result) {
        (None, Ok(())) => Ok(None),
        (None, Err(error)) => {
            eprintln!("{error:#}");
            Ok(None)
        }
        (Some(Ok(transcript)), Ok(())) => Ok(Some(transcript)),
        (Some(Ok(transcript)), Err(error)) => {
            eprintln!("{error:#}");
            Ok(Some(transcript))
        }
        (Some(Err(error)), Ok(())) => Err(error),
        (Some(Err(error)), Err(cleanup_error)) => {
            eprintln!("{cleanup_error:#}");
            Err(error)
        }
    }
}

async fn transcribe_pcm_snapshot(
    base_url: &str,
    options: &TranscribeOptions,
    sample_rate: u32,
    pcm: &[u8],
    label: &str,
) -> anyhow::Result<String> {
    let path = temp_audio_path(label);
    write_pcm_wav(&path, pcm, sample_rate).await?;
    let result = transcribe_file(base_url, &path, options).await;
    let cleanup_result = tokio::fs::remove_file(&path)
        .await
        .with_context(|| format!("failed to remove {}", path.display()));

    match (result, cleanup_result) {
        (Ok(transcript), Ok(())) => Ok(transcript),
        (Ok(transcript), Err(error)) => {
            eprintln!("{error:#}");
            Ok(transcript)
        }
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup_error)) => {
            eprintln!("{cleanup_error:#}");
            Err(error)
        }
    }
}

#[derive(Debug, Clone)]
struct PartialTranscriptStabilizer {
    previous_candidate: Option<String>,
    last_emitted: String,
    min_duration: Duration,
    min_chars: usize,
}

impl PartialTranscriptStabilizer {
    fn new(min_duration: Duration) -> Self {
        Self {
            previous_candidate: None,
            last_emitted: String::new(),
            min_duration,
            min_chars: 4,
        }
    }

    fn observe(&mut self, audio_duration: Duration, candidate: &str) -> Option<String> {
        if audio_duration < self.min_duration {
            return None;
        }
        let candidate = candidate.trim();
        if candidate.chars().count() < self.min_chars {
            return None;
        }

        let stable = self
            .previous_candidate
            .as_deref()
            .and_then(|previous| common_word_prefix(previous, candidate));
        self.previous_candidate = Some(candidate.to_string());

        let stable = stable?;
        if stable.chars().count() < self.min_chars || stable == self.last_emitted {
            return None;
        }
        self.last_emitted.clone_from(&stable);
        Some(stable)
    }
}

fn common_word_prefix(previous: &str, candidate: &str) -> Option<String> {
    let previous_words = word_spans(previous);
    let candidate_words = word_spans(candidate);
    let mut stable_end = 0usize;

    for ((previous_start, previous_end), (candidate_start, candidate_end)) in
        previous_words.into_iter().zip(candidate_words)
    {
        let previous_word = &previous[previous_start..previous_end];
        let candidate_word = &candidate[candidate_start..candidate_end];
        if !previous_word.eq_ignore_ascii_case(candidate_word) {
            break;
        }
        stable_end = candidate_end;
    }

    if stable_end == 0 {
        None
    } else {
        Some(candidate[..stable_end].trim_end().to_string())
    }
}

fn word_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = None;

    for (index, ch) in text.char_indices() {
        if ch.is_whitespace() {
            if let Some(word_start) = start.take() {
                spans.push((word_start, index));
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }

    if let Some(word_start) = start {
        spans.push((word_start, text.len()));
    }

    spans
}

fn pcm_bytes_for_duration(sample_rate: u32, duration: Duration) -> usize {
    let samples = duration.as_secs_f64() * f64::from(sample_rate);
    samples.ceil() as usize * 2
}

fn pcm_duration(sample_rate: u32, byte_len: usize) -> Duration {
    let samples = byte_len / 2;
    Duration::from_secs_f64(samples as f64 / f64::from(sample_rate))
}

fn temp_audio_path(label: &str) -> PathBuf {
    let counter = TEMP_AUDIO_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("trec-{label}-{}-{counter}.wav", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partials_wait_for_minimum_audio_duration() {
        let mut stabilizer = PartialTranscriptStabilizer::new(Duration::from_millis(2_500));

        assert_eq!(
            stabilizer.observe(Duration::from_millis(2_499), "hello world"),
            None
        );
        assert_eq!(
            stabilizer.observe(Duration::from_millis(2_500), "hello world"),
            None
        );
        assert_eq!(
            stabilizer.observe(Duration::from_millis(3_750), "hello world again"),
            Some("hello world".to_string())
        );
    }

    #[test]
    fn partials_can_emit_without_minimum_audio_duration() {
        let mut stabilizer = PartialTranscriptStabilizer::new(Duration::ZERO);

        assert_eq!(
            stabilizer.observe(Duration::from_millis(100), "hello world"),
            None
        );
        assert_eq!(
            stabilizer.observe(Duration::from_millis(200), "hello world again"),
            Some("hello world".to_string())
        );
    }

    #[test]
    fn partials_emit_only_word_prefixes_seen_twice() {
        let mut stabilizer = PartialTranscriptStabilizer::new(Duration::from_millis(0));

        assert_eq!(
            stabilizer.observe(Duration::from_secs(3), "yellow word"),
            None
        );
        assert_eq!(
            stabilizer.observe(Duration::from_secs(4), "hello world"),
            None
        );
        assert_eq!(
            stabilizer.observe(Duration::from_secs(5), "hello world today"),
            Some("hello world".to_string())
        );
    }

    #[test]
    fn partials_do_not_repeat_the_same_stable_prefix() {
        let mut stabilizer = PartialTranscriptStabilizer::new(Duration::from_millis(0));

        assert_eq!(
            stabilizer.observe(Duration::from_secs(3), "hello world"),
            None
        );
        assert_eq!(
            stabilizer.observe(Duration::from_secs(4), "hello world today"),
            Some("hello world".to_string())
        );
        assert_eq!(
            stabilizer.observe(Duration::from_secs(5), "hello world tomorrow"),
            None
        );
    }
}
