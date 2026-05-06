use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use tokio::sync::{mpsc, watch, Mutex};
use tokio::task::JoinHandle;

use crate::audio::{
    start_streaming_pcm_capture, write_pcm_wav, SharedPcmBuffer, StreamingPcmCapture,
    StreamingPcmSession, STT_SAMPLE_RATE,
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
const TRANSCRIBER_WARMUP_DURATION: Duration = Duration::from_millis(500);
const STT_PREROLL_LIMIT: Duration = Duration::from_millis(1_500);
const TRIM_LEADING_PAD: Duration = Duration::from_millis(150);
const TRIM_TRAILING_PAD: Duration = Duration::from_millis(300);
const TRIM_ANALYSIS_FRAME: Duration = Duration::from_millis(20);
const TRIM_MIN_SPEECH: Duration = Duration::from_millis(120);
const FIXED_SPEECH_RMS_FLOOR: f64 = 700.0;
const MAX_SPEECH_RMS_FLOOR: f64 = 1_500.0;
const NOISE_FLOOR_MULTIPLIER: f64 = 4.0;
const PARTIAL_MAX_AUDIO: Duration = Duration::from_secs(8);
const PARTIAL_CONTEXT: Duration = Duration::from_millis(500);
const SEGMENT_READY_SILENCE: Duration = Duration::from_millis(800);

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
            eprintln!("speaches-scribe transcript notification failed: {error:#}");
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
    eprintln!("speaches-scribe dictation failed: {body}");
    if let Err(notify_error) = notifier.notify_error(DICTATION_ERROR_SUMMARY, &body) {
        eprintln!("speaches-scribe notification failed: {notify_error:#}");
    }
}

#[derive(Clone)]
pub struct RollingHttpTranscriber {
    base_url: String,
    options: TranscribeOptions,
    capture: Arc<Mutex<Option<StreamingPcmCapture>>>,
    transcript_dir: Option<PathBuf>,
    partial_interval: Duration,
    partial_min_duration: Duration,
    leading_silence: Duration,
    preroll: Duration,
    sample_rate: u32,
}

impl RollingHttpTranscriber {
    pub fn new(base_url: String, options: TranscribeOptions) -> Self {
        Self {
            base_url,
            options,
            capture: Arc::new(Mutex::new(None)),
            transcript_dir: None,
            partial_interval: Duration::from_millis(1_250),
            partial_min_duration: Duration::ZERO,
            leading_silence: Duration::from_millis(250),
            preroll: Duration::from_millis(750),
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

pub struct RollingHttpSession {
    session_pcm: StreamingPcmSession,
    stop_partials: watch::Sender<bool>,
    partial_task: JoinHandle<()>,
}

#[async_trait]
impl LiveTranscriber for RollingHttpTranscriber {
    type Session = RollingHttpSession;

    async fn start(&self) -> anyhow::Result<LiveTranscriptionSession<Self::Session>> {
        let shared_pcm = self.ensure_capture_started().await?;
        let session_pcm =
            StreamingPcmSession::start(shared_pcm, self.sample_rate, self.preroll).await?;
        eprintln!(
            "speaches-scribe hotkey-down audio buffer: {:.2}s available; retained {:.2}s pre-roll (requested {}ms)",
            pcm_duration(self.sample_rate, session_pcm.available_at_start_bytes()).as_secs_f64(),
            pcm_duration(self.sample_rate, session_pcm.retained_preroll_bytes()).as_secs_f64(),
            self.preroll.as_millis()
        );
        let (updates_tx, updates_rx) = mpsc::channel(8);
        let (stop_tx, stop_rx) = watch::channel(false);
        let partial_task = tokio::spawn(run_partial_transcriptions(PartialTranscriptionLoop {
            session_pcm: session_pcm.clone(),
            stop_rx,
            updates_tx,
            base_url: self.base_url.clone(),
            options: self.options.clone(),
            sample_rate: self.sample_rate,
            interval: self.partial_interval,
            min_duration: self.partial_min_duration,
            leading_silence: self.leading_silence,
            transcript_dir: self.transcript_dir.clone(),
        }));

        Ok(LiveTranscriptionSession {
            session: RollingHttpSession {
                session_pcm,
                stop_partials: stop_tx,
                partial_task,
            },
            updates: updates_rx,
        })
    }

    async fn stop(&self, session: Self::Session) -> anyhow::Result<String> {
        let _ = session.stop_partials.send(true);
        drop(session.partial_task);

        let raw_pcm = session.session_pcm.snapshot().await;
        let stt_view_pcm = session
            .session_pcm
            .snapshot_with_preroll_limit(self.sample_rate, STT_PREROLL_LIMIT)
            .await;
        session.session_pcm.finish().await;
        if raw_pcm.is_empty() {
            anyhow::bail!("recording stopped before any audio was captured");
        }
        let final_audio = build_final_transcription_audio(self.sample_rate, &stt_view_pcm);
        eprintln!(
            "speaches-scribe final audio snapshot: {:.2}s raw including up to {}ms debug pre-roll",
            pcm_duration(self.sample_rate, raw_pcm.len()).as_secs_f64(),
            self.preroll.as_millis()
        );
        eprintln!(
            "speaches-scribe final audio sent: {:.2}s after trimming {:.2}s leading / {:.2}s trailing ({:.2}s detected trailing silence)",
            final_audio.audio_duration.as_secs_f64(),
            final_audio.leading_trim.as_secs_f64(),
            final_audio.trailing_trim.as_secs_f64(),
            final_audio.trailing_silence.as_secs_f64()
        );
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
                    "speaches-scribe preserved final transcript at {}",
                    path.display()
                ),
                Err(error) => {
                    eprintln!("speaches-scribe failed to preserve final transcript: {error:#}")
                }
            }
        }
        Ok(transcript)
    }
}

struct PartialTranscriptionLoop {
    session_pcm: StreamingPcmSession,
    stop_rx: watch::Receiver<bool>,
    updates_tx: mpsc::Sender<LiveTranscriptUpdate>,
    base_url: String,
    options: TranscribeOptions,
    sample_rate: u32,
    interval: Duration,
    min_duration: Duration,
    leading_silence: Duration,
    transcript_dir: Option<PathBuf>,
}

struct PartialTranscriptionRequest {
    request_id: u64,
    pcm: Vec<u8>,
    audio_duration: Duration,
    leading_trim: Duration,
    trailing_trim: Duration,
    trailing_silence: Duration,
}

struct PartialTranscriptionResult {
    audio_duration: Duration,
    result: anyhow::Result<Option<String>>,
}

async fn run_partial_transcriptions(loop_config: PartialTranscriptionLoop) {
    let PartialTranscriptionLoop {
        session_pcm,
        mut stop_rx,
        updates_tx,
        base_url,
        options,
        sample_rate,
        interval,
        min_duration,
        leading_silence,
        transcript_dir,
    } = loop_config;
    let min_bytes = pcm_bytes_for_duration(sample_rate, min_duration);
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut stabilizer = PartialTranscriptStabilizer::new(min_duration);
    let (result_tx, mut result_rx) = mpsc::channel::<PartialTranscriptionResult>(8);
    let mut latest_request_id = 0u64;
    let mut active_request: Option<JoinHandle<()>> = None;
    let mut pending_request: Option<PartialTranscriptionRequest> = None;

    loop {
        tokio::select! {
            changed = stop_rx.changed() => {
                if changed.is_err() || *stop_rx.borrow() {
                    break;
                }
            }
            Some(partial_result) = result_rx.recv() => {
                active_request = None;
                let mut updates_closed = false;
                match partial_result.result {
                    Ok(Some(transcript)) => {
                        if let Some(transcript) = normalize_transcript_for_injection(&transcript) {
                            if let Some(transcript) =
                                stabilizer.observe(partial_result.audio_duration, &transcript)
                            {
                                if let Some(transcript_dir) = transcript_dir.as_deref() {
                                    match preserve_transcript_snapshot(
                                        transcript_dir,
                                        &transcript,
                                        "partial",
                                    )
                                    .await
                                    {
                                        Ok(path) => eprintln!(
                                            "speaches-scribe preserved partial transcript at {}",
                                            path.display()
                                        ),
                                        Err(error) => eprintln!(
                                            "speaches-scribe failed to preserve partial transcript: {error:#}"
                                        ),
                                    }
                                }
                                if updates_tx
                                    .send(LiveTranscriptUpdate { transcript })
                                    .await
                                    .is_err()
                                {
                                    updates_closed = true;
                                }
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(error) => eprintln!("speaches-scribe partial transcription failed: {error:#}"),
                }
                if updates_closed {
                    break;
                }
                if let Some(request) = pending_request.take() {
                    active_request = Some(spawn_partial_transcription_request(
                        request,
                        result_tx.clone(),
                        &base_url,
                        &options,
                        sample_rate,
                        leading_silence,
                        &stop_rx,
                    ));
                }
            }
            _ = ticker.tick() => {
                let pcm = session_pcm
                    .snapshot_with_preroll_limit(sample_rate, STT_PREROLL_LIMIT)
                    .await;
                let Some(request) = build_partial_transcription_request(
                    sample_rate,
                    &pcm,
                    min_bytes,
                    latest_request_id.saturating_add(1),
                ) else {
                    continue;
                };
                if request.trailing_silence >= SEGMENT_READY_SILENCE {
                    eprintln!(
                        "speaches-scribe partial audio has {:.2}s trailing silence; future segment boundary candidate",
                        request.trailing_silence.as_secs_f64()
                    );
                }
                if request.leading_trim > Duration::ZERO
                    || request.trailing_trim > Duration::ZERO
                {
                    eprintln!(
                        "speaches-scribe partial audio sent: {:.2}s after trimming {:.2}s leading / {:.2}s trailing",
                        request.audio_duration.as_secs_f64(),
                        request.leading_trim.as_secs_f64(),
                        request.trailing_trim.as_secs_f64()
                    );
                }
                latest_request_id = request.request_id;
                if active_request.is_some() {
                    pending_request = Some(request);
                    continue;
                }
                active_request = Some(spawn_partial_transcription_request(
                    request,
                    result_tx.clone(),
                    &base_url,
                    &options,
                    sample_rate,
                    leading_silence,
                    &stop_rx,
                ));
            }
        }
    }

    if let Some(active_request) = active_request {
        active_request.abort();
    }
}

fn spawn_partial_transcription_request(
    request: PartialTranscriptionRequest,
    result_tx: mpsc::Sender<PartialTranscriptionResult>,
    base_url: &str,
    options: &TranscribeOptions,
    sample_rate: u32,
    leading_silence: Duration,
    stop_rx: &watch::Receiver<bool>,
) -> JoinHandle<()> {
    let request_base_url = base_url.to_string();
    let request_options = options.clone();
    let mut request_stop_rx = stop_rx.clone();
    tokio::spawn(async move {
        let result = transcribe_pcm_snapshot_until_stop(
            &request_base_url,
            &request_options,
            sample_rate,
            &request.pcm,
            "partial",
            leading_silence,
            &mut request_stop_rx,
        )
        .await;
        let _ = result_tx
            .send(PartialTranscriptionResult {
                audio_duration: request.audio_duration,
                result,
            })
            .await;
    })
}

async fn transcribe_pcm_snapshot_until_stop(
    base_url: &str,
    options: &TranscribeOptions,
    sample_rate: u32,
    pcm: &[u8],
    label: &str,
    leading_silence: Duration,
    stop_rx: &mut watch::Receiver<bool>,
) -> anyhow::Result<Option<String>> {
    let path = temp_audio_path(label);
    let pcm = pcm_with_leading_silence(sample_rate, leading_silence, pcm);
    write_pcm_wav(&path, &pcm, sample_rate).await?;
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
    detected_speech: bool,
}

#[derive(Debug, Clone)]
struct RmsFrame {
    start: usize,
    end: usize,
    rms: f64,
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

fn build_partial_transcription_request(
    sample_rate: u32,
    pcm: &[u8],
    min_bytes: usize,
    request_id: u64,
) -> Option<PartialTranscriptionRequest> {
    if pcm.is_empty() {
        return None;
    }

    let trim = trim_pcm_to_speech(sample_rate, pcm);
    if !trim.detected_speech || trim.pcm.len() < min_bytes {
        return None;
    }

    let (pcm, additional_leading_trim) = cap_partial_pcm(sample_rate, trim.pcm);
    let leading_trim = trim.leading_trim + additional_leading_trim;
    let audio_duration = pcm_duration(sample_rate, pcm.len());
    Some(PartialTranscriptionRequest {
        request_id,
        pcm,
        audio_duration,
        leading_trim,
        trailing_trim: trim.trailing_trim,
        trailing_silence: trim.trailing_silence,
    })
}

fn trim_pcm_to_speech(sample_rate: u32, pcm: &[u8]) -> SpeechTrim {
    let frames = rms_frames(sample_rate, pcm);
    if frames.is_empty() {
        return SpeechTrim {
            pcm: pcm.to_vec(),
            leading_trim: Duration::ZERO,
            trailing_trim: Duration::ZERO,
            trailing_silence: Duration::ZERO,
            detected_speech: false,
        };
    }

    let threshold = speech_threshold(sample_rate, &frames);
    let Some(first_speech) = frames.iter().position(|frame| frame.rms >= threshold) else {
        return SpeechTrim {
            pcm: pcm.to_vec(),
            leading_trim: Duration::ZERO,
            trailing_trim: Duration::ZERO,
            trailing_silence: pcm_duration(sample_rate, pcm.len()),
            detected_speech: false,
        };
    };
    let last_speech = frames
        .iter()
        .rposition(|frame| frame.rms >= threshold)
        .expect("first speech frame exists");
    let speech_start = frames[first_speech].start;
    let speech_end = frames[last_speech].end;
    if speech_end.saturating_sub(speech_start)
        < pcm_bytes_for_duration(sample_rate, TRIM_MIN_SPEECH)
    {
        return SpeechTrim {
            pcm: pcm.to_vec(),
            leading_trim: Duration::ZERO,
            trailing_trim: Duration::ZERO,
            trailing_silence: pcm_duration(sample_rate, pcm.len().saturating_sub(speech_end)),
            detected_speech: false,
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
        detected_speech: true,
    }
}

fn cap_partial_pcm(sample_rate: u32, pcm: Vec<u8>) -> (Vec<u8>, Duration) {
    let retain_bytes = pcm_bytes_for_duration(sample_rate, PARTIAL_MAX_AUDIO + PARTIAL_CONTEXT);
    let trim_bytes = pcm.len().saturating_sub(retain_bytes);
    if trim_bytes == 0 {
        return (pcm, Duration::ZERO);
    }

    (
        pcm[trim_bytes..].to_vec(),
        pcm_duration(sample_rate, trim_bytes),
    )
}

fn rms_frames(sample_rate: u32, pcm: &[u8]) -> Vec<RmsFrame> {
    let frame_bytes = pcm_bytes_for_duration(sample_rate, TRIM_ANALYSIS_FRAME).max(2);
    let mut frames = Vec::new();
    let mut start = 0usize;
    while start + 2 <= pcm.len() {
        let end = (start + frame_bytes).min(pcm.len());
        frames.push(RmsFrame {
            start,
            end,
            rms: pcm_rms(&pcm[start..end]),
        });
        start = end;
    }
    frames
}

fn speech_threshold(sample_rate: u32, frames: &[RmsFrame]) -> f64 {
    let noise_sample_bytes = pcm_bytes_for_duration(sample_rate, Duration::from_secs(1));
    let noise_frames = frames
        .iter()
        .take_while(|frame| frame.start < noise_sample_bytes)
        .collect::<Vec<_>>();
    if noise_frames.is_empty() {
        return FIXED_SPEECH_RMS_FLOOR;
    }

    let noise_floor =
        noise_frames.iter().map(|frame| frame.rms).sum::<f64>() / noise_frames.len() as f64;
    FIXED_SPEECH_RMS_FLOOR
        .max(noise_floor * NOISE_FLOOR_MULTIPLIER)
        .min(MAX_SPEECH_RMS_FLOOR)
}

fn pcm_rms(pcm: &[u8]) -> f64 {
    let mut sum = 0f64;
    let mut count = 0usize;
    for sample in pcm.chunks_exact(2) {
        let sample = i16::from_le_bytes([sample[0], sample[1]]) as f64;
        sum += sample * sample;
        count += 1;
    }

    if count == 0 {
        0.0
    } else {
        (sum / count as f64).sqrt()
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

fn transcript_file_name(label: &str) -> String {
    let counter = TEMP_AUDIO_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "speaches-scribe-{label}-{}-{counter}.txt",
        std::process::id()
    )
}

fn audio_file_name(label: &str) -> String {
    let counter = TEMP_AUDIO_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "speaches-scribe-{label}-{}-{counter}.wav",
        std::process::id()
    )
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

        if self.min_duration == Duration::ZERO {
            self.previous_candidate = Some(candidate.to_string());
            if candidate == self.last_emitted {
                return None;
            }
            self.last_emitted = candidate.to_string();
            return Some(candidate.to_string());
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

fn pcm_with_leading_silence(sample_rate: u32, duration: Duration, pcm: &[u8]) -> Vec<u8> {
    let silence_bytes = pcm_bytes_for_duration(sample_rate, duration);
    let mut padded_pcm = vec![0; silence_bytes];
    padded_pcm.extend_from_slice(pcm);
    padded_pcm
}

fn pcm_duration(sample_rate: u32, byte_len: usize) -> Duration {
    let samples = byte_len / 2;
    Duration::from_secs_f64(samples as f64 / f64::from(sample_rate))
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

        assert!(trim.detected_speech);
        assert_eq!(trim.leading_trim, Duration::from_millis(850));
        assert_eq!(trim.trailing_trim, Duration::from_millis(700));
        assert_eq!(
            pcm_duration(sample_rate, trim.pcm.len()),
            Duration::from_millis(950)
        );
    }

    #[test]
    fn speech_trimming_falls_back_when_no_speech_is_detected() {
        let sample_rate = 1_000;
        let pcm = pcm_for_duration(sample_rate, Duration::from_millis(400), 0);

        let trim = trim_pcm_to_speech(sample_rate, &pcm);

        assert!(!trim.detected_speech);
        assert_eq!(trim.pcm, pcm);
        assert_eq!(trim.trailing_silence, Duration::from_millis(400));
    }

    #[test]
    fn final_transcription_audio_preserves_pcm_when_no_speech_is_detected() {
        let sample_rate = 1_000;
        let pcm = pcm_for_duration(sample_rate, Duration::from_millis(400), 0);

        let audio = build_final_transcription_audio(sample_rate, &pcm);

        assert_eq!(audio.pcm, pcm);
        assert_eq!(audio.audio_duration, Duration::from_millis(400));
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

    #[test]
    fn partial_transcription_request_skips_silence() {
        let sample_rate = 1_000;
        let pcm = pcm_for_duration(sample_rate, Duration::from_millis(400), 0);

        assert!(build_partial_transcription_request(sample_rate, &pcm, 0, 1).is_none());
    }

    #[test]
    fn partial_transcription_request_is_bounded_to_recent_active_audio() {
        let sample_rate = 1_000;
        let pcm = pcm_for_duration(sample_rate, Duration::from_secs(10), 2_000);

        let request = build_partial_transcription_request(sample_rate, &pcm, 0, 7).unwrap();

        assert_eq!(request.request_id, 7);
        assert_eq!(request.audio_duration, Duration::from_millis(8_500));
        assert_eq!(request.leading_trim, Duration::from_millis(1_500));
    }

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
            Some("hello world".to_string())
        );
        assert_eq!(
            stabilizer.observe(Duration::from_millis(200), "hello world again"),
            Some("hello world again".to_string())
        );
    }

    #[test]
    fn partials_do_not_repeat_unchanged_zero_duration_candidates() {
        let mut stabilizer = PartialTranscriptStabilizer::new(Duration::ZERO);

        assert_eq!(
            stabilizer.observe(Duration::from_millis(100), "hello world"),
            Some("hello world".to_string())
        );
        assert_eq!(
            stabilizer.observe(Duration::from_millis(200), "hello world"),
            None
        );
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

    #[test]
    fn partials_emit_only_word_prefixes_seen_twice() {
        let mut stabilizer = PartialTranscriptStabilizer::new(Duration::from_millis(1));

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
        let mut stabilizer = PartialTranscriptStabilizer::new(Duration::from_millis(1));

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
