use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use speaches_companion::daemon::{DaemonResponse, HotkeyHandler};
use speaches_companion::inject::{FocusedWindow, TextInjector};
use speaches_companion::ipc::IpcCommand;
use speaches_companion::notification::{
    ErrorNotifier, TranscriptNotifier, DICTATION_ERROR_SUMMARY,
};
use speaches_companion::streaming::{
    LiveTranscriber, LiveTranscriptUpdate, LiveTranscriptionSession, PartialChunkingConfig,
    StreamingDictationController,
};
use tokio::sync::mpsc;

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
            InjectOperation::Type("hello win".to_string()),
            InjectOperation::Type("dow ".to_string()),
        ]
    );
    assert_eq!(
        *transcript_events.lock().unwrap(),
        vec![
            TranscriptNotice::Listening,
            TranscriptNotice::Partial("hello win".to_string()),
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
    )
    .with_partial_chunking_config(no_chunking());

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
async fn streaming_empty_live_update_keeps_existing_provisional_text() {
    let transcriber = ManualLiveTranscriber::new("");
    let updates = transcriber.updates.clone();
    let injector = FakeInjector::default();
    let operations = injector.operations.clone();
    let mut controller = StreamingDictationController::new_with_notifiers(
        transcriber,
        injector,
        FakeTranscriptNotifier::default(),
        FakeErrorNotifier::default(),
    )
    .with_partial_chunking_config(no_chunking());

    controller
        .handle_hotkey(IpcCommand::HotkeyDown)
        .await
        .unwrap();
    updates.send("hello").await;
    wait_for_operations_len(&operations, 1).await;
    updates.send("").await;
    tokio::task::yield_now().await;

    assert_eq!(
        *operations.lock().unwrap(),
        vec![InjectOperation::Type("hello".to_string())]
    );
}

#[tokio::test]
async fn streaming_empty_live_update_does_not_erase_marker_or_text() {
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
    .with_partial_chunking_config(no_chunking())
    .with_listening_marker(Some("💬".to_string()));

    controller
        .handle_hotkey(IpcCommand::HotkeyDown)
        .await
        .unwrap();
    updates.send("the front").await;
    wait_for_operations_len(&operations, 3).await;
    updates.send("").await;
    tokio::task::yield_now().await;
    updates.send("the front fell").await;
    wait_for_operations_len(&operations, 4).await;

    assert_eq!(
        *operations.lock().unwrap(),
        vec![
            InjectOperation::Type("💬".to_string()),
            InjectOperation::CursorLeft(1),
            InjectOperation::Type("the front".to_string()),
            InjectOperation::Type(" fell".to_string()),
        ]
    );
}

#[tokio::test]
async fn streaming_marker_tracks_live_partial_before_hotkey_up() {
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
    .with_partial_chunking_config(no_chunking())
    .with_listening_marker(Some("💬".to_string()));

    controller
        .handle_hotkey(IpcCommand::HotkeyDown)
        .await
        .unwrap();
    assert_eq!(
        *operations.lock().unwrap(),
        vec![
            InjectOperation::Type("💬".to_string()),
            InjectOperation::CursorLeft(1),
        ]
    );

    updates.send("hello").await;
    wait_for_operations_len(&operations, 3).await;

    assert_eq!(
        *operations.lock().unwrap(),
        vec![
            InjectOperation::Type("💬".to_string()),
            InjectOperation::CursorLeft(1),
            InjectOperation::Type("hello".to_string()),
        ]
    );
}

#[tokio::test]
async fn streaming_marker_is_removed_before_partial_and_final_text_commit() {
    let transcriber = ManualLiveTranscriber::new("  hello window\n");
    let updates = transcriber.updates.clone();
    let injector = FakeInjector::default();
    let operations = injector.operations.clone();
    let mut controller = StreamingDictationController::new_with_notifiers(
        transcriber,
        injector,
        FakeTranscriptNotifier::default(),
        FakeErrorNotifier::default(),
    )
    .with_partial_chunking_config(no_chunking())
    .with_listening_marker(Some("💬".to_string()));

    controller
        .handle_hotkey(IpcCommand::HotkeyDown)
        .await
        .unwrap();
    updates.send("hello").await;
    wait_for_operations_len(&operations, 3).await;
    controller
        .handle_hotkey(IpcCommand::HotkeyUp)
        .await
        .unwrap();

    assert_eq!(
        *operations.lock().unwrap(),
        vec![
            InjectOperation::Type("💬".to_string()),
            InjectOperation::CursorLeft(1),
            InjectOperation::Type("hello".to_string()),
            InjectOperation::CursorRight(1),
            InjectOperation::Backspace(1),
            InjectOperation::Type(" window ".to_string()),
        ]
    );
}

#[tokio::test]
async fn streaming_shows_wait_marker_while_final_transcript_is_pending() {
    let (release_stop, wait_for_release) = tokio::sync::oneshot::channel();
    let transcriber = BlockingStopTranscriber::new("hello window", wait_for_release);
    let updates = transcriber.updates.clone();
    let stop_started = transcriber.stop_started.clone();
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
    updates.send("hello").await;

    let stop_task = tokio::spawn(async move {
        controller
            .handle_hotkey(IpcCommand::HotkeyUp)
            .await
            .unwrap();
    });
    stop_started.notified().await;
    wait_for_operations_len(&operations, 3).await;

    assert_eq!(
        *operations.lock().unwrap(),
        vec![
            InjectOperation::Type("💬".to_string()),
            InjectOperation::CursorLeft(1),
            InjectOperation::Type("hello".to_string()),
        ]
    );

    release_stop.send(()).unwrap();
    stop_task.await.unwrap();

    assert_eq!(
        *operations.lock().unwrap(),
        vec![
            InjectOperation::Type("💬".to_string()),
            InjectOperation::CursorLeft(1),
            InjectOperation::Type("hello".to_string()),
            InjectOperation::CursorRight(1),
            InjectOperation::Backspace(1),
            InjectOperation::Type(" window ".to_string()),
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
            InjectOperation::CursorLeft(1),
            InjectOperation::CursorRight(1),
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
        vec![
            InjectOperation::Type("fallback text".to_string()),
            InjectOperation::Type(" ".to_string()),
        ]
    );
}

#[tokio::test]
async fn streaming_can_skip_final_pass_and_keep_last_partial() {
    let transcriber = FakeLiveTranscriber::new(["hello"], Ok("hello window".to_string()));
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
    )
    .with_listening_marker(Some("💬".to_string()))
    .with_final_transcript(false);

    controller
        .handle_hotkey(IpcCommand::HotkeyDown)
        .await
        .unwrap();
    controller
        .handle_hotkey(IpcCommand::HotkeyUp)
        .await
        .unwrap();

    assert_eq!(*stops.lock().unwrap(), 1);
    assert_eq!(
        *operations.lock().unwrap(),
        vec![
            InjectOperation::Type("💬".to_string()),
            InjectOperation::CursorLeft(1),
            InjectOperation::Type("hello".to_string()),
            InjectOperation::CursorRight(1),
            InjectOperation::Backspace(1),
            InjectOperation::Type(" ".to_string()),
        ]
    );
    assert_eq!(
        *transcript_events.lock().unwrap(),
        vec![
            TranscriptNotice::Listening,
            TranscriptNotice::Partial("hello".to_string()),
        ]
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
        vec![
            InjectOperation::Type("fallback text".to_string()),
            InjectOperation::Type(" ".to_string()),
        ]
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
    )
    .with_partial_chunking_config(no_chunking());

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
        vec![
            InjectOperation::Type("partial".to_string()),
            InjectOperation::Type(" ".to_string()),
        ]
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
            InjectOperation::CursorLeft(1),
            InjectOperation::CursorRight(1),
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

    assert!(error.to_string().contains("focused desktop target changed"));
    assert!(operations.lock().unwrap().is_empty());
    assert_eq!(
        *errors.lock().unwrap(),
        vec![(
            DICTATION_ERROR_SUMMARY.to_string(),
            "Final text replacement failed: focused desktop target changed from 1 to 2; aborting replacement".to_string()
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
        vec![
            InjectOperation::Type("hello".to_string()),
            InjectOperation::Type(" ".to_string()),
        ]
    );
    assert_eq!(
        *transcript_events.lock().unwrap(),
        vec![
            TranscriptNotice::Listening,
            TranscriptNotice::Partial("hello".to_string()),
            TranscriptNotice::Final("hello".to_string()),
        ]
    );
}

#[tokio::test]
async fn streaming_trailing_space_can_be_disabled() {
    let transcriber =
        FakeLiveTranscriber::new(["hel", "hello win"], Ok("hello window".to_string()));
    let injector = FakeInjector::default();
    let operations = injector.operations.clone();
    let mut controller = StreamingDictationController::new_with_notifiers(
        transcriber,
        injector,
        FakeTranscriptNotifier::default(),
        FakeErrorNotifier::default(),
    )
    .with_partial_chunking_config(no_chunking())
    .with_append_space(false);

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
            InjectOperation::Type("hel".to_string()),
            InjectOperation::Type("lo win".to_string()),
            InjectOperation::Type("dow".to_string()),
        ]
    );
}

#[tokio::test]
async fn streaming_no_partial_chunking_preserves_immediate_injection() {
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
    .with_partial_chunking_config(no_chunking());

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

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn streaming_partial_chunking_coalesces_character_updates() {
    let transcriber = ManualLiveTranscriber::new(" \n");
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
    updates.send("h").await;
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(40)).await;
    updates.send("he").await;
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(40)).await;
    updates.send("hel").await;
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(79)).await;
    tokio::task::yield_now().await;

    assert!(operations.lock().unwrap().is_empty());

    tokio::time::advance(Duration::from_millis(1)).await;
    wait_for_operations_len(&operations, 1).await;

    assert_eq!(
        *operations.lock().unwrap(),
        vec![InjectOperation::Type("hel".to_string())]
    );
}

#[tokio::test]
async fn streaming_partial_chunking_flushes_immediately_at_word_boundary() {
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
    updates.send("hello ").await;
    wait_for_operations_len(&operations, 1).await;

    assert_eq!(
        *operations.lock().unwrap(),
        vec![InjectOperation::Type("hello".to_string())]
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn streaming_partial_chunking_forces_max_delay_flush() {
    let transcriber = ManualLiveTranscriber::new(" \n");
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
    updates.send("h").await;
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(70)).await;
    updates.send("he").await;
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(70)).await;
    updates.send("hel").await;
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(70)).await;
    updates.send("hell").await;
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(39)).await;
    tokio::task::yield_now().await;

    assert!(operations.lock().unwrap().is_empty());

    tokio::time::advance(Duration::from_millis(1)).await;
    wait_for_operations_len(&operations, 1).await;

    assert_eq!(
        *operations.lock().unwrap(),
        vec![InjectOperation::Type("hell".to_string())]
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

#[derive(Clone)]
struct BlockingStopTranscriber {
    updates: ManualLiveUpdates,
    final_transcript: String,
    stop_started: Arc<tokio::sync::Notify>,
    wait_for_release: Arc<tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>,
}

impl BlockingStopTranscriber {
    fn new(
        final_transcript: impl Into<String>,
        wait_for_release: tokio::sync::oneshot::Receiver<()>,
    ) -> Self {
        Self {
            updates: ManualLiveUpdates::default(),
            final_transcript: final_transcript.into(),
            stop_started: Arc::new(tokio::sync::Notify::new()),
            wait_for_release: Arc::new(tokio::sync::Mutex::new(Some(wait_for_release))),
        }
    }
}

#[async_trait]
impl LiveTranscriber for BlockingStopTranscriber {
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
        self.stop_started.notify_one();
        if let Some(wait_for_release) = self.wait_for_release.lock().await.take() {
            let _ = wait_for_release.await;
        }
        self.updates.sender.lock().unwrap().take();
        Ok(self.final_transcript.clone())
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

fn no_chunking() -> PartialChunkingConfig {
    PartialChunkingConfig {
        enabled: false,
        ..PartialChunkingConfig::default()
    }
}

async fn wait_for_operations_len(
    operations: &Arc<Mutex<Vec<InjectOperation>>>,
    expected_len: usize,
) {
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    loop {
        if operations.lock().unwrap().len() >= expected_len {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {expected_len} inject operations; saw {:?}",
            *operations.lock().unwrap()
        );
        tokio::task::yield_now().await;
    }
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
    CursorLeft(usize),
    CursorRight(usize),
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

    fn move_cursor_left(&self, count: usize) -> anyhow::Result<()> {
        if count > 0 {
            self.operations
                .lock()
                .unwrap()
                .push(InjectOperation::CursorLeft(count));
        }
        Ok(())
    }

    fn move_cursor_right(&self, count: usize) -> anyhow::Result<()> {
        if count > 0 {
            self.operations
                .lock()
                .unwrap()
                .push(InjectOperation::CursorRight(count));
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
