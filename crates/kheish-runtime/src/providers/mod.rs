//! Provider adapters that map semantic Kheish requests onto concrete model APIs.

mod anthropic;
mod attachments;
mod errors;
mod google;
mod openai;
mod openrouter;
mod prompt;
mod sse;
#[cfg(test)]
mod test_fixtures;
#[cfg(test)]
mod testsupport;
mod transcription;
mod xai;

pub use anthropic::{AnthropicPricing, AnthropicProvider, AnthropicProviderConfig};
pub use google::{
    GoogleGeneratedImage, GoogleImageEditInput, GoogleImageEditRequest, GoogleImageEditor,
    GoogleImageGenerationRequest, GoogleImageGenerationResponse, GoogleImageGenerator,
    GoogleImageProviderConfig, GoogleProvider, GoogleProviderConfig, resolve_google_image_model,
    resolve_google_model,
};
pub use openai::{
    OpenAiGeneratedImage, OpenAiImageEditInput, OpenAiImageEditRequest, OpenAiImageEditor,
    OpenAiImageGenerationRequest, OpenAiImageGenerationResponse, OpenAiImageGenerator,
    OpenAiPricing, OpenAiProvider, OpenAiProviderConfig, OpenAiSpeechRequest, OpenAiSpeechResponse,
    OpenAiSpeechSynthesizer, resolve_openai_image_model, resolve_openai_tts_model,
};
pub use openrouter::{
    OpenRouterAudioTranscriber, OpenRouterGeneratedImage, OpenRouterImageEditInput,
    OpenRouterImageEditRequest, OpenRouterImageEditor, OpenRouterImageGenerationRequest,
    OpenRouterImageGenerationResponse, OpenRouterImageGenerator, OpenRouterModelCapabilities,
    OpenRouterProvider, OpenRouterProviderConfig, OpenRouterSpeechRequest,
    OpenRouterSpeechResponse, OpenRouterSpeechSynthesizer, fetch_openrouter_model_capabilities,
    parse_openrouter_model_capabilities, resolve_openrouter_image_model, resolve_openrouter_model,
    resolve_openrouter_transcription_model, resolve_openrouter_tts_model,
};
pub use transcription::{
    AudioTranscriptionRequest, AudioTranscriptionResponse, AudioTranscriptionSegmentTimestamp,
    AudioTranscriptionTimestamps, AudioTranscriptionWordTimestamp, OpenAiAudioTranscriber,
    resolve_openai_transcription_model, resolve_openai_transcription_request_model,
};
pub use xai::{
    XAiImageEditor, XAiImageGenerator, XAiProvider, XAiProviderConfig, resolve_xai_image_model,
    resolve_xai_model,
};
