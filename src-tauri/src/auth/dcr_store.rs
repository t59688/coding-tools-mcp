use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::platform::platform;

const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredClient {
    pub client_id: String,
    pub redirect_uris: Vec<String>,
    pub token_endpoint_auth_method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret_sha256: Option<String>,
    #[serde(default)]
    pub issued_at: u64,
}

#[derive(Serialize, Deserialize)]
struct StoreFile {
    schema_version: u32,
    clients: Vec<StoredClient>,
}

pub fn store_path(workspace_id: &str, service: &str) -> Option<PathBuf> {
    let workspace_id = sanitize_segment(workspace_id);
    let service = sanitize_segment(service);
    if workspace_id.is_empty() || service.is_empty() {
        return None;
    }
    platform().app_config_dir().ok().map(|root| {
        root.join("oauth-clients")
            .join(workspace_id)
            .join(format!("{service}.json"))
    })
}

pub fn secret_sha256(secret: &str) -> String {
    format!("{:x}", Sha256::digest(secret.as_bytes()))
}

pub fn load(path: &Path) -> HashMap<String, StoredClient> {
    let Ok(bytes) = fs::read(path) else {
        return HashMap::new();
    };
    let Ok(file) = serde_json::from_slice::<StoreFile>(&bytes) else {
        eprintln!("oauth DCR store ignored unreadable file {}", path.display());
        return HashMap::new();
    };
    if file.schema_version > SCHEMA_VERSION {
        eprintln!(
            "oauth DCR store {} has unsupported schema {}",
            path.display(),
            file.schema_version
        );
        return HashMap::new();
    }
    file.clients
        .into_iter()
        .filter(|client| client.client_id.starts_with("dcr-") && !client.redirect_uris.is_empty())
        .map(|client| (client.client_id.clone(), client))
        .collect()
}

pub fn save(path: &Path, clients: &HashMap<String, StoredClient>) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let mut records: Vec<StoredClient> = clients.values().cloned().collect();
    records.sort_by(|left, right| left.client_id.cmp(&right.client_id));
    let payload = serde_json::to_vec_pretty(&StoreFile {
        schema_version: SCHEMA_VERSION,
        clients: records,
    })
    .map_err(|error| error.to_string())?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, payload).map_err(|error| error.to_string())?;
    if path.exists() {
        fs::remove_file(path).map_err(|error| error.to_string())?;
    }
    fs::rename(&tmp, path).map_err(|error| {
        let _ = fs::remove_file(&tmp);
        error.to_string()
    })
}

fn sanitize_segment(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_registered_clients() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mcp.json");
        let mut clients = HashMap::new();
        clients.insert(
            "dcr-abc".into(),
            StoredClient {
                client_id: "dcr-abc".into(),
                redirect_uris: vec!["https://chatgpt.com/connector/oauth/test".into()],
                token_endpoint_auth_method: "none".into(),
                client_secret_sha256: None,
                issued_at: 1,
            },
        );
        save(&path, &clients).expect("save");
        let loaded = load(&path);
        assert_eq!(loaded["dcr-abc"].redirect_uris[0], "https://chatgpt.com/connector/oauth/test");
        assert_eq!(loaded["dcr-abc"].token_endpoint_auth_method, "none");
    }
}
