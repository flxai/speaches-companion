use serde_json::json;
use trec::event::{classify_event, RealtimeEvent};
use trec::phase::{PhaseGate, PhaseResult};

#[test]
fn classifies_input_transcription_delta() {
    let event = json!({
        "type": "conversation.item.input_audio_transcription.delta",
        "delta": "hello"
    });

    assert_eq!(
        classify_event(&event),
        RealtimeEvent::LiveDelta("hello".to_string())
    );
}

#[test]
fn classifies_completed_transcription() {
    let event = json!({
        "type": "conversation.item.input_audio_transcription.completed",
        "transcript": "hello world"
    });

    assert_eq!(
        classify_event(&event),
        RealtimeEvent::Completed("hello world".to_string())
    );
}

#[test]
fn classifies_failed_transcription() {
    let event = json!({
        "type": "conversation.item.input_audio_transcription.failed",
        "error": {"message": "model missing"}
    });

    assert_eq!(
        classify_event(&event),
        RealtimeEvent::Failed("model missing".to_string())
    );
}

#[test]
fn phase_passes_only_when_delta_precedes_completion() {
    let mut gate = PhaseGate::default();

    gate.observe(&RealtimeEvent::LiveDelta("hello".to_string()));
    gate.observe(&RealtimeEvent::Completed("hello world".to_string()));

    assert_eq!(gate.result(), PhaseResult::Passed);
}

#[test]
fn phase_blocks_on_completed_only_transcription() {
    let mut gate = PhaseGate::default();

    gate.observe(&RealtimeEvent::Completed("hello world".to_string()));

    assert_eq!(gate.result(), PhaseResult::Blocked);
}

#[test]
fn phase_fails_on_error_or_failed_event() {
    let mut gate = PhaseGate::default();

    gate.observe(&RealtimeEvent::Failed("model missing".to_string()));

    assert_eq!(gate.result(), PhaseResult::Failed);
}
