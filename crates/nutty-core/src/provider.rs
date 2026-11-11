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

    pub fn reasoning_choices(self) -> &'static [Reasoning] {
        match self {
            Self::DeepSeek => &[
                Reasoning::Auto,
                Reasoning::Off,
                Reasoning::Low,
                Reasoning::High,
                Reasoning::Max,
            ],
            _ => &Reasoning::ALL,
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
    pub fn available(configured: &[Self]) -> Vec<Self> {
        let mut levels = vec![Self::Auto];
        for level in configured {
            if !levels.contains(level) {
                levels.push(*level);
            }
        }
        levels
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Model {
    pub id: String,
    #[serde(default)]
    pub reasoning: Vec<Reasoning>,
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
                reasoning: vec![
                    Reasoning::Auto,
                    Reasoning::Low,
                    Reasoning::Medium,
                    Reasoning::High,
                ],
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
        let model = self
            .provider
            .models
            .iter()
            .find(|model| model.id == self.model)
            .ok_or_else(|| anyhow::anyhow!("模型不属于此提供方"))?;
        ensure!(
            self.reasoning == Reasoning::Auto || model.reasoning.contains(&self.reasoning),
            "此模型未启用所选推理强度"
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
        assert_eq!(
            Reasoning::available(&[
                Reasoning::High,
                Reasoning::Auto,
                Reasoning::High,
                Reasoning::Off
            ]),
            [Reasoning::Auto, Reasoning::High, Reasoning::Off]
        );
        assert_eq!(Reasoning::available(&[]), [Reasoning::Auto]);
    }
}
