use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task::JoinHandle;

#[cfg(feature = "debug-recordings")]
use crate::audio::write_pcm_mp3;
use crate::audio::{
    start_streaming_pcm_capture, write_pcm_wav, SharedPcmBuffer, StreamingPcmCapture,
    StreamingPcmSession, STT_SAMPLE_RATE,
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
use crate::stt::{transcribe_file, TranscribeOptions};

static TEMP_AUDIO_COUNTER: AtomicU64 = AtomicU64::new(0);
const TRANSCRIBER_WARMUP_DURATION: Duration = Duration::from_millis(500);
const TRIM_LEADING_PAD: Duration = Duration::from_millis(500);
const TRIM_TRAILING_PAD: Duration = Duration::from_millis(300);
const TRIM_ANALYSIS_FRAME: Duration = Duration::from_millis(20);
const TRIM_MIN_SPEECH: Duration = Duration::from_millis(120);
const FIXED_SPEECH_RMS_FLOOR: f64 = 700.0;
const MAX_SPEECH_RMS_FLOOR: f64 = 1_500.0;
const NOISE_FLOOR_MULTIPLIER: f64 = 4.0;

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
    ShowWaitingMarker {
        marker: Option<String>,
        ack: oneshot::Sender<anyhow::Result<()>>,
    },
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
            if let Err(error) = text_session.replace_text(marker) {
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

        if self.final_transcript {
            if let Err(error) =
                show_waiting_marker(&partial_command_tx, self.listening_marker.clone()).await
            {
                self.notify_failure("Final wait marker replacement failed", &error);
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

        let final_transcript = if self.final_transcript {
            normalize_transcript_for_injection(&stop_transcript)
        } else {
            None
        };

        match final_transcript {
            Some(transcript) => {
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
) -> PartialTextSession<I>
where
    I: TextInjector,
    V: TranscriptNotifier,
    N: ErrorNotifier,
{
    let mut latest_partial = None;
    let mut waiting_marker = None;
    let mut updates_open = true;
    let mut commands_open = true;
    while updates_open || commands_open {
        tokio::select! {
            update = updates.recv(), if updates_open => {
                let Some(update) = update else {
                    updates_open = false;
                    continue;
                };
                let transcript = normalize_transcript_for_injection(&update.transcript);
                let Some(transcript) = transcript else {
                    continue;
                };
                if latest_partial.as_deref() == Some(transcript.as_str()) && waiting_marker.is_none() {
                    continue;
                }
                latest_partial = Some(transcript.clone());
                let mut displayed_inline = false;
                if inline_partials || waiting_marker.is_some() {
                    let display_text = displayed_partial_text(latest_partial.as_deref(), waiting_marker.as_deref());
                    match text_session.replace_text(&display_text) {
                        Ok(changed) => displayed_inline = changed,
                        Err(error) => {
                            notify_failure(&error_notifier, "Partial text replacement failed", &error);
                            break;
                        }
                    }
                }
                if displayed_inline || !inline_partials {
                    notify_transcript(&transcript_notifier, |notifier| {
                        notifier.notify_partial(&transcript)
                    });
                }
            }
            command = commands.recv(), if commands_open => {
                let Some(command) = command else {
                    commands_open = false;
                    continue;
                };
                match command {
                    PartialTextCommand::ShowWaitingMarker { marker, ack } => {
                        waiting_marker = marker.and_then(|marker| normalize_transcript_for_injection(&marker));
                        let result = match waiting_marker.as_deref() {
                            Some(_) => {
                                let display_text = displayed_partial_text(latest_partial.as_deref(), waiting_marker.as_deref());
                                text_session.replace_text(&display_text).map(|_| ())
                            }
                            None => Ok(()),
                        };
                        let _ = ack.send(result);
                    }
                }
            }
        }
    }
    PartialTextSession {
        text_session,
        latest_partial,
    }
}

async fn show_waiting_marker(
    command_tx: &mpsc::Sender<PartialTextCommand>,
    marker: Option<String>,
) -> anyhow::Result<()> {
    let Some(marker) = marker.and_then(|marker| normalize_transcript_for_injection(&marker)) else {
        return Ok(());
    };
    let (ack_tx, ack_rx) = oneshot::channel();
    command_tx
        .send(PartialTextCommand::ShowWaitingMarker {
            marker: Some(marker),
            ack: ack_tx,
        })
        .await
        .context("partial text task stopped before final wait marker could be shown")?;
    ack_rx
        .await
        .context("partial text task stopped before acknowledging final wait marker")?
}

fn displayed_partial_text(partial: Option<&str>, waiting_marker: Option<&str>) -> String {
    match (partial, waiting_marker) {
        (Some(partial), Some(marker)) => append_marker(partial, marker),
        (Some(partial), None) => partial.to_string(),
        (None, Some(marker)) => marker.to_string(),
        (None, None) => String::new(),
    }
}

fn append_marker(text: &str, marker: &str) -> String {
    if text.is_empty() {
        marker.to_string()
    } else if text.ends_with(char::is_whitespace) || marker.starts_with(char::is_whitespace) {
        format!("{text}{marker}")
    } else {
        format!("{text} {marker}")
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

fn notify_transcript<V>(notifier: &V, notify: impl FnOnce(&V) -> anyhow::Result<()>)
where
    V: TranscriptNotifier,
{
    if let Err(error) = notify(notifier) {
        eprintln!("speaches-scribe transcript notification failed: {error:#}");
    }
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
            "speaches-scribe hotkey-down audio buffer: {:.2}s available; retained {:.2}s pre-roll (requested {}ms)",
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
                    "speaches-scribe preserved MP3 recording at {}",
                    path.display()
                ),
                Err(error) => {
                    eprintln!("speaches-scribe failed to preserve MP3 recording: {error:#}")
                }
            }
        }
        let final_audio = build_final_transcription_audio(self.sample_rate, &raw_pcm);
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

fn trim_pcm_to_speech(sample_rate: u32, pcm: &[u8]) -> SpeechTrim {
    let frames = rms_frames(sample_rate, pcm);
    if frames.is_empty() {
        return SpeechTrim {
            pcm: pcm.to_vec(),
            leading_trim: Duration::ZERO,
            trailing_trim: Duration::ZERO,
            trailing_silence: Duration::ZERO,
        };
    }

    let threshold = speech_threshold(sample_rate, &frames);
    let Some(first_speech) = frames.iter().position(|frame| frame.rms >= threshold) else {
        return SpeechTrim {
            pcm: pcm.to_vec(),
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
        < pcm_bytes_for_duration(sample_rate, TRIM_MIN_SPEECH)
    {
        return SpeechTrim {
            pcm: pcm.to_vec(),
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
        "speaches-scribe-{label}-{}-{counter}.txt",
        std::process::id()
    )
}

#[cfg(feature = "debug-recordings")]
fn recording_file_name(label: &str) -> String {
    let counter = TEMP_AUDIO_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "speaches-scribe-{label}-{}-{counter}.mp3",
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

        assert_eq!(trim.leading_trim, Duration::from_millis(500));
        assert_eq!(trim.trailing_trim, Duration::from_millis(700));
        assert_eq!(
            pcm_duration(sample_rate, trim.pcm.len()),
            Duration::from_millis(1_300)
        );
    }

    #[test]
    fn speech_trimming_falls_back_when_no_speech_is_detected() {
        let sample_rate = 1_000;
        let pcm = pcm_for_duration(sample_rate, Duration::from_millis(400), 0);

        let trim = trim_pcm_to_speech(sample_rate, &pcm);

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

    #[cfg(feature = "debug-recordings")]
    #[test]
    fn recording_snapshot_names_use_mp3_extension() {
        let name = recording_file_name("realtime");

        assert!(name.starts_with("speaches-scribe-realtime-"));
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
