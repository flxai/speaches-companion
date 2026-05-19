use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task::JoinHandle;
use tokio::time::Instant;

#[cfg(feature = "debug-recordings")]
use crate::audio::write_pcm_mp3;
use crate::audio::{
    pcm_bytes_for_duration, pcm_duration, start_streaming_pcm_capture, write_pcm_wav,
    SharedPcmBuffer, StreamingPcmCapture, StreamingPcmSession, STT_SAMPLE_RATE,
};
use crate::daemon::{DaemonResponse, HotkeyHandler};
use crate::inject::{
    format_transcript_for_injection, normalize_transcript_for_injection, SpeculativeTextSession,
    TextInjector,
};
use crate::ipc::IpcCommand;
use crate::notification::{
    dictation_error_body, ErrorNotifier, NoopErrorNotifier, NoopTranscriptNotifier,
    TranscriptNotifier, DICTATION_ERROR_SUMMARY,
};
use crate::speech::{
    adaptive_speech_threshold, rms_frames, DEFAULT_MIN_SPEECH_DURATION,
    DEFAULT_SPEECH_ANALYSIS_FRAME,
};
use crate::stt::{transcribe_file, TranscribeOptions};

static TEMP_AUDIO_COUNTER: AtomicU64 = AtomicU64::new(0);
const TRANSCRIBER_WARMUP_DURATION: Duration = Duration::from_millis(500);
const TRIM_LEADING_PAD: Duration = Duration::from_millis(500);
const TRIM_TRAILING_PAD: Duration = Duration::from_millis(300);
pub const DEFAULT_PARTIAL_CHUNK_DELAY: Duration = Duration::from_millis(80);
pub const DEFAULT_PARTIAL_CHUNK_MAX_DELAY: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveTranscriptUpdate {
    pub transcript: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartialChunkingConfig {
    pub enabled: bool,
    pub delay: Duration,
    pub max_delay: Duration,
}

impl Default for PartialChunkingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            delay: DEFAULT_PARTIAL_CHUNK_DELAY,
            max_delay: DEFAULT_PARTIAL_CHUNK_MAX_DELAY,
        }
    }
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
    partial_chunking: PartialChunkingConfig,
    final_transcript: bool,
    append_space: bool,
    active: Option<ActiveStreamingSession<L::Session, I>>,
}

struct ActiveStreamingSession<S, I>
where
    I: TextInjector,
{
    session: S,
    partial_command_tx: mpsc::Sender<PartialTextCommand>,
    partial_task: JoinHandle<PartialTextSession<I>>,
}

struct PartialTextSession<I>
where
    I: TextInjector,
{
    text_session: SpeculativeTextSession<I>,
    latest_partial: Option<String>,
}

enum PartialTextCommand {
    FlushPending {
        ack: oneshot::Sender<anyhow::Result<()>>,
    },
}

#[derive(Debug)]
struct PartialDisplayState {
    latest_partial: Option<String>,
    displayed_partial: Option<String>,
    last_announced_partial: Option<String>,
    dirty_started_at: Option<Instant>,
    last_update_at: Option<Instant>,
    last_flush_at: Option<Instant>,
}

impl PartialDisplayState {
    fn new() -> Self {
        Self {
            latest_partial: None,
            displayed_partial: None,
            last_announced_partial: None,
            dirty_started_at: None,
            last_update_at: None,
            last_flush_at: None,
        }
    }

    fn update_latest_partial(&mut self, transcript: String) -> bool {
        if self.latest_partial.as_deref() == Some(transcript.as_str()) {
            return false;
        }
        self.latest_partial = Some(transcript);
        true
    }

    fn note_pending_flush(&mut self, now: Instant) {
        self.dirty_started_at.get_or_insert(now);
        self.last_update_at = Some(now);
    }

    fn flush_deadline(&self, config: PartialChunkingConfig) -> Option<Instant> {
        let dirty_started_at = self.dirty_started_at?;
        let last_update_at = self.last_update_at.unwrap_or(dirty_started_at);
        let max_anchor = self.last_flush_at.unwrap_or(dirty_started_at);
        Some(std::cmp::min(
            last_update_at + config.delay,
            max_anchor + config.max_delay,
        ))
    }

    fn has_pending_partial(&self) -> bool {
        self.latest_partial != self.displayed_partial
    }

    fn announce_partial<V>(&mut self, transcript_notifier: &V, transcript: &str)
    where
        V: TranscriptNotifier,
    {
        if self.last_announced_partial.as_deref() == Some(transcript) {
            return;
        }
        notify_transcript(transcript_notifier, |notifier| {
            notifier.notify_partial(transcript)
        });
        self.last_announced_partial = Some(transcript.to_string());
    }

    fn flush_display<I, V>(
        &mut self,
        text_session: &mut SpeculativeTextSession<I>,
        transcript_notifier: &V,
        now: Instant,
    ) -> anyhow::Result<()>
    where
        I: TextInjector,
        V: TranscriptNotifier,
    {
        let display_text = self.latest_partial.as_deref().unwrap_or("");
        text_session.replace_text(display_text)?;
        self.displayed_partial = self.latest_partial.clone();
        self.dirty_started_at = None;
        self.last_update_at = None;
        self.last_flush_at = Some(now);

        if let Some(partial) = self.latest_partial.clone() {
            self.announce_partial(transcript_notifier, &partial);
        }

        Ok(())
    }
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
            partial_chunking: PartialChunkingConfig::default(),
            final_transcript: true,
            append_space: true,
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
            partial_chunking: PartialChunkingConfig::default(),
            final_transcript: true,
            append_space: true,
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

    pub fn with_partial_chunking_config(mut self, partial_chunking: PartialChunkingConfig) -> Self {
        self.partial_chunking = partial_chunking;
        self
    }

    pub fn with_final_transcript(mut self, final_transcript: bool) -> Self {
        self.final_transcript = final_transcript;
        self
    }

    pub fn with_append_space(mut self, append_space: bool) -> Self {
        self.append_space = append_space;
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

        let live_session = match self.transcriber.start().await {
            Ok(session) => session,
            Err(error) => {
                self.notify_failure("Streaming start failed", &error);
                return Err(error);
            }
        };
        let mut text_session = match SpeculativeTextSession::start(self.injector.clone()) {
            Ok(session) => session,
            Err(error) => {
                let _ = self.transcriber.stop(live_session.session).await;
                self.notify_failure("Dictation target capture failed", &error);
                return Err(error);
            }
        };
        if let Some(marker) = self.listening_marker.as_deref() {
            if let Err(error) = text_session.show_trailing_marker(marker) {
                let _ = self.transcriber.stop(live_session.session).await;
                self.notify_failure("Listening marker injection failed", &error);
                return Err(error);
            }
        }
        self.notify_transcript(|notifier| notifier.notify_listening());

        let (partial_command_tx, partial_command_rx) = mpsc::channel(4);
        let partial_task = tokio::spawn(consume_live_updates(
            live_session.updates,
            partial_command_rx,
            self.transcript_notifier.clone(),
            self.error_notifier.clone(),
            text_session,
            self.inline_partials,
            self.partial_chunking,
        ));
        self.active = Some(ActiveStreamingSession {
            session: live_session.session,
            partial_command_tx,
            partial_task,
        });

        Ok(DaemonResponse::Started)
    }

    async fn stop_streaming_and_inject(&mut self) -> anyhow::Result<DaemonResponse> {
        let Some(active) = self.active.take() else {
            return Ok(DaemonResponse::AlreadyIdle);
        };
        let ActiveStreamingSession {
            session,
            partial_command_tx,
            partial_task,
        } = active;

        if let Err(error) = flush_pending_partial(&partial_command_tx).await {
            if self.final_transcript {
                self.notify_failure("Pending partial flush failed", &error);
            }
        }

        let stop_transcript = match self.transcriber.stop(session).await {
            Ok(transcript) => transcript,
            Err(error) => {
                drop(partial_command_tx);
                let mut partial_state = match finish_partial_task(partial_task).await {
                    Ok(partial_state) => partial_state,
                    Err(partial_error) => {
                        self.notify_failure("Partial text replacement failed", &partial_error);
                        return Err(partial_error);
                    }
                };
                if let Err(cleanup_error) = partial_state.text_session.hide_trailing_marker() {
                    self.notify_failure("Listening marker cleanup failed", &cleanup_error);
                }
                if let Some(partial) = partial_state.latest_partial.as_deref() {
                    if let Some(text) = format_transcript_for_injection(partial, self.append_space)
                    {
                        if let Err(replace_error) = partial_state.text_session.replace_text(&text) {
                            self.notify_failure(
                                "Partial fallback replacement failed",
                                &replace_error,
                            );
                        }
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
        drop(partial_command_tx);
        let mut partial_state = match finish_partial_task(partial_task).await {
            Ok(partial_state) => partial_state,
            Err(error) => {
                self.notify_failure("Partial text replacement failed", &error);
                return Err(error);
            }
        };
        if let Err(error) = partial_state.text_session.hide_trailing_marker() {
            self.notify_failure("Listening marker cleanup failed", &error);
            return Err(error);
        }

        let final_transcript = if self.final_transcript {
            normalize_transcript_for_injection(&stop_transcript)
        } else {
            None
        };

        match final_transcript {
            Some(transcript) => {
                let transcript =
                    choose_final_transcript(transcript, partial_state.latest_partial.as_deref());
                let text = format_transcript_for_injection(&transcript, self.append_space)
                    .expect("normalized transcript should format for injection");
                if let Err(error) = partial_state.text_session.replace_text(&text) {
                    self.notify_failure("Final text replacement failed", &error);
                    return Err(error);
                }
                self.notify_transcript(|notifier| notifier.notify_final(&transcript));
            }
            None => {
                if let Some(partial) = partial_state.latest_partial.as_deref() {
                    if let Some(text) = format_transcript_for_injection(partial, self.append_space)
                    {
                        if let Err(error) = partial_state.text_session.replace_text(&text) {
                            self.notify_failure("Partial fallback replacement failed", &error);
                            return Err(error);
                        }
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
        notify_transcript(&self.transcript_notifier, notify);
    }
}

async fn consume_live_updates<I, V, N>(
    mut updates: mpsc::Receiver<LiveTranscriptUpdate>,
    mut commands: mpsc::Receiver<PartialTextCommand>,
    transcript_notifier: V,
    error_notifier: N,
    mut text_session: SpeculativeTextSession<I>,
    inline_partials: bool,
    partial_chunking: PartialChunkingConfig,
) -> PartialTextSession<I>
where
    I: TextInjector,
    V: TranscriptNotifier,
    N: ErrorNotifier,
{
    let mut display_state = PartialDisplayState::new();
    let mut updates_open = true;
    let mut commands_open = true;
    let mut flush_timer = Box::pin(tokio::time::sleep_until(
        Instant::now() + Duration::from_secs(24 * 60 * 60),
    ));
    while updates_open || commands_open {
        let next_flush_deadline = display_state.flush_deadline(partial_chunking);
        if let Some(deadline) = next_flush_deadline {
            flush_timer.as_mut().reset(deadline);
        }
        tokio::select! {
            update = updates.recv(), if updates_open => {
                let Some(update) = update else {
                    updates_open = false;
                    continue;
                };
                let ends_at_boundary = ends_at_stable_boundary(&update.transcript);
                let transcript = normalize_transcript_for_injection(&update.transcript);
                let Some(transcript) = transcript else {
                    continue;
                };
                if !display_state.update_latest_partial(transcript.clone()) {
                    continue;
                }
                if !inline_partials {
                    display_state.announce_partial(&transcript_notifier, &transcript);
                    continue;
                }

                let should_flush_now = !partial_chunking.enabled || ends_at_boundary;
                if should_flush_now {
                    if let Err(error) = display_state.flush_display(
                        &mut text_session,
                        &transcript_notifier,
                        Instant::now(),
                    ) {
                        notify_failure(&error_notifier, "Partial text replacement failed", &error);
                        break;
                    }
                } else {
                    display_state.note_pending_flush(Instant::now());
                }
            }
            _ = &mut flush_timer, if next_flush_deadline.is_some() => {
                if let Err(error) = display_state.flush_display(
                    &mut text_session,
                    &transcript_notifier,
                    Instant::now(),
                ) {
                    notify_failure(&error_notifier, "Partial text replacement failed", &error);
                    break;
                }
            }
            command = commands.recv(), if commands_open => {
                let Some(command) = command else {
                    commands_open = false;
                    continue;
                };
                match command {
                    PartialTextCommand::FlushPending { ack } => {
                        while updates_open {
                            match updates.try_recv() {
                                Ok(update) => {
                                    if let Some(transcript) =
                                        normalize_transcript_for_injection(&update.transcript)
                                    {
                                        display_state.update_latest_partial(transcript);
                                    }
                                }
                                Err(mpsc::error::TryRecvError::Empty) => break,
                                Err(mpsc::error::TryRecvError::Disconnected) => {
                                    updates_open = false;
                                    break;
                                }
                            }
                        }
                        let result = if display_state.has_pending_partial() {
                            display_state.flush_display(
                                &mut text_session,
                                &transcript_notifier,
                                Instant::now(),
                            )
                        } else {
                            Ok(())
                        };
                        let _ = ack.send(result);
                    }
                }
            }
        }
    }
    if display_state.has_pending_partial() {
        if let Err(error) =
            display_state.flush_display(&mut text_session, &transcript_notifier, Instant::now())
        {
            notify_failure(&error_notifier, "Partial text replacement failed", &error);
        }
    }
    PartialTextSession {
        text_session,
        latest_partial: display_state.latest_partial,
    }
}

async fn flush_pending_partial(
    command_tx: &mpsc::Sender<PartialTextCommand>,
) -> anyhow::Result<()> {
    let (ack_tx, ack_rx) = oneshot::channel();
    command_tx
        .send(PartialTextCommand::FlushPending { ack: ack_tx })
        .await
        .context("partial text task stopped before pending partial could be flushed")?;
    ack_rx
        .await
        .context("partial text task stopped before acknowledging pending partial flush")?
}

fn ends_at_stable_boundary(transcript: &str) -> bool {
    transcript.chars().last().is_some_and(|last_char| {
        last_char.is_whitespace()
            || matches!(
                last_char,
                '.' | ','
                    | '!'
                    | '?'
                    | ';'
                    | ':'
                    | ')'
                    | ']'
                    | '}'
                    | '"'
                    | '\''
                    | '»'
                    | '”'
                    | '’'
                    | '…'
            )
    })
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

fn notify_transcript<V>(notifier: &V, notify: impl FnOnce(&V) -> anyhow::Result<()>)
where
    V: TranscriptNotifier,
{
    if let Err(error) = notify(notifier) {
        eprintln!("speaches-companion transcript notification failed: {error:#}");
    }
}

fn notify_failure<N>(notifier: &N, stage: &str, error: &anyhow::Error)
where
    N: ErrorNotifier,
{
    let body = dictation_error_body(stage, error);
    eprintln!("speaches-companion dictation failed: {body}");
    if let Err(notify_error) = notifier.notify_error(DICTATION_ERROR_SUMMARY, &body) {
        eprintln!("speaches-companion notification failed: {notify_error:#}");
    }
}

fn choose_final_transcript(final_transcript: String, latest_partial: Option<&str>) -> String {
    let Some(partial) = latest_partial.and_then(normalize_transcript_for_injection) else {
        return final_transcript;
    };

    if final_transcript == partial {
        return final_transcript;
    }

    if final_transcript.chars().count() >= partial.chars().count() {
        return final_transcript;
    }

    if final_transcript_looks_truncated(&final_transcript, &partial) {
        log_final_transcript_decision(
            "partial",
            &final_transcript,
            Some(&partial),
            "final looks truncated",
        );
        return partial;
    }

    log_final_transcript_decision(
        "final",
        &final_transcript,
        Some(&partial),
        "shorter but plausible",
    );
    final_transcript
}

fn final_transcript_looks_truncated(final_transcript: &str, partial: &str) -> bool {
    let final_words = word_count(final_transcript);
    let partial_words = word_count(partial);
    if partial_words < 8 || final_words >= partial_words {
        return false;
    }

    let final_lower = final_transcript.to_lowercase();
    let partial_lower = partial.to_lowercase();
    if partial_lower.ends_with(final_lower.trim()) {
        return true;
    }

    partial_words >= 12 && final_words.saturating_mul(10) < partial_words.saturating_mul(7)
}

fn word_count(text: &str) -> usize {
    text.split_whitespace().count()
}

fn log_final_transcript_decision(
    choice: &str,
    final_transcript: &str,
    latest_partial: Option<&str>,
    reason: &str,
) {
    let final_chars = final_transcript.chars().count();
    let final_words = word_count(final_transcript);
    let (partial_chars, partial_words, partial_preview) = latest_partial
        .map(|partial| {
            (
                partial.chars().count(),
                word_count(partial),
                transcript_preview(partial),
            )
        })
        .unwrap_or((0, 0, String::new()));
    eprintln!(
        "speaches-companion final transcript decision: choice={choice} reason={reason}; final={final_chars} chars/{final_words} words \"{}\"; partial={partial_chars} chars/{partial_words} words \"{}\"",
        transcript_preview(final_transcript),
        partial_preview
    );
}

fn transcript_preview(text: &str) -> String {
    const PREVIEW_CHARS: usize = 96;
    let mut preview = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(PREVIEW_CHARS)
        .collect::<String>();
    if text.chars().count() > PREVIEW_CHARS {
        preview.push_str("...");
    }
    preview
}

#[derive(Clone)]
pub struct FinalHttpTranscriber {
    base_url: String,
    options: TranscribeOptions,
    capture: Arc<Mutex<Option<StreamingPcmCapture>>>,
    transcript_dir: Option<PathBuf>,
    #[cfg(feature = "debug-recordings")]
    record_dir: Option<PathBuf>,
    leading_silence: Duration,
    preroll: Duration,
    sample_rate: u32,
}

impl FinalHttpTranscriber {
    pub fn new(base_url: String, options: TranscribeOptions) -> Self {
        Self {
            base_url,
            options,
            capture: Arc::new(Mutex::new(None)),
            transcript_dir: None,
            #[cfg(feature = "debug-recordings")]
            record_dir: None,
            leading_silence: Duration::from_millis(250),
            preroll: Duration::from_millis(750),
            sample_rate: STT_SAMPLE_RATE,
        }
    }

    pub fn with_leading_silence(mut self, duration: Duration) -> Self {
        self.leading_silence = duration;
        self
    }

    pub fn with_preroll(mut self, duration: Duration) -> Self {
        self.preroll = duration;
        self
    }

    pub fn with_transcript_dir(mut self, transcript_dir: Option<PathBuf>) -> Self {
        self.transcript_dir = transcript_dir;
        self
    }

    #[cfg(feature = "debug-recordings")]
    pub fn with_record_dir(mut self, record_dir: Option<PathBuf>) -> Self {
        self.record_dir = record_dir;
        self
    }

    pub async fn prepare_capture(&self) -> anyhow::Result<()> {
        self.ensure_capture_started().await.map(|_| ())
    }

    pub async fn warm_up(&self) -> anyhow::Result<()> {
        self.prepare_capture().await?;
        self.warm_up_transcription().await
    }

    pub async fn warm_up_transcription(&self) -> anyhow::Result<()> {
        let mut options = self.options.clone();
        options.stream = false;
        let pcm = vec![0; pcm_bytes_for_duration(self.sample_rate, TRANSCRIBER_WARMUP_DURATION)];
        transcribe_pcm_snapshot(
            &self.base_url,
            &options,
            self.sample_rate,
            &pcm,
            "warmup",
            Duration::ZERO,
        )
        .await
        .map(|_| ())
    }

    async fn ensure_capture_started(&self) -> anyhow::Result<SharedPcmBuffer> {
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

pub struct FinalHttpSession {
    session_pcm: StreamingPcmSession,
}

#[async_trait]
impl LiveTranscriber for FinalHttpTranscriber {
    type Session = FinalHttpSession;

    async fn start(&self) -> anyhow::Result<LiveTranscriptionSession<Self::Session>> {
        let shared_pcm = self.ensure_capture_started().await?;
        let session_pcm =
            StreamingPcmSession::start(shared_pcm, self.sample_rate, self.preroll).await?;
        eprintln!(
            "speaches-companion hotkey-down audio buffer: {:.2}s available; retained {:.2}s pre-roll (requested {}ms)",
            pcm_duration(self.sample_rate, session_pcm.available_at_start_bytes()).as_secs_f64(),
            pcm_duration(self.sample_rate, session_pcm.retained_preroll_bytes()).as_secs_f64(),
            self.preroll.as_millis()
        );
        let (_updates_tx, updates_rx) = mpsc::channel(1);

        Ok(LiveTranscriptionSession {
            session: FinalHttpSession { session_pcm },
            updates: updates_rx,
        })
    }

    async fn stop(&self, session: Self::Session) -> anyhow::Result<String> {
        let raw_pcm = session.session_pcm.snapshot().await;
        session.session_pcm.finish().await;
        if raw_pcm.is_empty() {
            anyhow::bail!("recording stopped before any audio was captured");
        }
        #[cfg(feature = "debug-recordings")]
        if let Some(record_dir) = self.record_dir.as_deref() {
            match preserve_recording_snapshot(record_dir, &raw_pcm, self.sample_rate, "final").await
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
        let final_audio = build_final_transcription_audio(self.sample_rate, &raw_pcm);
        eprintln!(
            "speaches-companion final audio snapshot: {:.2}s raw including up to {}ms debug pre-roll",
            pcm_duration(self.sample_rate, raw_pcm.len()).as_secs_f64(),
            self.preroll.as_millis()
        );
        eprintln!(
            "speaches-companion final audio sent: {:.2}s after trimming {:.2}s leading / {:.2}s trailing ({:.2}s detected trailing silence)",
            final_audio.audio_duration.as_secs_f64(),
            final_audio.leading_trim.as_secs_f64(),
            final_audio.trailing_trim.as_secs_f64(),
            final_audio.trailing_silence.as_secs_f64()
        );
        if final_audio.pcm.is_empty() {
            eprintln!(
                "speaches-companion final audio rejected as noise-only; skipping final transcription"
            );
            return Ok(String::new());
        }
        let transcript = transcribe_pcm_snapshot(
            &self.base_url,
            &self.options,
            self.sample_rate,
            &final_audio.pcm,
            "final-trimmed",
            self.leading_silence,
        )
        .await?;
        if let Some(transcript_dir) = self.transcript_dir.as_deref() {
            match preserve_transcript_snapshot(transcript_dir, &transcript, "final").await {
                Ok(path) => eprintln!(
                    "speaches-companion preserved final transcript at {}",
                    path.display()
                ),
                Err(error) => {
                    eprintln!("speaches-companion failed to preserve final transcript: {error:#}")
                }
            }
        }
        Ok(transcript)
    }
}

async fn transcribe_pcm_snapshot(
    base_url: &str,
    options: &TranscribeOptions,
    sample_rate: u32,
    pcm: &[u8],
    label: &str,
    leading_silence: Duration,
) -> anyhow::Result<String> {
    let path = temp_audio_path(label);
    let pcm = pcm_with_leading_silence(sample_rate, leading_silence, pcm);
    write_pcm_wav(&path, &pcm, sample_rate).await?;
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
struct TranscriptionAudio {
    pcm: Vec<u8>,
    audio_duration: Duration,
    leading_trim: Duration,
    trailing_trim: Duration,
    trailing_silence: Duration,
}

#[derive(Debug, Clone)]
struct SpeechTrim {
    pcm: Vec<u8>,
    leading_trim: Duration,
    trailing_trim: Duration,
    trailing_silence: Duration,
}

fn build_final_transcription_audio(sample_rate: u32, pcm: &[u8]) -> TranscriptionAudio {
    let trim = trim_pcm_to_speech(sample_rate, pcm);
    TranscriptionAudio {
        audio_duration: pcm_duration(sample_rate, trim.pcm.len()),
        pcm: trim.pcm,
        leading_trim: trim.leading_trim,
        trailing_trim: trim.trailing_trim,
        trailing_silence: trim.trailing_silence,
    }
}

fn trim_pcm_to_speech(sample_rate: u32, pcm: &[u8]) -> SpeechTrim {
    let frames = rms_frames(sample_rate, pcm, DEFAULT_SPEECH_ANALYSIS_FRAME);
    if frames.is_empty() {
        return SpeechTrim {
            pcm: Vec::new(),
            leading_trim: Duration::ZERO,
            trailing_trim: Duration::ZERO,
            trailing_silence: Duration::ZERO,
        };
    }

    let threshold = adaptive_speech_threshold(sample_rate, &frames);
    let Some(first_speech) = frames.iter().position(|frame| frame.rms >= threshold) else {
        return SpeechTrim {
            pcm: Vec::new(),
            leading_trim: Duration::ZERO,
            trailing_trim: Duration::ZERO,
            trailing_silence: pcm_duration(sample_rate, pcm.len()),
        };
    };
    let last_speech = frames
        .iter()
        .rposition(|frame| frame.rms >= threshold)
        .expect("first speech frame exists");
    let speech_start = frames[first_speech].start;
    let speech_end = frames[last_speech].end;
    if speech_end.saturating_sub(speech_start)
        < pcm_bytes_for_duration(sample_rate, DEFAULT_MIN_SPEECH_DURATION)
    {
        return SpeechTrim {
            pcm: Vec::new(),
            leading_trim: Duration::ZERO,
            trailing_trim: Duration::ZERO,
            trailing_silence: pcm_duration(sample_rate, pcm.len().saturating_sub(speech_end)),
        };
    }

    let padded_start =
        speech_start.saturating_sub(pcm_bytes_for_duration(sample_rate, TRIM_LEADING_PAD));
    let padded_end = speech_end
        .saturating_add(pcm_bytes_for_duration(sample_rate, TRIM_TRAILING_PAD))
        .min(pcm.len());
    SpeechTrim {
        pcm: pcm[padded_start..padded_end].to_vec(),
        leading_trim: pcm_duration(sample_rate, padded_start),
        trailing_trim: pcm_duration(sample_rate, pcm.len().saturating_sub(padded_end)),
        trailing_silence: pcm_duration(sample_rate, pcm.len().saturating_sub(speech_end)),
    }
}

async fn preserve_transcript_snapshot(
    transcript_dir: &Path,
    transcript: &str,
    label: &str,
) -> anyhow::Result<PathBuf> {
    tokio::fs::create_dir_all(transcript_dir)
        .await
        .with_context(|| format!("failed to create {}", transcript_dir.display()))?;
    let path = transcript_dir.join(transcript_file_name(label));
    tokio::fs::write(&path, transcript)
        .await
        .with_context(|| format!("failed to write transcript {}", path.display()))?;
    Ok(path)
}

#[cfg(feature = "debug-recordings")]
pub async fn preserve_recording_snapshot(
    record_dir: &Path,
    pcm: &[u8],
    sample_rate: u32,
    label: &str,
) -> anyhow::Result<PathBuf> {
    let path = record_dir.join(recording_file_name(label));
    write_pcm_mp3(&path, pcm, sample_rate).await?;
    Ok(path)
}

fn transcript_file_name(label: &str) -> String {
    let counter = TEMP_AUDIO_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "speaches-companion-{label}-{}-{counter}.txt",
        std::process::id()
    )
}

#[cfg(feature = "debug-recordings")]
fn recording_file_name(label: &str) -> String {
    let counter = TEMP_AUDIO_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "speaches-companion-{label}-{}-{counter}.mp3",
        std::process::id()
    )
}

fn audio_file_name(label: &str) -> String {
    let counter = TEMP_AUDIO_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "speaches-companion-{label}-{}-{counter}.wav",
        std::process::id()
    )
}

fn pcm_with_leading_silence(sample_rate: u32, duration: Duration, pcm: &[u8]) -> Vec<u8> {
    let silence_bytes = pcm_bytes_for_duration(sample_rate, duration);
    let mut padded_pcm = vec![0; silence_bytes];
    padded_pcm.extend_from_slice(pcm);
    padded_pcm
}

fn temp_audio_path(label: &str) -> PathBuf {
    std::env::temp_dir().join(audio_file_name(label))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speech_trimming_removes_leading_and_trailing_silence() {
        let sample_rate = 1_000;
        let pcm = [
            pcm_for_duration(sample_rate, Duration::from_secs(1), 0),
            pcm_for_duration(sample_rate, Duration::from_millis(500), 2_000),
            pcm_for_duration(sample_rate, Duration::from_secs(1), 0),
        ]
        .concat();

        let trim = trim_pcm_to_speech(sample_rate, &pcm);

        assert_eq!(trim.leading_trim, Duration::from_millis(500));
        assert_eq!(trim.trailing_trim, Duration::from_millis(700));
        assert_eq!(
            pcm_duration(sample_rate, trim.pcm.len()),
            Duration::from_millis(1_300)
        );
    }

    #[test]
    fn speech_trimming_rejects_noise_when_no_speech_is_detected() {
        let sample_rate = 1_000;
        let pcm = pcm_for_duration(sample_rate, Duration::from_millis(400), 0);

        let trim = trim_pcm_to_speech(sample_rate, &pcm);

        assert!(trim.pcm.is_empty());
        assert_eq!(trim.trailing_silence, Duration::from_millis(400));
    }

    #[test]
    fn final_transcription_audio_rejects_noise_when_no_speech_is_detected() {
        let sample_rate = 1_000;
        let pcm = pcm_for_duration(sample_rate, Duration::from_millis(400), 0);

        let audio = build_final_transcription_audio(sample_rate, &pcm);

        assert!(audio.pcm.is_empty());
        assert_eq!(audio.audio_duration, Duration::ZERO);
    }

    #[test]
    fn speech_trimming_rejects_too_short_burst() {
        let sample_rate = 1_000;
        let pcm = [
            pcm_for_duration(sample_rate, Duration::from_millis(300), 0),
            pcm_for_duration(sample_rate, Duration::from_millis(40), 2_000),
            pcm_for_duration(sample_rate, Duration::from_millis(300), 0),
        ]
        .concat();

        let trim = trim_pcm_to_speech(sample_rate, &pcm);

        assert!(trim.pcm.is_empty());
        assert_eq!(trim.trailing_silence, Duration::from_millis(300));
    }

    #[tokio::test]
    async fn transcript_snapshot_writes_text_file() {
        let dir = tempfile::tempdir().unwrap();

        let path = preserve_transcript_snapshot(dir.path(), "hello window", "final")
            .await
            .unwrap();

        assert_eq!(path.extension().and_then(|ext| ext.to_str()), Some("txt"));
        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            "hello window"
        );
    }

    #[cfg(feature = "debug-recordings")]
    #[test]
    fn recording_snapshot_names_use_mp3_extension() {
        let name = recording_file_name("realtime");

        assert!(name.starts_with("speaches-companion-realtime-"));
        assert!(name.ends_with(".mp3"));
    }

    #[test]
    fn leading_silence_prepends_zeroed_pcm_samples() {
        assert_eq!(
            pcm_with_leading_silence(4, Duration::from_millis(250), &[1, 2, 3, 4]),
            vec![0, 0, 1, 2, 3, 4]
        );
        assert_eq!(
            pcm_with_leading_silence(4, Duration::ZERO, &[1, 2]),
            vec![1, 2]
        );
    }

    fn pcm_for_duration(sample_rate: u32, duration: Duration, amplitude: i16) -> Vec<u8> {
        let samples = (duration.as_secs_f64() * f64::from(sample_rate)).round() as usize;
        let mut pcm = Vec::with_capacity(samples * 2);
        for _ in 0..samples {
            pcm.extend_from_slice(&amplitude.to_le_bytes());
        }
        pcm
    }
}
