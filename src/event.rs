use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RealtimeEvent {
    LiveDelta(String),
    LiveHypothesis(RealtimeHypothesis),
    Completed(String),
    Failed(String),
    Error(String),
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RealtimeHypothesis {
    pub transcript: String,
    pub confirmed_prefix: String,
    pub provisional: String,
}

pub fn classify_event(event: &Value) -> RealtimeEvent {
    let event_type = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("<missing type>");

    match event_type {
        "conversation.item.input_audio_transcription.completed" => {
            RealtimeEvent::Completed(string_field(event, "transcript").unwrap_or_default())
        }
        "conversation.item.input_audio_transcription.failed" => {
            RealtimeEvent::Failed(error_message(event))
        }
        "error" => RealtimeEvent::Error(error_message(event)),
        _ if event_type.contains("input_audio_transcription")
            && event_type.ends_with(".hypothesis")
            && string_field(event, "transcript").is_some() =>
        {
            RealtimeEvent::LiveHypothesis(RealtimeHypothesis {
                transcript: string_field(event, "transcript").unwrap_or_default(),
                confirmed_prefix: string_field(event, "confirmed_prefix").unwrap_or_default(),
                provisional: string_field(event, "provisional").unwrap_or_default(),
            })
        }
        _ if event_type.contains("input_audio_transcription")
            && event_type.ends_with(".delta")
            && string_field(event, "delta").is_some() =>
        {
            RealtimeEvent::LiveDelta(string_field(event, "delta").unwrap_or_default())
        }
        _ => RealtimeEvent::Other(event_type.to_string()),
    }
}

fn string_field(event: &Value, key: &str) -> Option<String> {
    event
        .get(key)
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn error_message(event: &Value) -> String {
    event
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .or_else(|| event.get("message").and_then(Value::as_str))
        .unwrap_or("unknown realtime error")
        .to_string()
}
