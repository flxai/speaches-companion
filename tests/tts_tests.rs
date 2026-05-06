use httpmock::prelude::*;
use speaches_scribe::tts::{
    normalize_read_aloud_text, speech_url, synthesize_speech, SpeechOptions,
};

#[test]
fn speech_url_uses_openai_compatible_audio_speech_path() {
    let url = speech_url("http://ono.tail:8000/base?ignored=true").unwrap();

    assert_eq!(url.as_str(), "http://ono.tail:8000/v1/audio/speech");
}

#[test]
fn read_aloud_text_is_trimmed_and_rejects_empty_input() {
    assert_eq!(
        normalize_read_aloud_text("  read this\n"),
        Some("read this".to_string())
    );
    assert_eq!(normalize_read_aloud_text(" \n\t"), None);
}

#[tokio::test]
async fn synthesize_speech_posts_openai_compatible_json() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/audio/speech")
            .json_body_obj(&serde_json::json!({
                "input": "hello window",
                "model": "tts-1",
                "voice": "lessac",
                "response_format": "wav",
            }));
        then.status(200)
            .header("content-type", "audio/wav")
            .body("RIFF fake wav");
    });
    let options = SpeechOptions {
        model: "tts-1".to_string(),
        voice: "lessac".to_string(),
        response_format: "wav".to_string(),
    };

    let audio = synthesize_speech(&server.base_url(), " hello window ", &options)
        .await
        .unwrap();

    mock.assert();
    assert_eq!(audio, b"RIFF fake wav");
}
