use serde_json::json;
use tempfile::tempdir;
use trec::trace::TraceWriter;

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
    assert!(lines[0].contains("session.created"));
    assert!(lines[1].contains("hello"));
}
