use std::{
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::RwLock,
};

use anyhow::{Context, Result};
use axum::http::HeaderMap;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::terminal::session::AgentKind;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyPolicy {
    #[serde(default)]
    pub id: String,
    #[serde(default = "default_key_name")]
    pub name: String,
    pub key: String,
    #[serde(default = "default_wildcard_models")]
    pub allowed_models: Vec<String>,
}

fn default_key_name() -> String {
    "default".to_string()
}

fn default_wildcard_models() -> Vec<String> {
    vec!["*".to_string()]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum ApiKeyEntry {
    Policy(ApiKeyPolicy),
    Raw(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSpec {
    pub id: String,
    #[serde(default)]
    pub agent_kind: AgentKind,
    #[serde(default)]
    pub upstream_model: Option<String>,
    pub launch_command: String,
    #[serde(default = "default_model_cwd")]
    pub cwd: PathBuf,
    #[serde(default)]
    pub use_markers: bool,
    #[serde(default = "default_max_sessions")]
    pub max_sessions: usize,
    #[serde(default = "default_idle_ttl_secs")]
    pub idle_ttl_secs: u64,
    #[serde(default = "default_startup_settle_ms")]
    pub startup_settle_ms: u64,
}

fn default_model_cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn default_max_sessions() -> usize {
    4
}

fn default_idle_ttl_secs() -> u64 {
    1800
}

fn default_startup_settle_ms() -> u64 {
    600
}

#[derive(Debug, Clone)]
pub struct AuthenticatedKey {
    pub id: String,
    pub name: String,
    pub allowed_models: Vec<String>,
}

impl AuthenticatedKey {
    pub fn can_access_model(&self, model: &str) -> bool {
        if self.allowed_models.is_empty() {
            return true;
        }
        self.allowed_models
            .iter()
            .any(|allowed| allowed == "*" || allowed == model)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedConfigFile {
    api_keys: Vec<ApiKeyEntry>,
    #[serde(default)]
    models: Vec<ModelSpec>,
    #[serde(default = "default_timeout_secs")]
    request_timeout_secs: u64,
    #[serde(default = "default_idle_quiesce_ms")]
    idle_quiesce_ms: u64,
    #[serde(default)]
    default_session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct NormalizedConfigFile {
    api_keys: Vec<ApiKeyPolicy>,
    models: Vec<ModelSpec>,
    request_timeout_secs: u64,
    idle_quiesce_ms: u64,
    default_session_id: Option<String>,
}

fn default_timeout_secs() -> u64 {
    180
}

fn default_idle_quiesce_ms() -> u64 {
    1500
}

pub struct AppConfig {
    pub listen_addr: SocketAddr,
    pub data_dir: PathBuf,
    pub config_path: PathBuf,
    api_keys: RwLock<Vec<ApiKeyPolicy>>,
    models: RwLock<Vec<ModelSpec>>,
    pub request_timeout_secs: u64,
    pub idle_quiesce_ms: u64,
    pub default_session_id: Option<String>,
    pub open_browser: bool,
}

#[derive(Debug, Default)]
struct CliOverrides {
    host: Option<String>,
    port: Option<u16>,
    api_key: Option<String>,
    data_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
    request_timeout_secs: Option<u64>,
    idle_quiesce_ms: Option<u64>,
    open_browser: bool,
}

impl AppConfig {
    pub fn load() -> Result<Self> {
        let cli = parse_cli_args()?;

        let data_dir = cli
            .data_dir
            .or_else(|| std::env::var_os("GEMINI_PROXY_DATA_DIR").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("data"));
        fs::create_dir_all(&data_dir)
            .with_context(|| format!("failed to create data directory {}", data_dir.display()))?;

        let config_path = cli
            .config_path
            .or_else(|| std::env::var_os("GEMINI_PROXY_CONFIG").map(PathBuf::from))
            .unwrap_or_else(|| data_dir.join("config.json"));

        let persisted = if config_path.is_file() {
            let raw = fs::read_to_string(&config_path)
                .with_context(|| format!("failed to read {}", config_path.display()))?;
            serde_json::from_str::<PersistedConfigFile>(&raw)
                .with_context(|| format!("failed to parse {}", config_path.display()))?
        } else {
            PersistedConfigFile {
                api_keys: Vec::new(),
                models: Vec::new(),
                request_timeout_secs: default_timeout_secs(),
                idle_quiesce_ms: default_idle_quiesce_ms(),
                default_session_id: None,
            }
        };

        let mut policies: Vec<ApiKeyPolicy> = persisted
            .api_keys
            .into_iter()
            .filter_map(|entry| match entry {
                ApiKeyEntry::Policy(mut p) => {
                    let trimmed = p.key.trim().to_string();
                    if trimmed.is_empty() {
                        return None;
                    }
                    p.key = trimmed;
                    if p.id.trim().is_empty() {
                        p.id = derive_key_id(&p.key);
                    }
                    if p.name.trim().is_empty() {
                        p.name = p.id.clone();
                    }
                    Some(p)
                }
                ApiKeyEntry::Raw(raw) => {
                    let trimmed = raw.trim().to_string();
                    if trimmed.is_empty() {
                        return None;
                    }
                    Some(ApiKeyPolicy {
                        id: derive_key_id(&trimmed),
                        name: "default".to_string(),
                        key: trimmed,
                        allowed_models: default_wildcard_models(),
                    })
                }
            })
            .collect();

        let env_api_key = std::env::var("GEMINI_PROXY_API_KEY")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());

        let override_api_key = cli.api_key.or(env_api_key);
        let mut should_save = !config_path.exists();

        if let Some(key) = override_api_key {
            if !policies.iter().any(|existing| existing.key == key) {
                policies.insert(
                    0,
                    ApiKeyPolicy {
                        id: derive_key_id(&key),
                        name: "cli-primary".to_string(),
                        key,
                        allowed_models: default_wildcard_models(),
                    },
                );
                should_save = true;
            }
        }

        if policies.is_empty() {
            let key = generate_api_key();
            policies.push(ApiKeyPolicy {
                id: derive_key_id(&key),
                name: "default".to_string(),
                key,
                allowed_models: default_wildcard_models(),
            });
            should_save = true;
        }

        let request_timeout_secs = cli
            .request_timeout_secs
            .or_else(|| {
                std::env::var("GEMINI_PROXY_TIMEOUT_SECS")
                    .ok()
                    .and_then(|v| v.parse().ok())
            })
            .unwrap_or(persisted.request_timeout_secs)
            .max(5);

        let idle_quiesce_ms = cli
            .idle_quiesce_ms
            .or_else(|| {
                std::env::var("GEMINI_PROXY_IDLE_QUIESCE_MS")
                    .ok()
                    .and_then(|v| v.parse().ok())
            })
            .unwrap_or(persisted.idle_quiesce_ms)
            .max(200);

        let mut models = persisted.models;
        for m in &mut models {
            m.max_sessions = m.max_sessions.max(1);
            m.idle_ttl_secs = m.idle_ttl_secs.max(30);
        }

        if should_save {
            save_normalized_config(
                &config_path,
                &NormalizedConfigFile {
                    api_keys: policies.clone(),
                    models: models.clone(),
                    request_timeout_secs,
                    idle_quiesce_ms,
                    default_session_id: persisted.default_session_id.clone(),
                },
            )?;
        }

        let host = cli
            .host
            .or_else(|| {
                std::env::var("GEMINI_PROXY_HOST")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
            })
            .unwrap_or_else(|| "0.0.0.0".to_string());

        let port = cli
            .port
            .or_else(|| {
                std::env::var("GEMINI_PROXY_PORT")
                    .ok()
                    .and_then(|v| v.parse().ok())
            })
            .unwrap_or(8787);

        let listen_addr: SocketAddr = format!("{host}:{port}")
            .parse()
            .with_context(|| format!("invalid listen address {host}:{port}"))?;

        let open_browser = cli.open_browser
            || matches!(
                std::env::var("GEMINI_PROXY_OPEN_BROWSER")
                    .ok()
                    .as_deref()
                    .map(str::trim),
                Some("1" | "true" | "yes")
            );

        Ok(Self {
            listen_addr,
            data_dir,
            config_path,
            api_keys: RwLock::new(policies),
            models: RwLock::new(models),
            request_timeout_secs,
            idle_quiesce_ms,
            default_session_id: persisted.default_session_id,
            open_browser,
        })
    }

    pub fn authenticate_headers(&self, headers: &HeaderMap) -> Option<AuthenticatedKey> {
        let candidate = extract_api_key_from_headers(headers)?;
        let guard = self.api_keys.read().ok()?;
        authenticate_against_policies(&guard, candidate)
    }

    pub fn primary_api_key_secret(&self) -> Option<String> {
        self.api_keys
            .read()
            .ok()?
            .first()
            .map(|p| p.key.clone())
    }

    pub fn primary_api_key_masked(&self) -> String {
        self.api_keys
            .read()
            .ok()
            .and_then(|g| g.first().map(|p| mask_key(&p.key)))
            .unwrap_or_default()
    }

    pub fn api_key_count(&self) -> usize {
        self.api_keys.read().map(|g| g.len()).unwrap_or(0)
    }

    pub fn api_key_summaries(&self) -> Vec<serde_json::Value> {
        self.api_keys
            .read()
            .map(|g| {
                g.iter()
                    .map(|p| {
                        serde_json::json!({
                            "id": p.id,
                            "name": p.name,
                            "key_masked": mask_key(&p.key),
                            "allowed_models": p.allowed_models,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn list_models(&self) -> Vec<ModelSpec> {
        self.models
            .read()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    pub fn find_model(&self, id: &str) -> Option<ModelSpec> {
        let target = id.trim();
        self.models
            .read()
            .ok()?
            .iter()
            .find(|m| m.id == target)
            .cloned()
    }

    pub fn upsert_model(&self, mut spec: ModelSpec) -> Result<ModelSpec> {
        spec.id = spec.id.trim().to_string();
        anyhow::ensure!(!spec.id.is_empty(), "model id must not be empty");
        spec.launch_command = spec.launch_command.trim().to_string();
        anyhow::ensure!(
            !spec.launch_command.is_empty(),
            "launch_command must not be empty"
        );
        spec.max_sessions = spec.max_sessions.max(1);
        spec.idle_ttl_secs = spec.idle_ttl_secs.max(30);

        {
            let mut guard = self
                .models
                .write()
                .map_err(|_| anyhow::anyhow!("models lock poisoned"))?;
            if let Some(existing) = guard.iter_mut().find(|m| m.id == spec.id) {
                *existing = spec.clone();
            } else {
                guard.push(spec.clone());
            }
        }
        self.persist()?;
        Ok(spec)
    }

    pub fn delete_model(&self, id: &str) -> Result<bool> {
        let removed = {
            let mut guard = self
                .models
                .write()
                .map_err(|_| anyhow::anyhow!("models lock poisoned"))?;
            let before = guard.len();
            guard.retain(|m| m.id != id);
            guard.len() < before
        };
        if removed {
            self.persist()?;
        }
        Ok(removed)
    }

    pub fn upsert_api_key_policy(&self, mut policy: ApiKeyPolicy) -> Result<ApiKeyPolicy> {
        policy.key = policy.key.trim().to_string();
        anyhow::ensure!(!policy.key.is_empty(), "api key must not be empty");
        if policy.id.trim().is_empty() {
            policy.id = derive_key_id(&policy.key);
        }
        if policy.name.trim().is_empty() {
            policy.name = policy.id.clone();
        }
        {
            let mut guard = self
                .api_keys
                .write()
                .map_err(|_| anyhow::anyhow!("api_keys lock poisoned"))?;
            if let Some(existing) = guard.iter_mut().find(|p| p.id == policy.id || p.key == policy.key) {
                *existing = policy.clone();
            } else {
                guard.push(policy.clone());
            }
        }
        self.persist()?;
        Ok(policy)
    }

    fn persist(&self) -> Result<()> {
        let api_keys = self
            .api_keys
            .read()
            .map_err(|_| anyhow::anyhow!("api_keys lock poisoned"))?
            .clone();
        let models = self
            .models
            .read()
            .map_err(|_| anyhow::anyhow!("models lock poisoned"))?
            .clone();
        save_normalized_config(
            &self.config_path,
            &NormalizedConfigFile {
                api_keys,
                models,
                request_timeout_secs: self.request_timeout_secs,
                idle_quiesce_ms: self.idle_quiesce_ms,
                default_session_id: self.default_session_id.clone(),
            },
        )
    }
}

pub fn derive_key_id(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    let hex = hex::encode(digest);
    format!("key_{}", &hex[..8])
}

pub fn extract_api_key_from_headers(headers: &HeaderMap) -> Option<&str> {
    if let Some(auth) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        let trimmed = auth.trim();
        if let Some((scheme, token)) = trimmed.split_once(' ') {
            if scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty() {
                return Some(token.trim());
            }
        }
    }
    if let Some(x_key) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        let trimmed = x_key.trim();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }
    None
}

pub fn authenticate_against_policies(
    policies: &[ApiKeyPolicy],
    candidate: &str,
) -> Option<AuthenticatedKey> {
    let candidate_bytes = candidate.as_bytes();
    let mut matched_idx = None;
    for (idx, policy) in policies.iter().enumerate() {
        let key_bytes = policy.key.as_bytes();
        if key_bytes.len() == candidate_bytes.len() && bool::from(key_bytes.ct_eq(candidate_bytes))
        {
            matched_idx = Some(idx);
        }
    }
    matched_idx.map(|idx| {
        let p = &policies[idx];
        AuthenticatedKey {
            id: p.id.clone(),
            name: p.name.clone(),
            allowed_models: p.allowed_models.clone(),
        }
    })
}

pub fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        return "*".repeat(chars.len());
    }
    let prefix: String = chars[..5].iter().collect();
    let suffix: String = chars[chars.len() - 4..].iter().collect();
    format!("{prefix}...{suffix}")
}

fn generate_api_key() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    let mut out = String::from("gp-");
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn save_normalized_config(path: &Path, config: &NormalizedConfigFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(config)?;
    fs::write(path, json).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn parse_cli_args() -> Result<CliOverrides> {
    let mut out = CliOverrides::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--host" => {
                out.host = Some(args.next().context("--host requires a value")?);
            }
            "--port" => {
                let val = args.next().context("--port requires a value")?;
                out.port = Some(val.parse().context("--port must be a u16")?);
            }
            "--api-key" => {
                out.api_key = Some(args.next().context("--api-key requires a value")?);
            }
            "--data-dir" => {
                out.data_dir = Some(PathBuf::from(
                    args.next().context("--data-dir requires a value")?,
                ));
            }
            "--config" => {
                out.config_path = Some(PathBuf::from(
                    args.next().context("--config requires a value")?,
                ));
            }
            "--timeout-secs" => {
                let val = args.next().context("--timeout-secs requires a value")?;
                out.request_timeout_secs = Some(val.parse().context("--timeout-secs must be u64")?);
            }
            "--idle-quiesce-ms" => {
                let val = args.next().context("--idle-quiesce-ms requires a value")?;
                out.idle_quiesce_ms = Some(val.parse().context("--idle-quiesce-ms must be u64")?);
            }
            "--open" => {
                out.open_browser = true;
            }
            "--help" | "-h" => {
                println!(
                    "gemini-proxy\n\n\
                     Options:\n\
                       --host <HOST>                 Bind host (default 0.0.0.0)\n\
                       --port <PORT>                 Bind port (default 8787)\n\
                       --api-key <KEY>               API key for /v1/* endpoints\n\
                       --data-dir <DIR>              Data directory (default ./data)\n\
                       --config <FILE>               Config JSON path\n\
                       --timeout-secs <SECS>         Default agent turn timeout (default 180)\n\
                       --idle-quiesce-ms <MS>        Idle settle window in ms (default 1500)\n\
                       --open                        Open Web UI in default browser on startup"
                );
                std::process::exit(0);
            }
            other => {
                anyhow::bail!("unknown CLI argument: {other}");
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn extracts_bearer_and_x_api_key_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer gp-secret-123"),
        );
        assert_eq!(extract_api_key_from_headers(&headers), Some("gp-secret-123"));

        let mut x_headers = HeaderMap::new();
        x_headers.insert("x-api-key", HeaderValue::from_static("gp-x-456"));
        assert_eq!(extract_api_key_from_headers(&x_headers), Some("gp-x-456"));
    }

    #[test]
    fn authenticates_policies_and_enforces_model_allowlist() {
        let policies = vec![
            ApiKeyPolicy {
                id: derive_key_id("gp-alpha"),
                name: "alpha".into(),
                key: "gp-alpha".into(),
                allowed_models: vec!["model-a".into()],
            },
            ApiKeyPolicy {
                id: derive_key_id("gp-admin"),
                name: "admin".into(),
                key: "gp-admin".into(),
                allowed_models: vec!["*".into()],
            },
        ];

        let auth_a = authenticate_against_policies(&policies, "gp-alpha").unwrap();
        assert!(auth_a.can_access_model("model-a"));
        assert!(!auth_a.can_access_model("model-b"));

        let auth_admin = authenticate_against_policies(&policies, "gp-admin").unwrap();
        assert!(auth_admin.can_access_model("model-a"));
        assert!(auth_admin.can_access_model("model-b"));

        assert!(authenticate_against_policies(&policies, "gp-wrong").is_none());
    }
}
