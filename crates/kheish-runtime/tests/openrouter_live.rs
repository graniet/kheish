use anyhow::Result;
use kheish_runtime::{
    AudioTranscriptionRequest, OpenRouterAudioTranscriber, OpenRouterProviderConfig,
    OpenRouterSpeechRequest, OpenRouterSpeechSynthesizer,
};

fn openrouter_api_key() -> Option<String> {
    std::env::var("KHEISH_OPENROUTER_API_KEY")
        .ok()
        .or_else(|| std::env::var("OPENROUTER_API_KEY").ok())
}

#[tokio::test]
async fn openrouter_live_can_generate_and_transcribe_audio() -> Result<()> {
    let Some(api_key) = openrouter_api_key() else {
        eprintln!("Skipping OpenRouter live audio test: no API key environment variable was set.");
        return Ok(());
    };

    let tts = OpenRouterSpeechSynthesizer::new(OpenRouterProviderConfig::new(
        "openai/gpt-4o-mini-tts-2025-12-15",
        api_key.clone(),
    ))?;
    let audio = tts
        .synthesize(&OpenRouterSpeechRequest {
            input: "OpenRouter live audio round trip".to_string(),
            instructions: Some("Speak clearly and neutrally.".to_string()),
            voice: Some("alloy".to_string()),
            response_format: Some("mp3".to_string()),
            speed: Some(1.0),
        })
        .await?;

    assert_eq!(audio.provider, "openrouter");
    assert_eq!(audio.media_type, "audio/mpeg");
    assert!(
        audio.bytes.len() > 1024,
        "unexpectedly small audio response"
    );

    let stt = OpenRouterAudioTranscriber::new(OpenRouterProviderConfig::new(
        "openai/gpt-4o-mini-transcribe",
        api_key,
    ))?;
    let transcript = stt
        .transcribe(&AudioTranscriptionRequest {
            file_name: "openrouter-live.mp3".to_string(),
            media_type: "audio/mpeg".to_string(),
            bytes: audio.bytes,
            prompt: Some("The speaker says: OpenRouter live audio round trip.".to_string()),
            language: Some("en".to_string()),
            timestamp_granularities: Vec::new(),
            diarization: false,
        })
        .await?;

    assert_eq!(transcript.provider, "openrouter");
    let normalized_transcript = transcript
        .text
        .to_ascii_lowercase()
        .split_whitespace()
        .collect::<String>();
    assert!(
        normalized_transcript.contains("openrouterliveaudio"),
        "unexpected transcript: {}",
        transcript.text
    );

    Ok(())
}
