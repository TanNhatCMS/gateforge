// Registry data extracted from the 9router JS registry (data, not hand-typed code).
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Display {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub color: String,
    #[serde(default)]
    pub text_icon: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Transport {
    #[serde(default, rename = "baseUrl")]
    pub base_url: String,
    #[serde(default = "default_format")]
    pub format: String,
    #[serde(default)]
    pub headers: std::collections::HashMap<String, String>,
    #[serde(default, rename = "noAuth")]
    pub no_auth: bool,
    /// Everything else (auth scheme, urlSuffix, quirks…) preserved verbatim.
    #[serde(flatten)]
    pub raw: serde_json::Value,
}

fn default_format() -> String {
    "openai".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct Model {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Provider {
    pub id: String,
    #[serde(default)]
    pub priority: i64,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub display: Option<Display>,
    #[serde(default)]
    pub transport: Option<Transport>,
    #[serde(default)]
    pub models: Vec<Model>,
    /// Every unrecognised registry key, preserved verbatim (never drop data).
    #[serde(flatten)]
    pub raw: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub providers: Vec<Provider>,
}

impl Registry {
    pub fn load_embedded() -> Registry {
        serde_json::from_str(include_str!("../registry.json"))
            .expect("embedded registry.json must parse")
    }

    pub fn get(&self, id_or_alias: &str) -> Option<&Provider> {
        self.providers
            .iter()
            .find(|p| p.id == id_or_alias)
    }

    /// Flattened provider/model pairs for GET /v1/models (llm kinds only).
    pub fn model_entries(&self) -> Vec<(String, &Model)> {
        let mut out = Vec::new();
        for provider in &self.providers {
            for model in &provider.models {
                if let Some(kind) = &model.kind {
                    if kind != "llm" {
                        continue;
                    }
                }
                out.push((provider.id.clone(), model));
            }
        }
        out
    }
}
