//! Bounded ACP configuration snapshots, including legacy model selectors.

use anyhow::{Result, ensure};
use serde_json::{Value, json};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Category {
    Model,
    Reasoning,
    Mode,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Method {
    Configuration,
    Model,
    Mode,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub value: String,
    pub label: String,
    pub group: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Setting {
    pub id: String,
    pub label: String,
    pub category: Category,
    pub current: Value,
    pub choices: Vec<Choice>,
    method: Method,
}

impl Setting {
    pub fn current_label(&self) -> String {
        self.choices
            .iter()
            .find(|choice| self.current.as_str() == Some(&choice.value))
            .map(|choice| choice.label.clone())
            .unwrap_or_else(|| self.current.as_str().unwrap_or_default().to_string())
    }

    pub fn request(&self, session: &str, value: Value) -> Result<(&'static str, Value)> {
        ensure!(
            self.current.is_boolean() && value.is_boolean()
                || value
                    .as_str()
                    .is_some_and(|value| self.choices.iter().any(|c| c.value == value)),
            "Agent 不支持此配置值，请刷新模型列表"
        );
        Ok(match self.method {
            Method::Configuration => (
                "session/set_config_option",
                if value.is_boolean() {
                    json!({"sessionId":session,"configId":self.id,"type":"boolean","value":value})
                } else {
                    json!({"sessionId":session,"configId":self.id,"value":value})
                },
            ),
            Method::Model => (
                "session/set_model",
                json!({"sessionId":session,"modelId":value}),
            ),
            Method::Mode => (
                "session/set_mode",
                json!({"sessionId":session,"modeId":value}),
            ),
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Configuration {
    pub settings: Vec<Setting>,
    revision: u64,
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)?
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 512)
        .map(str::to_string)
}

impl Configuration {
    pub fn from_response(response: &Value) -> Self {
        let mut configuration = Self::default();
        configuration.update(response);
        configuration
    }

    pub fn update(&mut self, response: &Value) {
        let previous = self.settings.clone();
        if let Some(options) = response.get("configOptions").and_then(Value::as_array) {
            self.settings.clear();
            for option in options.iter().take(32) {
                let (Some(id), Some(label)) = (text(option, "id"), text(option, "name")) else {
                    continue;
                };
                if self.settings.iter().any(|setting| setting.id == id) {
                    continue;
                }
                let category = match option
                    .get("category")
                    .and_then(Value::as_str)
                    .unwrap_or(&id)
                {
                    "model" => Category::Model,
                    "thought_level" | "reasoning_effort" | "reasoning" => Category::Reasoning,
                    "mode" => Category::Mode,
                    _ => Category::Other,
                };
                let mut choices = Vec::new();
                if let Some(options) = option.get("options").and_then(Value::as_array) {
                    for value in options.iter().take(512) {
                        let values = value.get("options").and_then(Value::as_array);
                        let group = values.is_some().then(|| text(value, "name")).flatten();
                        let values = values
                            .map(Vec::as_slice)
                            .unwrap_or(std::slice::from_ref(value));
                        for value in values.iter().take(512 - choices.len()) {
                            if let (Some(value), Some(label)) =
                                (text(value, "value"), text(value, "name"))
                                && !choices.iter().any(|c: &Choice| c.value == value)
                            {
                                choices.push(Choice {
                                    value,
                                    label,
                                    group: group.clone(),
                                });
                            }
                        }
                    }
                }
                let Some(current) = option.get("currentValue") else {
                    continue;
                };
                if !(current.is_boolean() || current.as_str().is_some_and(|s| s.len() <= 512)) {
                    continue;
                }
                self.settings.push(Setting {
                    id,
                    label,
                    category,
                    current: current.clone(),
                    choices,
                    method: Method::Configuration,
                });
            }
        }
        for (key, list, current, method, category, id, label) in [
            (
                "models",
                "availableModels",
                "currentModelId",
                Method::Model,
                Category::Model,
                "_legacy_model",
                "模型",
            ),
            (
                "modes",
                "availableModes",
                "currentModeId",
                Method::Mode,
                Category::Mode,
                "_legacy_mode",
                "模式",
            ),
        ] {
            let Some(state) = response.get(key) else {
                continue;
            };
            if self.settings.iter().any(|setting| {
                setting.category == category && setting.method == Method::Configuration
            }) {
                continue;
            }
            let Some(current) = text(state, current) else {
                continue;
            };
            let choices = state
                .get(list)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .take(512)
                .filter_map(|choice| {
                    Some(Choice {
                        value: text(choice, "modelId").or_else(|| text(choice, "id"))?,
                        label: text(choice, "name")?,
                        group: None,
                    })
                })
                .collect();
            self.settings.retain(|setting| setting.id != id);
            self.settings.push(Setting {
                id: id.into(),
                label: label.into(),
                category,
                current: current.into(),
                choices,
                method,
            });
        }
        if self.settings != previous {
            self.revision = self.revision.saturating_add(1);
        }
    }

    pub fn set_current(&mut self, id: &str, value: Value) {
        if let Some(setting) = self.settings.iter_mut().find(|setting| setting.id == id)
            && setting.current != value
        {
            setting.current = value;
            self.revision = self.revision.saturating_add(1);
        }
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn update_notification(&mut self, update: &Value) -> bool {
        let (category, key) = match update.get("sessionUpdate").and_then(Value::as_str) {
            Some("config_option_update") => {
                let previous = self.revision;
                self.update(update);
                return self.revision != previous;
            }
            Some("current_model_update") => (Category::Model, "currentModelId"),
            Some("current_mode_update") => (Category::Mode, "currentModeId"),
            _ => return false,
        };
        if let Some(value) = text(update, key)
            && let Some(setting) = self.settings.iter_mut().find(|s| s.category == category)
        {
            if setting.current.as_str() == Some(&value) {
                return false;
            }
            setting.current = value.into();
            self.revision = self.revision.saturating_add(1);
            return true;
        }
        false
    }

    pub fn model(&self) -> Option<&Setting> {
        self.settings
            .iter()
            .find(|setting| setting.category == Category::Model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modern_grouped_models_and_reasoning_use_server_values() -> Result<()> {
        let response = json!({"configOptions":[
            {"id":"model","name":"Model","category":"model","type":"select","currentValue":"auto","options":[{"group":"cursor","name":"Cursor Models","options":[{"value":"auto","name":"Auto"},{"value":"grok-4.7","name":"Grok 4.7"}]}]},
            {"id":"effort","name":"Thinking","category":"thought_level","type":"select","currentValue":"high","options":[{"value":"high","name":"High"}]}
        ]});
        let configuration = Configuration::from_response(&response);
        let model = configuration.model().expect("fixture has model options");
        assert_eq!(model.choices[1].group.as_deref(), Some("Cursor Models"));
        assert_eq!(model.current_label(), "Auto");
        let (method, params) = model.request("session", "grok-4.7".into())?;
        assert_eq!(method, "session/set_config_option");
        assert_eq!(params["value"], "grok-4.7");
        assert!(model.request("session", "invented".into()).is_err());
        assert_eq!(configuration.settings[1].category, Category::Reasoning);
        Ok(())
    }

    #[test]
    fn legacy_models_are_preserved_and_modern_options_replace_them() -> Result<()> {
        let mut configuration = Configuration::from_response(
            &json!({"models":{"currentModelId":"auto","availableModels":[{"id":"auto","name":"Auto"},{"id":"grok-4.7","name":"Grok 4.7"}]}}),
        );
        let (method, params) = configuration
            .model()
            .expect("legacy model")
            .request("session", "auto".into())?;
        assert_eq!(method, "session/set_model");
        assert_eq!(params["modelId"], "auto");
        configuration.update(&json!({"configOptions":[]}));
        assert!(
            configuration.model().is_none(),
            "empty authoritative updates clear stale models"
        );
        Ok(())
    }

    #[test]
    fn modern_controls_replace_legacy_duplicates_and_follow_notifications() {
        let mut configuration = Configuration::from_response(&json!({
            "configOptions":[
                {"id":"model","name":"Model","category":"model","currentValue":"auto-smart","options":[{"value":"auto-smart","name":"Auto"}]},
                {"id":"mode","name":"Mode","category":"mode","currentValue":"agent","options":[{"value":"agent","name":"Agent"},{"value":"ask","name":"Ask"}]}
            ],
            "models":{"currentModelId":"auto-smart","availableModels":[{"modelId":"auto-smart","name":"Auto"}]},
            "modes":{"currentModeId":"agent","availableModes":[{"id":"agent","name":"Agent"}]}
        }));
        assert_eq!(
            configuration.settings.len(),
            2,
            "one control per advertised category"
        );
        assert!(
            configuration
                .settings
                .iter()
                .all(|s| s.method == Method::Configuration)
        );
        let update = json!({"sessionUpdate":"current_mode_update","currentModeId":"ask"});
        assert!(configuration.update_notification(&update));
        assert_eq!(configuration.settings[1].current_label(), "Ask");
        assert!(
            !configuration.update_notification(&update),
            "unchanged notifications do not refresh UI"
        );
        assert!(
            !configuration.update_notification(&json!({"sessionUpdate":"agent_message_chunk"}))
        );
    }

    #[test]
    fn legacy_updates_refresh_models_and_modes_without_duplicates() -> Result<()> {
        let mut configuration = Configuration::from_response(&json!({
            "models":{"currentModelId":"auto","availableModels":[{"modelId":"auto","name":"Auto"}]},
            "modes":{"currentModeId":"agent","availableModes":[{"id":"agent","name":"Agent"},{"id":"ask","name":"Ask"}]}
        }));
        configuration.update(&json!({"models":{"currentModelId":"grok-4.7","availableModels":[{"modelId":"auto","name":"Auto"},{"modelId":"grok-4.7","name":"Grok 4.7"}]}}));
        assert_eq!(configuration.settings.len(), 2);
        assert_eq!(
            configuration
                .model()
                .expect("model refreshed")
                .current_label(),
            "Grok 4.7"
        );
        assert!(configuration.update_notification(
            &json!({"sessionUpdate":"current_mode_update","currentModeId":"ask"})
        ));
        let mode = configuration
            .settings
            .iter()
            .find(|s| s.category == Category::Mode)
            .expect("legacy mode");
        assert_eq!(mode.current_label(), "Ask");
        let (method, params) = mode.request("session", "agent".into())?;
        assert_eq!(method, "session/set_mode");
        assert_eq!(params["modeId"], "agent");
        Ok(())
    }
}
