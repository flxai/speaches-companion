use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;
use trec::daemon::{DaemonResponse, HotkeyHandler};
use trec::inject::{FocusedWindow, TextInjector};
use trec::ipc::IpcCommand;
use trec::notification::{ErrorNotifier, TranscriptNotifier, DICTATION_ERROR_SUMMARY};
use trec::streaming::{
    LiveTranscriber, LiveTranscriptUpdate, LiveTranscriptionSession, StreamingDictationController,
};

#[tokio::test]
async fn streaming_hotkey_replaces_partial_text_with_final_text() {
    let transcriber =
        FakeLiveTranscriber::new(["hel", "hello win"], Ok("  hello window\n".to_string()));
    let starts = transcriber.starts.clone();
    let stops = transcriber.stops.clone();
    let injector = FakeInjector::default();
    let operations = injector.operations.clone();
    let transcript_notifier = FakeTranscriptNotifier::default();
    let transcript_events = transcript_notifier.events.clone();
    let mut controller = StreamingDictationController::new_with_notifiers(
        transcriber,
        injector,
        transcript_notifier,
        FakeErrorNotifier::default(),
    );

    assert_eq!(
        controller
            .handle_hotkey(IpcCommand::HotkeyDown)
            .await
            .unwrap(),
        DaemonResponse::Started
    );
    assert!(controller.is_recording());
    assert_eq!(
        controller
            .handle_hotkey(IpcCommand::HotkeyUp)
            .await
            .unwrap(),
        DaemonResponse::Stopped
    );

    assert!(!controller.is_recording());
    assert_eq!(*starts.lock().unwrap(), 1);
    assert_eq!(*stops.lock().unwrap(), 1);
    assert_eq!(
        *operations.lock().unwrap(),
        vec![
            InjectOperation::Type("hel".to_string()),
            InjectOperation::Type("lo win".to_string()),
            InjectOperation::Type("dow".to_string()),
        ]
    );
    assert_eq!(
        *transcript_events.lock().unwrap(),
        vec![
            TranscriptNotice::Listening,
            TranscriptNotice::Final("hello window".to_string()),
        ]
    );
}

#[tokio::test]
async fn streaming_injects_live_partial_before_hotkey_up() {
    let transcriber = ManualLiveTranscriber::new("hello window");
    let updates = transcriber.updates.clone();
    let injector = FakeInjector::default();
    let operations = injector.operations.clone();
    let mut controller = StreamingDictationController::new_with_notifiers(
        transcriber,
        injector,
        FakeTranscriptNotifier::default(),
        FakeErrorNotifier::default(),
    );

    controller
        .handle_hotkey(IpcCommand::HotkeyDown)
        .await
        .unwrap();
    updates.send("hel").await;
    wait_for_operations_len(&operations, 1).await;

    assert_eq!(
        *operations.lock().unwrap(),
        vec![InjectOperation::Type("hel".to_string())]
    );
}

#[tokio::test]
async fn streaming_marker_is_replaced_by_live_partial_before_hotkey_up() {
    let transcriber = ManualLiveTranscriber::new("hello window");
    let updates = transcriber.updates.clone();
    let injector = FakeInjector::default();
    let operations = injector.operations.clone();
    let mut controller = StreamingDictationController::new_with_notifiers(
        transcriber,
        injector,
        FakeTranscriptNotifier::default(),
        FakeErrorNotifier::default(),
    )
    .with_listening_marker(Some("💬".to_string()));

    controller
        .handle_hotkey(IpcCommand::HotkeyDown)
        .await
        .unwrap();
    assert_eq!(
        *operations.lock().unwrap(),
        vec![InjectOperation::Type("💬".to_string())]
    );

    updates.send("hello").await;
    wait_for_operations_len(&operations, 3).await;

    assert_eq!(
        *operations.lock().unwrap(),
        vec![
            InjectOperation::Type("💬".to_string()),
            InjectOperation::Backspace(1),
            InjectOperation::Type("hello".to_string()),
        ]
    );
}

#[tokio::test]
async fn streaming_marker_is_replaced_by_partial_and_final_text() {
    let transcriber = FakeLiveTranscriber::new(["hello"], Ok("  hello window\n".to_string()));
    let injector = FakeInjector::default();
    let operations = injector.operations.clone();
    let mut controller = StreamingDictationController::new_with_notifiers(
        transcriber,
        injector,
        FakeTranscriptNotifier::default(),
        FakeErrorNotifier::default(),
    )
    .with_listening_marker(Some("💬".to_string()));

    controller
        .handle_hotkey(IpcCommand::HotkeyDown)
        .await
        .unwrap();
    controller
        .handle_hotkey(IpcCommand::HotkeyUp)
        .await
        .unwrap();

    assert_eq!(
        *operations.lock().unwrap(),
        vec![
            InjectOperation::Type("💬".to_string()),
            InjectOperation::Backspace(1),
            InjectOperation::Type("hello".to_string()),
            InjectOperation::Type(" window".to_string()),
        ]
    );
}

#[tokio::test]
async fn streaming_marker_is_erased_when_no_transcript_arrives() {
    let transcriber = FakeLiveTranscriber::new(Vec::<String>::new(), Ok(" \n".to_string()));
    let injector = FakeInjector::default();
    let operations = injector.operations.clone();
    let mut controller = StreamingDictationController::new_with_notifiers(
        transcriber,
        injector,
        FakeTranscriptNotifier::default(),
        FakeErrorNotifier::default(),
    )
    .with_listening_marker(Some("💬".to_string()));

    controller
        .handle_hotkey(IpcCommand::HotkeyDown)
        .await
        .unwrap();
    controller
        .handle_hotkey(IpcCommand::HotkeyUp)
        .await
        .unwrap();

    assert_eq!(
        *operations.lock().unwrap(),
        vec![
            InjectOperation::Type("💬".to_string()),
            InjectOperation::Backspace(1),
        ]
    );
}

#[tokio::test]
async fn streaming_uses_last_partial_when_final_is_empty() {
    let transcriber = FakeLiveTranscriber::new(["fallback text"], Ok(" \n".to_string()));
    let injector = FakeInjector::default();
    let operations = injector.operations.clone();
    let mut controller = StreamingDictationController::new_with_notifiers(
        transcriber,
        injector,
        FakeTranscriptNotifier::default(),
        FakeErrorNotifier::default(),
    );

    controller
        .handle_hotkey(IpcCommand::HotkeyDown)
        .await
        .unwrap();
    controller
        .handle_hotkey(IpcCommand::HotkeyUp)
        .await
        .unwrap();

    assert_eq!(
        *operations.lock().unwrap(),
        vec![InjectOperation::Type("fallback text".to_string())]
    );
}

#[tokio::test]
async fn streaming_can_defer_partial_injection_until_stop() {
    let transcriber = ManualLiveTranscriber::new(" \n");
    let updates = transcriber.updates.clone();
    let injector = FakeInjector::default();
    let operations = injector.operations.clone();
    let mut controller = StreamingDictationController::new_with_notifiers(
        transcriber,
        injector,
        FakeTranscriptNotifier::default(),
        FakeErrorNotifier::default(),
    )
    .with_inline_partials(false);

    controller
        .handle_hotkey(IpcCommand::HotkeyDown)
        .await
        .unwrap();
    updates.send_until_first_is_processed("fallback text").await;
    assert!(operations.lock().unwrap().is_empty());

    controller
        .handle_hotkey(IpcCommand::HotkeyUp)
        .await
        .unwrap();

    assert_eq!(
        *operations.lock().unwrap(),
        vec![InjectOperation::Type("fallback text".to_string())]
    );
}

#[tokio::test]
async fn streaming_stop_error_keeps_speculative_partial_and_notifies() {
    let transcriber = ManualLiveTranscriber::failing("connection refused");
    let updates = transcriber.updates.clone();
    let injector = FakeInjector::default();
    let operations = injector.operations.clone();
    let error_notifier = FakeErrorNotifier::default();
    let errors = error_notifier.errors.clone();
    let mut controller = StreamingDictationController::new_with_notifiers(
        transcriber,
        injector,
        FakeTranscriptNotifier::default(),
        error_notifier,
    );

    controller
        .handle_hotkey(IpcCommand::HotkeyDown)
        .await
        .unwrap();
    updates.send("partial").await;
    wait_for_operations_len(&operations, 1).await;
    let error = controller
        .handle_hotkey(IpcCommand::HotkeyUp)
        .await
        .unwrap_err();

    assert!(!controller.is_recording());
    assert!(error.to_string().contains("connection refused"));
    assert_eq!(
        *operations.lock().unwrap(),
        vec![InjectOperation::Type("partial".to_string())]
    );
    assert_eq!(
        *errors.lock().unwrap(),
        vec![(
            DICTATION_ERROR_SUMMARY.to_string(),
            "Streaming transcription failed: connection refused".to_string()
        )]
    );
}

#[tokio::test]
async fn streaming_stop_error_cleans_up_marker_when_no_partial_exists() {
    let transcriber =
        FakeLiveTranscriber::new(Vec::<String>::new(), Err("connection refused".to_string()));
    let injector = FakeInjector::default();
    let operations = injector.operations.clone();
    let error_notifier = FakeErrorNotifier::default();
    let mut controller = StreamingDictationController::new_with_notifiers(
        transcriber,
        injector,
        FakeTranscriptNotifier::default(),
        error_notifier,
    )
    .with_listening_marker(Some("💬".to_string()));

    controller
        .handle_hotkey(IpcCommand::HotkeyDown)
        .await
        .unwrap();
    let error = controller
        .handle_hotkey(IpcCommand::HotkeyUp)
        .await
        .unwrap_err();

    assert!(error.to_string().contains("connection refused"));
    assert_eq!(
        *operations.lock().unwrap(),
        vec![
            InjectOperation::Type("💬".to_string()),
            InjectOperation::Backspace(1),
        ]
    );
}

#[tokio::test]
async fn streaming_aborts_final_replacement_when_focus_changes() {
    let transcriber = FakeLiveTranscriber::new(Vec::<String>::new(), Ok("hello".to_string()));
    let injector = FakeInjector::default();
    let operations = injector.operations.clone();
    let current_window = injector.current_window.clone();
    let error_notifier = FakeErrorNotifier::default();
    let errors = error_notifier.errors.clone();
    let mut controller = StreamingDictationController::new_with_notifiers(
        transcriber,
        injector,
        FakeTranscriptNotifier::default(),
        error_notifier,
    );

    controller
        .handle_hotkey(IpcCommand::HotkeyDown)
        .await
        .unwrap();
    *current_window.lock().unwrap() = Some(FocusedWindow(2));
    let error = controller
        .handle_hotkey(IpcCommand::HotkeyUp)
        .await
        .unwrap_err();

    assert!(error.to_string().contains("focused X11 window changed"));
    assert!(operations.lock().unwrap().is_empty());
    assert_eq!(
        *errors.lock().unwrap(),
        vec![(
            DICTATION_ERROR_SUMMARY.to_string(),
            "Final text replacement failed: focused X11 window changed from 1 to 2; aborting replacement".to_string()
        )]
    );
}

#[tokio::test]
async fn duplicate_streaming_partials_are_ignored() {
    let transcriber =
        FakeLiveTranscriber::new(["hello", "hello", "hello"], Ok("hello".to_string()));
    let injector = FakeInjector::default();
    let operations = injector.operations.clone();
    let transcript_notifier = FakeTranscriptNotifier::default();
    let transcript_events = transcript_notifier.events.clone();
    let mut controller = StreamingDictationController::new_with_notifiers(
        transcriber,
        injector,
        transcript_notifier,
        FakeErrorNotifier::default(),
    );

    controller
        .handle_hotkey(IpcCommand::HotkeyDown)
        .await
        .unwrap();
    controller
        .handle_hotkey(IpcCommand::HotkeyUp)
        .await
        .unwrap();

    assert_eq!(
        *operations.lock().unwrap(),
        vec![InjectOperation::Type("hello".to_string())]
    );
    assert_eq!(
        *transcript_events.lock().unwrap(),
        vec![
            TranscriptNotice::Listening,
            TranscriptNotice::Final("hello".to_string()),
        ]
    );
}

#[tokio::test]
async fn repeated_streaming_down_does_not_start_second_session() {
    let transcriber = FakeLiveTranscriber::new(Vec::<String>::new(), Ok("hello".to_string()));
    let starts = transcriber.starts.clone();
    let mut controller = StreamingDictationController::new(transcriber, FakeInjector::default());

    assert_eq!(
        controller
            .handle_hotkey(IpcCommand::HotkeyDown)
            .await
            .unwrap(),
        DaemonResponse::Started
    );
    assert_eq!(
        controller
            .handle_hotkey(IpcCommand::HotkeyDown)
            .await
            .unwrap(),
        DaemonResponse::AlreadyRecording
    );

    assert_eq!(*starts.lock().unwrap(), 1);
}

#[tokio::test]
async fn streaming_starts_audio_capture_before_target_snapshot() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let transcriber = OrderingTranscriber {
        events: events.clone(),
    };
    let injector = OrderingInjector {
        events: events.clone(),
    };
    let mut controller = StreamingDictationController::new(transcriber, injector);

    controller
        .handle_hotkey(IpcCommand::HotkeyDown)
        .await
        .unwrap();

    assert_eq!(*events.lock().unwrap(), vec!["start", "focus"]);
}

#[derive(Clone)]
struct FakeSession;

#[derive(Clone)]
struct ManualLiveTranscriber {
    updates: ManualLiveUpdates,
    final_result: Result<String, String>,
}

impl ManualLiveTranscriber {
    fn new(final_transcript: impl Into<String>) -> Self {
        Self {
            updates: ManualLiveUpdates::default(),
            final_result: Ok(final_transcript.into()),
        }
    }

    fn failing(message: impl Into<String>) -> Self {
        Self {
            updates: ManualLiveUpdates::default(),
            final_result: Err(message.into()),
        }
    }
}

#[async_trait]
impl LiveTranscriber for ManualLiveTranscriber {
    type Session = FakeSession;

    async fn start(&self) -> anyhow::Result<LiveTranscriptionSession<Self::Session>> {
        let (updates_tx, updates_rx) = mpsc::channel(1);
        *self.updates.sender.lock().unwrap() = Some(updates_tx);
        Ok(LiveTranscriptionSession {
            session: FakeSession,
            updates: updates_rx,
        })
    }

    async fn stop(&self, _session: Self::Session) -> anyhow::Result<String> {
        self.updates.sender.lock().unwrap().take();
        self.final_result.clone().map_err(anyhow::Error::msg)
    }
}

#[derive(Clone, Default)]
struct ManualLiveUpdates {
    sender: Arc<Mutex<Option<mpsc::Sender<LiveTranscriptUpdate>>>>,
}

impl ManualLiveUpdates {
    async fn send(&self, transcript: &str) {
        let sender = self.sender.lock().unwrap().clone().unwrap();
        sender
            .send(LiveTranscriptUpdate {
                transcript: transcript.to_string(),
            })
            .await
            .unwrap();
    }

    async fn send_until_first_is_processed(&self, transcript: &str) {
        // With a one-slot channel, completing the third send means the receiver
        // has returned to recv after fully handling the first update.
        self.send(transcript).await;
        self.send(transcript).await;
        self.send(transcript).await;
    }
}

async fn wait_for_operations_len(
    operations: &Arc<Mutex<Vec<InjectOperation>>>,
    expected_len: usize,
) {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if operations.lock().unwrap().len() >= expected_len {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[derive(Clone)]
struct OrderingTranscriber {
    events: Arc<Mutex<Vec<&'static str>>>,
}

#[async_trait]
impl LiveTranscriber for OrderingTranscriber {
    type Session = FakeSession;

    async fn start(&self) -> anyhow::Result<LiveTranscriptionSession<Self::Session>> {
        self.events.lock().unwrap().push("start");
        let (_updates_tx, updates_rx) = tokio::sync::mpsc::channel(8);
        Ok(LiveTranscriptionSession {
            session: FakeSession,
            updates: updates_rx,
        })
    }

    async fn stop(&self, _session: Self::Session) -> anyhow::Result<String> {
        Ok(String::new())
    }
}

#[derive(Clone)]
struct OrderingInjector {
    events: Arc<Mutex<Vec<&'static str>>>,
}

impl TextInjector for OrderingInjector {
    fn inject_text(&self, _text: &str) -> anyhow::Result<()> {
        Ok(())
    }

    fn focused_window(&self) -> anyhow::Result<Option<FocusedWindow>> {
        self.events.lock().unwrap().push("focus");
        Ok(Some(FocusedWindow(1)))
    }
}

#[derive(Clone)]
struct FakeLiveTranscriber {
    updates: Vec<String>,
    final_result: Result<String, String>,
    starts: Arc<Mutex<usize>>,
    stops: Arc<Mutex<usize>>,
}

impl FakeLiveTranscriber {
    fn new(
        updates: impl IntoIterator<Item = impl Into<String>>,
        final_result: Result<String, String>,
    ) -> Self {
        Self {
            updates: updates.into_iter().map(Into::into).collect(),
            final_result,
            starts: Arc::new(Mutex::new(0)),
            stops: Arc::new(Mutex::new(0)),
        }
    }
}

#[async_trait]
impl LiveTranscriber for FakeLiveTranscriber {
    type Session = FakeSession;

    async fn start(&self) -> anyhow::Result<LiveTranscriptionSession<Self::Session>> {
        *self.starts.lock().unwrap() += 1;
        let (updates_tx, updates_rx) = tokio::sync::mpsc::channel(8);
        for transcript in &self.updates {
            updates_tx
                .send(LiveTranscriptUpdate {
                    transcript: transcript.clone(),
                })
                .await
                .unwrap();
        }
        drop(updates_tx);
        Ok(LiveTranscriptionSession {
            session: FakeSession,
            updates: updates_rx,
        })
    }

    async fn stop(&self, _session: Self::Session) -> anyhow::Result<String> {
        *self.stops.lock().unwrap() += 1;
        self.final_result.clone().map_err(anyhow::Error::msg)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InjectOperation {
    Type(String),
    Backspace(usize),
}

#[derive(Clone)]
struct FakeInjector {
    operations: Arc<Mutex<Vec<InjectOperation>>>,
    current_window: Arc<Mutex<Option<FocusedWindow>>>,
}

impl Default for FakeInjector {
    fn default() -> Self {
        Self {
            operations: Arc::new(Mutex::new(Vec::new())),
            current_window: Arc::new(Mutex::new(Some(FocusedWindow(1)))),
        }
    }
}

impl TextInjector for FakeInjector {
    fn inject_text(&self, text: &str) -> anyhow::Result<()> {
        if !text.is_empty() {
            self.operations
                .lock()
                .unwrap()
                .push(InjectOperation::Type(text.to_string()));
        }
        Ok(())
    }

    fn erase_chars(&self, count: usize) -> anyhow::Result<()> {
        if count > 0 {
            self.operations
                .lock()
                .unwrap()
                .push(InjectOperation::Backspace(count));
        }
        Ok(())
    }

    fn focused_window(&self) -> anyhow::Result<Option<FocusedWindow>> {
        Ok(*self.current_window.lock().unwrap())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TranscriptNotice {
    Listening,
    Partial(String),
    Final(String),
}

#[derive(Clone, Default)]
struct FakeTranscriptNotifier {
    events: Arc<Mutex<Vec<TranscriptNotice>>>,
}

impl TranscriptNotifier for FakeTranscriptNotifier {
    fn notify_listening(&self) -> anyhow::Result<()> {
        self.events
            .lock()
            .unwrap()
            .push(TranscriptNotice::Listening);
        Ok(())
    }

    fn notify_partial(&self, transcript: &str) -> anyhow::Result<()> {
        self.events
            .lock()
            .unwrap()
            .push(TranscriptNotice::Partial(transcript.to_string()));
        Ok(())
    }

    fn notify_final(&self, transcript: &str) -> anyhow::Result<()> {
        self.events
            .lock()
            .unwrap()
            .push(TranscriptNotice::Final(transcript.to_string()));
        Ok(())
    }
}

#[derive(Clone, Default)]
struct FakeErrorNotifier {
    errors: Arc<Mutex<Vec<(String, String)>>>,
}

impl ErrorNotifier for FakeErrorNotifier {
    fn notify_error(&self, summary: &str, body: &str) -> anyhow::Result<()> {
        self.errors
            .lock()
            .unwrap()
            .push((summary.to_string(), body.to_string()));
        Ok(())
    }
}
