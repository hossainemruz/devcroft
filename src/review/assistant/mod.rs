//! User-visible author provider preferences.
use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum GenerationProvider {
    #[default]
    Codex,
    Claude,
    Opencode,
    Omp,
}
impl GenerationProvider {
    pub fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
            Self::Opencode => "OpenCode",
            Self::Omp => "Omp",
        }
    }
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub(crate) enum Effort {
    #[default]
    Default,
    Low,
    Medium,
    High,
    Xhigh,
}
impl Effort {
    pub fn argument(self) -> Option<&'static str> {
        match self {
            Self::Default => None,
            Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High => Some("high"),
            Self::Xhigh => Some("xhigh"),
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct GenerationOptions {
    pub provider: GenerationProvider,
    pub model: String,
    pub effort: Effort,
}
impl GenerationOptions {
    pub fn select_provider(&mut self, provider: GenerationProvider) {
        if self.provider != provider {
            self.provider = provider;
            self.model.clear();
            self.effort = Effort::Default;
        }
    }
}
pub(crate) fn generation_options() -> GenerationOptions {
    crate::data::resolve_data_root()
        .ok()
        .and_then(|root| crate::data::DeviceStore::new(&root).load().ok())
        .and_then(|state| state.extra.get("review_assistant").cloned())
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default()
}
