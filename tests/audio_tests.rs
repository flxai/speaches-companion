use speaches_companion::audio::write_pcm_wav;
use tempfile::tempdir;

#[tokio::test]
async fn write_pcm_wav_creates_basic_mono_16bit_wav() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("sample.wav");

    write_pcm_wav(&path, &[1, 2, 3, 4], 16_000).await.unwrap();

    let bytes = tokio::fs::read(path).await.unwrap();
    assert_eq!(&bytes[0..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WAVE");
    assert_eq!(&bytes[12..16], b"fmt ");
    assert_eq!(u16::from_le_bytes([bytes[20], bytes[21]]), 1);
    assert_eq!(u16::from_le_bytes([bytes[22], bytes[23]]), 1);
    assert_eq!(
        u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]),
        16_000
    );
    assert_eq!(&bytes[36..40], b"data");
    assert_eq!(
        u32::from_le_bytes([bytes[40], bytes[41], bytes[42], bytes[43]]),
        4
    );
    assert_eq!(&bytes[44..], &[1, 2, 3, 4]);
}
