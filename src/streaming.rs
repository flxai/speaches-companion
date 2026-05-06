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
    active: Option<ActiveStreamingSession<L::Session, I>>,
}

struct ActiveStreamingSession<S, I>
where
    I: TextInjector,
{
    session: S,
    partial_task: JoinHandle<SpeculativeTextSession<I>>,
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
            active: None,
        }
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

        let text_session = match SpeculativeTextSession::start(self.injector.clone()) {
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
        self.notify_transcript(|notifier| notifier.notify_listening());

        let partial_task = tokio::spawn(consume_live_updates(
            live_session.updates,
            self.error_notifier.clone(),
            text_session,
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
                active.partial_task.abort();
                self.notify_failure("Streaming transcription failed", &error);
                return Err(error);
            }
        };
        let mut text_session = match active.partial_task.await {
            Ok(text_session) => text_session,
            Err(error) => {
                let error = anyhow::anyhow!("partial transcript task failed: {error:#}");
                self.notify_failure("Partial text replacement failed", &error);
                return Err(error);
            }
        };

        if let Some(text) = normalize_transcript_for_injection(&final_transcript) {
            if let Err(error) = text_session.replace_text(&text) {
                self.notify_failure("Final text replacement failed", &error);
                return Err(error);
            }
            self.notify_transcript(|notifier| notifier.notify_final(&text));
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
) -> SpeculativeTextSession<I>
where
    I: TextInjector,
    N: ErrorNotifier,
{
    while let Some(update) = updates.recv().await {
        let Some(transcript) = normalize_transcript_for_injection(&update.transcript) else {
            continue;
        };
        match text_session.replace_text(&transcript) {
            Ok(_) => {}
            Err(error) => {
                notify_failure(&error_notifier, "Partial text replacement failed", &error);
                break;
            }
        }
    }
    text_session
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
            partial_min_duration: Duration::from_millis(900),
            sample_rate: STT_SAMPLE_RATE,
        }
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
    let mut last_sent = String::new();

    loop {
        tokio::select! {
            changed = stop_rx.changed() => {
                if changed.is_err() || *stop_rx.borrow() {
                    break;
                }
            }
            _ = ticker.tick() => {
                let pcm = shared_pcm.lock().await.clone();
                if pcm.len() < min_bytes {
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
                        if transcript == last_sent {
                            continue;
                        }
                        last_sent.clone_from(&transcript);
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

fn pcm_bytes_for_duration(sample_rate: u32, duration: Duration) -> usize {
    let samples = duration.as_secs_f64() * f64::from(sample_rate);
    samples.ceil() as usize * 2
}

fn temp_audio_path(label: &str) -> PathBuf {
    let counter = TEMP_AUDIO_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("trec-{label}-{}-{counter}.wav", std::process::id()))
}
