//! Provider metadata, model choices and provider-native reasoning preferences.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub enum Kind {
    #[default]
    OpenAi,
    Anthropic,
    Gemini,
    Ollama,
    DeepSeek,
}

impl Kind {
    pub const ALL: [Self; 5] = [
        Self::OpenAi,
        Self::Anthropic,
        Self::Gemini,
        Self::Ollama,
        Self::DeepSeek,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::OpenAi => "OpenAI / Compatible",
            Self::Anthropic => "Anthropic",
            Self::Gemini => "Google Gemini",
            Self::Ollama => "Ollama",
            Self::DeepSeek => "DeepSeek",
        }
    }
    pub fn endpoint(self) -> &'static str {
        match self {
            Self::OpenAi => "https://api.openai.com/v1",
            Self::Anthropic => "https://api.anthropic.com",
            Self::Gemini => "https://generativelanguage.googleapis.com",
            Self::Ollama => "http://localhost:11434",
            Self::DeepSeek => "https://api.deepseek.com",
        }
    }

    /// The vendor id Rig's model catalog files this provider's models under.
    pub fn catalog_vendor(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::Gemini => "gcp.gemini",
            Self::Ollama => "ollama",
            Self::DeepSeek => "deepseek",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub enum Reasoning {
    #[default]
    Auto,
    Off,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl Reasoning {
    pub const ALL: [Self; 8] = [
        Self::Auto,
        Self::Off,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Max,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Off => "Off",
            Self::Minimal => "Minimal",
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
            Self::XHigh => "XHigh",
            Self::Max => "Max",
        }
    }
    fn chinese_label(self) -> &'static str {
        match self {
            Self::Auto => "自动",
            Self::Off => "关闭推理",
            Self::Minimal => "极低",
            Self::Low => "低",
            Self::Medium => "中等",
            Self::High => "高",
            Self::XHigh => "极高",
            Self::Max => "最高",
        }
    }
    pub fn display_label(self) -> &'static str {
        crate::i18n::text(self.chinese_label())
    }
    pub fn from_display_label(label: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|level| level.label() == label || level.chinese_label() == label)
    }
    /// Every effort level, offered when a model's own levels are not known.
    pub const FULL: [Self; 7] = [
        Self::Auto,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Max,
    ];
    /// The level's name in Rig's catalog (`off`, `minimal` … `max`); `None` for Auto.
    pub fn catalog_name(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::Off => Some("off"),
            Self::Minimal => Some("minimal"),
            Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High => Some("high"),
            Self::XHigh => Some("xhigh"),
            Self::Max => Some("max"),
        }
    }
}

/// A configured model. Reasoning levels are not stored: they come from Rig's model
/// catalog when the model is chosen. Profiles that still carry the former
/// per-model `reasoning` list load unchanged; the field is dropped on the next save.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Model {
    pub id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Provider {
    pub id: Uuid,
    pub name: String,
    #[serde(default)]
    pub kind: Kind,
    pub endpoint: String,
    pub models: Vec<Model>,
    #[serde(default)]
    pub proxy: Option<Uuid>,
    #[serde(default = "enabled")]
    pub enabled: bool,
}

fn enabled() -> bool {
    true
}

impl Provider {
    pub fn legacy(preferences: &crate::config::Preferences) -> Self {
        Self {
            id: Uuid::nil(),
            name: "OpenAI".into(),
            kind: Kind::OpenAi,
            endpoint: preferences.ai_endpoint.clone(),
            models: vec![Model {
                id: preferences.ai_model.clone(),
            }],
            proxy: None,
            enabled: true,
        }
    }
    pub fn secret_name(&self) -> String {
        if self.id.is_nil() {
            "ai".into()
        } else {
            format!("ai-{}", self.id)
        }
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.name.trim().is_empty() && self.name.len() <= 128,
            "提供方名称无效"
        );
        let endpoint = url::Url::parse(&self.endpoint)?;
        ensure!(
            matches!(endpoint.scheme(), "http" | "https")
                && endpoint.host_str().is_some()
                && endpoint.username().is_empty()
                && endpoint.password().is_none(),
            "API 地址无效，请将密钥保存在独立字段"
        );
        ensure!(
            !self.models.is_empty() && self.models.len() <= 512,
            "至少配置一个模型，最多 512 个"
        );
        let mut seen = std::collections::HashSet::new();
        for model in &self.models {
            ensure!(
                !model.id.trim().is_empty()
                    && model.id.len() <= 256
                    && !model.id.contains(['\r', '\n', '\0'])
                    && seen.insert(&model.id),
                "模型名称为空、重复或无效"
            );
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct Selection {
    pub provider: Provider,
    pub model: String,
    pub reasoning: Reasoning,
    pub proxy: Option<crate::network::Proxy>,
}

impl Selection {
    pub fn validate(&self) -> Result<()> {
        self.provider.validate()?;
        ensure!(self.provider.enabled, "提供方已禁用");
        ensure!(
            self.provider
                .models
                .iter()
                .any(|model| model.id == self.model),
            "模型不属于此提供方"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn localized_reasoning_preserves_protocol_values_and_model_limits() {
        for level in Reasoning::ALL {
            assert_eq!(Reasoning::from_display_label(level.label()), Some(level));
            assert_eq!(
                Reasoning::from_display_label(level.chinese_label()),
                Some(level)
            );
        }
        assert_eq!(Reasoning::from_display_label("unknown"), None);
    }
}
