use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::audio::{
    start_raw_pcm_recording, stop_raw_pcm_recording, RawPcmRecording, STT_SAMPLE_RATE,
};
use crate::clock::unix_millis;
use crate::daemon::{DaemonResponse, HotkeyHandler};
use crate::inject::{format_transcript_for_injection, TextInjector};
use crate::ipc::IpcCommand;
use crate::notification::{
    dictation_error_body, ErrorNotifier, NoopErrorNotifier, DICTATION_ERROR_SUMMARY,
};
use crate::stt::{transcribe_file, TranscribeOptions};

#[async_trait]
pub trait Recorder: Send + Sync {
    type Recording: Send;

    async fn start(&self) -> anyhow::Result<Self::Recording>;
    async fn stop(&self, recording: Self::Recording) -> anyhow::Result<PathBuf>;
}

#[async_trait]
pub trait Transcriber: Send + Sync {
    async fn transcribe(&self, audio_path: &Path) -> anyhow::Result<String>;
}

pub struct DictationController<R, T, I, N = NoopErrorNotifier>
where
    R: Recorder,
    T: Transcriber,
    I: TextInjector,
    N: ErrorNotifier,
{
    recorder: R,
    transcriber: T,
    injector: I,
    notifier: N,
    recording: Option<R::Recording>,
    append_space: bool,
}

impl<R, T, I> DictationController<R, T, I>
where
    R: Recorder,
    T: Transcriber,
    I: TextInjector,
{
    pub fn new(recorder: R, transcriber: T, injector: I) -> Self {
        Self {
            recorder,
            transcriber,
            injector,
            notifier: NoopErrorNotifier,
            recording: None,
            append_space: true,
        }
    }
}

impl<R, T, I, N> DictationController<R, T, I, N>
where
    R: Recorder,
    T: Transcriber,
    I: TextInjector,
    N: ErrorNotifier,
{
    pub fn new_with_notifier(recorder: R, transcriber: T, injector: I, notifier: N) -> Self {
        Self {
            recorder,
            transcriber,
            injector,
            notifier,
            recording: None,
            append_space: true,
        }
    }

    pub fn with_append_space(mut self, append_space: bool) -> Self {
        self.append_space = append_space;
        self
    }

    pub fn is_recording(&self) -> bool {
        self.recording.is_some()
    }
}

#[async_trait]
impl<R, T, I, N> HotkeyHandler for DictationController<R, T, I, N>
where
    R: Recorder + Send,
    T: Transcriber + Send,
    I: TextInjector + Send,
    N: ErrorNotifier + Send,
{
    async fn handle_hotkey(&mut self, command: IpcCommand) -> anyhow::Result<DaemonResponse> {
        match command {
            IpcCommand::HotkeyDown => self.start_recording().await,
            IpcCommand::HotkeyUp => self.stop_transcribe_and_inject().await,
        }
    }
}

impl<R, T, I, N> DictationController<R, T, I, N>
where
    R: Recorder,
    T: Transcriber,
    I: TextInjector,
    N: ErrorNotifier,
{
    async fn start_recording(&mut self) -> anyhow::Result<DaemonResponse> {
        if self.recording.is_some() {
            return Ok(DaemonResponse::AlreadyRecording);
        }

        self.recording = Some(match self.recorder.start().await {
            Ok(recording) => recording,
            Err(error) => {
                self.notify_failure("Recording start failed", &error);
                return Err(error);
            }
        });
        Ok(DaemonResponse::Started)
    }

    async fn stop_transcribe_and_inject(&mut self) -> anyhow::Result<DaemonResponse> {
        let Some(recording) = self.recording.take() else {
            return Ok(DaemonResponse::AlreadyIdle);
        };

        let audio_path = match self.recorder.stop(recording).await {
            Ok(path) => path,
            Err(error) => {
                self.notify_failure("Recording stop failed", &error);
                return Err(error);
            }
        };
        let transcript = match self.transcriber.transcribe(&audio_path).await {
            Ok(transcript) => transcript,
            Err(error) => {
                self.notify_failure("Transcription failed", &error);
                return Err(error);
            }
        };
        if let Some(text) = format_transcript_for_injection(&transcript, self.append_space) {
            if let Err(error) = self.injector.inject_text(&text) {
                self.notify_failure("Text injection failed", &error);
                return Err(error);
            }
        }

        Ok(DaemonResponse::Stopped)
    }

    fn notify_failure(&self, stage: &str, error: &anyhow::Error) {
        let body = dictation_error_body(stage, error);
        if let Err(notify_error) = self.notifier.notify_error(DICTATION_ERROR_SUMMARY, &body) {
            eprintln!("speaches-companion notification failed: {notify_error:#}");
        }
    }
}

#[derive(Debug, Clone)]
pub struct PwRecordRecorder {
    output_dir: PathBuf,
    sample_rate: u32,
}

impl PwRecordRecorder {
    pub fn new(output_dir: PathBuf) -> Self {
        Self {
            output_dir,
            sample_rate: STT_SAMPLE_RATE,
        }
    }

    pub fn with_sample_rate(mut self, sample_rate: u32) -> Self {
        self.sample_rate = sample_rate;
        self
    }
}

pub struct PwRecording {
    inner: RawPcmRecording,
    output_path: PathBuf,
    sample_rate: u32,
}

#[async_trait]
impl Recorder for PwRecordRecorder {
    type Recording = PwRecording;

    async fn start(&self) -> anyhow::Result<Self::Recording> {
        let output_path = self.output_dir.join(recording_file_name());
        let inner = start_raw_pcm_recording(self.sample_rate).await?;
        Ok(PwRecording {
            inner,
            output_path,
            sample_rate: self.sample_rate,
        })
    }

    async fn stop(&self, recording: Self::Recording) -> anyhow::Result<PathBuf> {
        stop_raw_pcm_recording(
            recording.inner,
            &recording.output_path,
            recording.sample_rate,
        )
        .await?;
        Ok(recording.output_path)
    }
}

#[derive(Debug, Clone)]
pub struct SpeachesTranscriber {
    base_url: String,
    options: TranscribeOptions,
}

impl SpeachesTranscriber {
    pub fn new(base_url: String, options: TranscribeOptions) -> Self {
        Self { base_url, options }
    }
}

#[async_trait]
impl Transcriber for SpeachesTranscriber {
    async fn transcribe(&self, audio_path: &Path) -> anyhow::Result<String> {
        transcribe_file(&self.base_url, audio_path, &self.options).await
    }
}

pub fn default_recording_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("speaches-companion-recordings")
}

fn recording_file_name() -> String {
    format!(
        "speaches-companion-{}-{}.wav",
        std::process::id(),
        unix_millis()
    )
}
