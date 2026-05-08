use serde_json::json;
use speaches_companion::trace::TraceWriter;
use tempfile::tempdir;

#[tokio::test]
async fn trace_writer_preserves_jsonl_order() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("trace.jsonl");
    let mut writer = TraceWriter::default();

    writer.push(json!({"type": "session.created"}));
    writer.push(
        json!({"type": "conversation.item.input_audio_transcription.delta", "delta": "hello"}),
    );

    writer.write_jsonl(&path).await.expect("write trace");

    let contents = tokio::fs::read_to_string(path).await.expect("read trace");
    let lines = contents.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 2);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(lines[0]).expect("first trace event"),
        json!({"type": "session.created"})
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(lines[1]).expect("second trace event"),
        json!({"type": "conversation.item.input_audio_transcription.delta", "delta": "hello"})
    );
}
