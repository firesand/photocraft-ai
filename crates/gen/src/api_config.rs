//! Public, non-secret configuration shared by the engine and all frontends.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Protocol {
    GeminiInteractions,
    GeminiGenerateContent,
    Seedream,
    Flux,
    /// OpenAI `POST /v1/images/edits` with a JSON body, also spoken by compatible gateways and
    /// local servers.
    OpenAiImages,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Profile {
    pub protocol: Protocol,
    /// Full request URL; generateContent additionally substitutes `{model}`.
    pub endpoint: String,
    pub model: String,
}

pub const PROVIDERS: [&str; 4] = ["gemini", "seedream", "flux", "openai"];

pub fn preset(provider: &str) -> Option<Profile> {
    match provider {
        "openai" => Some(Profile { protocol: Protocol::OpenAiImages, endpoint: "https://api.openai.com/v1/images/edits".into(), model: "gpt-image-2".into() }),
        "flux" => Some(Profile { protocol: Protocol::Flux, endpoint: "https://api.bfl.ai/v1/{model}".into(), model: "flux-2-pro".into() }),
        "gemini" => Some(Profile {
            protocol: Protocol::GeminiInteractions,
            endpoint: "https://generativelanguage.googleapis.com/v1beta/interactions".into(),
            model: "gemini-nano-banana-2.1".into(),
        }),
        "seedream" => Some(Profile {
            protocol: Protocol::Seedream,
            endpoint: "https://ark.cn-beijing.volces.com/api/v3/images/generations".into(),
            model: "doubao-seedream-5-0-flash-260915".into(),
        }),
        _ => None,
    }
}
