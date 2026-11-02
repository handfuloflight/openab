use fs2::FileExt;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use uuid::Uuid;

use crate::mcp_plugins::{McpError, PluginStore};
use crate::types::*;

const DEFAULT_PRINT_TIMEOUT: &str = "20m";

fn model_parts(model: &str) -> (&str, &str) {
    let (value, name) = model.split_once('\t').unwrap_or((model, model));
    (value.trim(), name.trim())
}

fn resolve_model_id<'a>(models: &'a [String], model: &'a str) -> &'a str {
    let value = model_parts(model).0;
    let mut parts = models.iter().map(|row| model_parts(row));
    parts
        .clone()
        .find(|(id, _)| *id == value)
        .or_else(|| parts.find(|(_, name)| *name == value))
        .map(|(id, _)| id)
        .unwrap_or(value)
}

fn prompt_extra_args(extra: &str) -> Vec<String> {
    let mut args = shell_words::split(extra).unwrap_or_else(|_| {
        eprintln!("[agy-acp] WARN: failed to parse AGY_EXTRA_ARGS, ignoring");
        Vec::new()
    });
    if !args
        .iter()
        .any(|arg| arg == "--print-timeout" || arg.starts_with("--print-timeout="))
    {
        args.push("--print-timeout".to_string());
        args.push(DEFAULT_PRINT_TIMEOUT.to_string());
    }
    args
}

pub struct Adapter {
    pub sessions: HashMap<String, Session>,
    pub working_dir: String,
    pub conversations_dir: PathBuf,
    pub state_file: PathBuf,
    pub available_models: Option<Vec<String>>,
    pub mcp: PluginStore,
}

impl Adapter {
    pub fn new() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        let state_dir = PathBuf::from(&home).join(".openab/agy-acp");
        Self {
            sessions: HashMap::new(),
            working_dir: std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| "/tmp".to_string()),
            conversations_dir: PathBuf::from(&home).join(".gemini/antigravity-cli/conversations"),
            state_file: state_dir.join("sessions.json"),
            available_models: None,
            mcp: PluginStore::new(PathBuf::from(&home)),
        }
    }

    // --- Model cache ---

    pub fn models_cache_path(&self) -> PathBuf {
        self.state_file.with_file_name("models_cache.json")
    }

    pub fn load_cached_models(&self) -> Option<Vec<String>> {
        let path = self.models_cache_path();
        let content = fs::read_to_string(&path).ok()?;
        serde_json::from_str::<Vec<String>>(&content).ok().filter(|v| !v.is_empty())
    }

    pub fn save_models_cache(&self, models: &[String]) {
        if let Some(parent) = self.models_cache_path().parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string(models) {
            let tmp = self.models_cache_path().with_extension("tmp");
            if fs::write(&tmp, &json).is_ok() {
                let _ = fs::rename(&tmp, self.models_cache_path());
            }
        }
    }

    pub fn static_fallback_models() -> Vec<String> {
        vec![
            "Gemini 3.5 Flash (Medium)".to_string(),
            "Gemini 3.5 Flash (High)".to_string(),
            "Gemini 3.5 Flash (Low)".to_string(),
            "Gemini 3.1 Pro (Low)".to_string(),
            "Gemini 3.1 Pro (High)".to_string(),
        ]
    }

    /// Resolve the `agy` binary path.
    pub fn agy_bin() -> &'static str {
        "/usr/local/bin/agy"
    }

    /// Build PATH with common agent binary locations prepended.
    pub fn augmented_path() -> String {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/agent".to_string());
        let base = std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".to_string());
        format!("{home}/bin:{home}/.local/bin:{home}/.local/share/fnm/aliases/default/bin:{base}")
    }

    pub fn fetch_available_models() -> Vec<String> {
        std::process::Command::new(Self::agy_bin())
            .arg("models")
            .env("PATH", Self::augmented_path())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn get_available_models(&mut self) -> &[String] {
        if self.available_models.is_none() {
            let models = Self::fetch_available_models();
            if !models.is_empty() {
                eprintln!("[agy-acp] fetched {} models from `agy models`, updating cache", models.len());
                self.save_models_cache(&models);
                self.available_models = Some(models);
            } else if let Some(cached) = self.load_cached_models() {
                eprintln!("[agy-acp] `agy models` failed, using cached model list ({} models)", cached.len());
                self.available_models = Some(cached);
            } else {
                eprintln!("[agy-acp] `agy models` failed and no cache found, using hardcoded fallback");
                self.available_models = Some(Self::static_fallback_models());
            }
        }
        self.available_models.as_ref().unwrap()
    }

    pub fn config_options_json(&mut self, model_id: Option<&str>) -> Value {
        let models = self.get_available_models();
        if models.is_empty() {
            return json!([]);
        }
        let current = model_id
            .or_else(|| models.first().map(|s| s.as_str()))
            .map(|model| resolve_model_id(models, model))
            .unwrap_or("");
        let options: Vec<Value> = models
            .iter()
            .map(|model| {
                let (value, name) = model_parts(model);
                json!({ "value": value, "name": name })
            })
            .collect();
        json!([{
            "id": "model",
            "name": "Model",
            "category": "model",
            "type": "select",
            "currentValue": current,
            "options": options,
        }])
    }

    // --- State persistence ---

    fn lock_state_file(&self) -> Option<fs::File> {
        if let Some(parent) = self.state_file.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let lock_path = self.state_file.with_extension("lock");
        let lock_file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .ok()?;
        lock_file.lock_exclusive().ok()?;
        Some(lock_file)
    }

    fn load_store_inner(&self) -> SessionStore {
        let Some(file) = fs::File::open(&self.state_file).ok() else {
            return SessionStore::default();
        };
        serde_json::from_reader(&file).unwrap_or_default()
    }

    pub fn load_store(&self) -> SessionStore {
        let _lock = self.lock_state_file();
        self.load_store_inner()
    }

    pub fn restore_session(&self, session_id: &str) -> Option<(String, i64, Option<String>)> {
        let store = self.load_store();
        store.sessions.get(session_id).and_then(|s| {
            s.conversation_id.clone().map(|cid| (cid, s.last_step_idx, s.model_id.clone()))
        })
    }

    pub fn persist_session(&self, session_id: &str, conversation_id: Option<&str>, last_step_idx: i64, model_id: Option<&str>) {
        let Some(_lock) = self.lock_state_file() else { return; };
        let mut store = self.load_store_inner();
        store.sessions.insert(
            session_id.to_string(),
            StoredSession {
                conversation_id: conversation_id.map(String::from),
                last_step_idx,
                model_id: model_id.map(String::from),
            },
        );
        let tmp = self.state_file.with_extension("tmp");
        if let Ok(file) = fs::File::create(&tmp) {
            if serde_json::to_writer_pretty(&file, &store).is_ok() {
                let _ = fs::rename(&tmp, &self.state_file);
            }
        }
    }

    // --- Conversation snapshot ---

    pub fn conversation_snapshot(&self) -> HashSet<String> {
        let Ok(entries) = fs::read_dir(&self.conversations_dir) else {
            return HashSet::new();
        };
        entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let path = e.path();
                if path.extension().map(|x| x == "db").unwrap_or(false) {
                    path.file_stem().map(|s| s.to_string_lossy().to_string())
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn new_conversation_id(&self, before: &HashSet<String>) -> Option<String> {
        let after = self.conversation_snapshot();
        let mut created: Vec<_> = after.difference(before).collect();
        if created.is_empty() { return None; }
        if created.len() > 1 {
            eprintln!("[agy-acp] WARN: multiple new agy conversation files appeared; refusing to bind");
            return None;
        }
        Some(created.remove(0).clone())
    }

    // --- Session management ---

    pub fn evict_if_needed(&mut self) {
        const MAX_SESSIONS: usize = 64;
        while self.sessions.len() >= MAX_SESSIONS {
            if let Some(key) = self.sessions.keys().next().cloned() {
                self.sessions.remove(&key);
                self.mcp.remove_session(&key);
            } else {
                break;
            }
        }
    }

    pub fn restore_session_state(&mut self, session_id: &str) -> bool {
        let Some((conversation_id, last_step_idx, model_id)) = self.restore_session(session_id) else {
            return false;
        };
        if !self.sessions.contains_key(session_id) {
            self.evict_if_needed();
        }
        self.sessions.insert(
            session_id.to_string(),
            Session { conversation_id: Some(conversation_id), last_step_idx, model_id },
        );
        true
    }

    // --- JSON-RPC handlers ---

    pub fn handle_initialize(&self, id: Value) -> JsonRpcResponse {
        JsonRpcResponse {
            jsonrpc: "2.0",
            id,
            result: Some(json!({
                "protocolVersion": 1,
                "agentInfo": { "name": "agy", "version": env!("CARGO_PKG_VERSION") },
                "agentCapabilities": {
                    "streaming": true,
                    "loadSession": true,
                    "mcpCapabilities": { "http": true, "sse": false },
                },
            })),
            error: None,
        }
    }

    pub fn handle_session_new(&mut self, id: Value, params: &Value) -> JsonRpcResponse {
        let session_id = Uuid::new_v4().to_string();
        if let Err(err) = self.apply_mcp_servers(&session_id, params) {
            return mcp_error_response(id, err);
        }
        self.evict_if_needed();
        self.sessions.insert(session_id.clone(), Session {
            conversation_id: None, last_step_idx: -1, model_id: None,
        });
        let config_options = self.config_options_json(None);
        JsonRpcResponse {
            jsonrpc: "2.0",
            id,
            result: Some(json!({ "sessionId": session_id, "configOptions": config_options })),
            error: None,
        }
    }

    pub fn handle_session_load(&mut self, id: Value, params: &Value) -> JsonRpcResponse {
        let session_id = params.get("sessionId").and_then(|v| v.as_str()).unwrap_or("");
        if session_id.is_empty() {
            return JsonRpcResponse { jsonrpc: "2.0", id, result: None,
                error: Some(json!({"code":-32602,"message":"missing sessionId"})) };
        }
        if self.restore_session_state(session_id) {
            if let Err(err) = self.apply_mcp_servers(session_id, params) {
                return mcp_error_response(id, err);
            }
            let model_id = self.sessions.get(session_id).and_then(|s| s.model_id.clone());
            let config_options = self.config_options_json(model_id.as_deref());
            return JsonRpcResponse { jsonrpc: "2.0", id,
                result: Some(json!({ "sessionId": session_id, "configOptions": config_options })), error: None };
        }
        JsonRpcResponse { jsonrpc: "2.0", id, result: None,
            error: Some(json!({"code":-32000,"message":format!("unknown sessionId: {session_id}")})) }
    }

    pub fn handle_session_set_config_option(&mut self, id: Value, params: &Value) -> JsonRpcResponse {
        let session_id = params.get("sessionId").and_then(|v| v.as_str()).unwrap_or("");
        let config_id = params.get("configId").and_then(|v| v.as_str()).unwrap_or("");
        let value = params.get("value").and_then(|v| v.as_str()).unwrap_or("");
        let value = model_parts(value).0;

        if session_id.is_empty() || config_id != "model" || value.is_empty() {
            return JsonRpcResponse { jsonrpc: "2.0", id, result: None,
                error: Some(json!({"code":-32602,"message":"missing sessionId, configId, or value"})) };
        }
        if !self.sessions.contains_key(session_id) {
            let _ = self.restore_session_state(session_id);
        }
        let value = resolve_model_id(self.get_available_models(), value).to_string();
        let Some(session) = self.sessions.get_mut(session_id) else {
            return JsonRpcResponse { jsonrpc: "2.0", id, result: None,
                error: Some(json!({"code":-32000,"message":format!("unknown sessionId: {session_id}")})) };
        };
        session.model_id = Some(value.clone());
        let conv_id = session.conversation_id.clone();
        let last_step_idx = session.last_step_idx;
        self.persist_session(session_id, conv_id.as_deref(), last_step_idx, Some(&value));
        let config_options = self.config_options_json(Some(&value));
        JsonRpcResponse { jsonrpc: "2.0", id, result: Some(json!({ "configOptions": config_options })), error: None }
    }

    pub fn handle_session_close(&mut self, id: Value, params: &Value) -> JsonRpcResponse {
        let session_id = params.get("sessionId").and_then(|v| v.as_str()).unwrap_or("");
        if session_id.is_empty() {
            return JsonRpcResponse { jsonrpc: "2.0", id, result: None,
                error: Some(json!({"code": -32602, "message": "missing sessionId"})) };
        }
        let known = self.sessions.remove(session_id).is_some();
        self.mcp.remove_session(session_id);
        if !known {
            return JsonRpcResponse { jsonrpc: "2.0", id, result: None,
                error: Some(json!({"code": -32000, "message": format!("unknown sessionId: {session_id}")})) };
        }
        JsonRpcResponse { jsonrpc: "2.0", id, result: Some(json!({})), error: None }
    }

    fn apply_mcp_servers(&mut self, session_id: &str, params: &Value) -> Result<(), McpError> {
        let servers = match params.get("mcpServers") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => items.clone(),
            Some(_) => return Err(McpError::invalid("mcpServers must be an array")),
        };
        self.mcp.sync(session_id, &servers).map(|_| ())
    }

    /// Gather session state needed for prompt execution (under lock).
    pub fn prepare_prompt_state(
        &mut self,
        params: &Value,
    ) -> (String, String, Vec<String>, Option<HashSet<String>>, Option<String>, i64) {
        let session_id = params.get("sessionId").and_then(|v| v.as_str()).unwrap_or("").to_string();

        if !session_id.is_empty() && !self.sessions.contains_key(&session_id) {
            let _ = self.restore_session_state(&session_id);
        }

        let prompt_text = params
            .get("prompt")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|b| b.get("text").and_then(|t| t.as_str())).collect::<Vec<_>>().join("\n"))
            .unwrap_or_default();
        let clean_prompt = prompt_text.trim().to_string();

        let snapshot = if self.sessions.get(&session_id).map(|s| s.conversation_id.is_none()).unwrap_or(false) {
            Some(self.conversation_snapshot())
        } else {
            None
        };

        let mut args: Vec<String> = Vec::new();
        args.push("--add-dir".to_string());
        args.push(self.working_dir.clone());
        args.extend(prompt_extra_args(&std::env::var("AGY_EXTRA_ARGS").unwrap_or_default()));
        let model_id = self.sessions.get(&session_id).and_then(|session| session.model_id.clone());
        if let Some(model_id) = model_id {
            args.push("--model".to_string());
            // Resolve persisted display names as well as complete rows from older adapters.
            args.push(resolve_model_id(self.get_available_models(), &model_id).to_string());
        }
        if let Some(session) = self.sessions.get(&session_id) {
            if let Some(conv_id) = &session.conversation_id {
                args.push("--conversation".to_string());
                args.push(conv_id.clone());
            }
        }
        args.push("-p".to_string());
        args.push(clean_prompt.clone());

        let initial_conv_id = self.sessions.get(&session_id).and_then(|s| s.conversation_id.clone());
        let initial_step_idx = self.sessions.get(&session_id).map(|s| s.last_step_idx).unwrap_or(-1);

        (session_id, clean_prompt, args, snapshot, initial_conv_id, initial_step_idx)
    }
}

impl Drop for Adapter {
    fn drop(&mut self) {
        self.mcp.remove_all();
    }
}

fn mcp_error_response(id: Value, err: McpError) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(json!({ "code": err.code(), "message": err.message() })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Removes the temp-dir fixture on drop, so cleanup still runs when an
    /// assertion panics mid-test.
    struct TempDirGuard(PathBuf);
    impl std::ops::Deref for TempDirGuard {
        type Target = std::path::Path;
        fn deref(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    #[ignore]
    fn model_options_separate_ids_and_names_from_live_or_cached_rows() {
        let row = "gemini-3.8-flash-high\tGemini 3.8 Flash (High)";
        let legacy = "Gemini 3.1 Pro (High)";
        let root = TempDirGuard(std::env::temp_dir().join(format!("agy-acp-models-{}", Uuid::new_v4())));
        let mut adapter = Adapter {
            sessions: HashMap::new(),
            working_dir: root.to_string_lossy().to_string(),
            conversations_dir: root.join("conversations"),
            state_file: root.join("sessions.json"),
            available_models: Some(vec![row.to_string(), legacy.to_string()]),
            mcp: PluginStore::detached(root.to_path_buf()),
        };
        adapter.save_models_cache(adapter.available_models.as_ref().unwrap());
        adapter.available_models = adapter.load_cached_models();

        for current in [
            None,
            Some(row),
            Some("gemini-3.8-flash-high"),
            Some("Gemini 3.8 Flash (High)"),
        ] {
            let config = adapter.config_options_json(current);
            assert_eq!(config[0]["currentValue"], "gemini-3.8-flash-high");
            assert_eq!(
                config[0]["options"],
                json!([
                    {"value": "gemini-3.8-flash-high", "name": "Gemini 3.8 Flash (High)"},
                    {"value": legacy, "name": legacy},
                ])
            );
        }
        assert_eq!(
            adapter.config_options_json(Some(legacy))[0]["currentValue"],
            legacy
        );
        adapter.available_models = Some(Adapter::static_fallback_models());
        let config = adapter.config_options_json(Some("Gemini 3.5 Flash (Medium)"));
        assert_eq!(config[0]["currentValue"], "Gemini 3.5 Flash (Medium)");
        for option in config[0]["options"].as_array().unwrap() {
            assert_eq!(option["value"], option["name"]);
        }
    }

    #[test]
    #[ignore]
    fn model_selection_and_restored_legacy_rows_use_only_the_id_in_argv() {
        let row = "gemini-3.8-flash-high\tGemini 3.8 Flash (High)";
        let root = TempDirGuard(std::env::temp_dir().join(format!("agy-acp-model-selection-{}", Uuid::new_v4())));
        let mut adapter = Adapter {
            sessions: HashMap::new(),
            working_dir: root.to_string_lossy().to_string(),
            conversations_dir: root.join("conversations"),
            state_file: root.join("sessions.json"),
            available_models: Some(vec![row.to_string()]),
            mcp: PluginStore::detached(root.to_path_buf()),
        };
        let created = adapter.handle_session_new(json!(1), &json!({}));
        let session_id = created.result.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();
        for value in ["gemini-3.8-flash-high", row, "Gemini 3.8 Flash (High)"] {
            let response = adapter.handle_session_set_config_option(
                json!(2),
                &json!({
                    "sessionId": session_id, "configId": "model", "value": value,
                }),
            );
            assert!(response.error.is_none());
            assert_eq!(
                response.result.unwrap()["configOptions"][0]["currentValue"],
                "gemini-3.8-flash-high"
            );
            assert_eq!(
                adapter.sessions[&session_id].model_id.as_deref(),
                Some("gemini-3.8-flash-high")
            );
            assert_eq!(
                adapter.load_store().sessions[&session_id]
                    .model_id
                    .as_deref(),
                Some("gemini-3.8-flash-high")
            );
        }

        // Simulate state persisted before this fix, without rewriting conversation history.
        for value in [row, "Gemini 3.8 Flash (High)"] {
            adapter.persist_session(&session_id, Some("existing-conversation"), 7, Some(value));
            adapter.sessions.clear();
            let loaded = adapter.handle_session_load(json!(3), &json!({"sessionId": session_id}));
            assert!(loaded.error.is_none());
            assert_eq!(
                loaded.result.unwrap()["configOptions"][0]["currentValue"],
                "gemini-3.8-flash-high"
            );
            let (_, _, args, _, conversation, step) = adapter.prepare_prompt_state(&json!({
                "sessionId": session_id, "prompt": [{"type": "text", "text": "hello"}],
            }));
            let model_index = args.iter().position(|arg| arg == "--model").unwrap();
            assert_eq!(args[model_index + 1], "gemini-3.8-flash-high");
            assert!(!args.iter().any(|arg| arg.contains('\t')));
            assert_eq!(conversation.as_deref(), Some("existing-conversation"));
            assert_eq!(step, 7);
        }

        for value in ["custom-model", "Unknown Model (High)"] {
            let response = adapter.handle_session_set_config_option(
                json!(4),
                &json!({"sessionId": session_id, "configId": "model", "value": value}),
            );
            assert!(response.error.is_none());
            assert_eq!(
                adapter.sessions[&session_id].model_id.as_deref(),
                Some(value)
            );
            assert_eq!(
                response.result.unwrap()["configOptions"][0]["currentValue"],
                value
            );
        }
    }

    #[test]
    fn default_timeout_is_added_without_discarding_extra_args() {
        assert_eq!(prompt_extra_args(""), ["--print-timeout", "20m"]);
        assert_eq!(
            prompt_extra_args("--model 'model with spaces'"),
            ["--model", "model with spaces", "--print-timeout", "20m"]
        );
    }

    #[test]
    fn explicit_timeout_is_preserved_in_both_forms() {
        assert_eq!(
            prompt_extra_args("--print-timeout 5m"),
            ["--print-timeout", "5m"]
        );
        assert_eq!(
            prompt_extra_args("--print-timeout=30m"),
            ["--print-timeout=30m"]
        );
    }

    #[test]
    fn invalid_explicit_timeouts_are_left_for_cli_validation() {
        for extra in [
            "--print-timeout",
            "--print-timeout=",
            "--print-timeout ''",
            "--print-timeout --model example",
            "--print-timeout=invalid",
        ] {
            assert_eq!(prompt_extra_args(extra), shell_words::split(extra).unwrap());
        }
    }

    #[test]
    fn similar_flag_does_not_suppress_default() {
        assert_eq!(
            prompt_extra_args("--print-timeout-other 1s"),
            ["--print-timeout-other", "1s", "--print-timeout", "20m"]
        );
    }

    #[test]
    fn malformed_extra_args_still_get_default_timeout() {
        assert_eq!(
            prompt_extra_args("--print-timeout '5m"),
            ["--print-timeout", "20m"]
        );
    }

    fn adapter_at(home: &std::path::Path) -> Adapter {
        Adapter {
            sessions: HashMap::new(),
            working_dir: home.to_string_lossy().to_string(),
            conversations_dir: home.join("conversations"),
            state_file: home.join("sessions.json"),
            available_models: Some(vec![]),
            mcp: PluginStore::new(home.to_path_buf()),
        }
    }

    #[test]
    fn session_new_writes_plugin_and_close_removes_it() {
        let root = TempDirGuard(std::env::temp_dir().join(format!("agy-acp-wire-{}", Uuid::new_v4())));
        fs::create_dir_all(&*root).unwrap();
        let mut adapter = adapter_at(&root);
        let params = json!({
            "mcpServers": [
                {
                    "name": "conduit",
                    "command": "/bin/true",
                    "args": ["--flag"],
                    "env": [{"name": "TOKEN", "value": "env-secret-value"}]
                },
                {
                    "type": "http",
                    "name": "remote",
                    "url": "http://127.0.0.1:9/mcp",
                    "headers": [{"name": "Authorization", "value": "header-secret-value"}]
                }
            ]
        });
        let created = adapter.handle_session_new(json!(1), &params);
        assert!(created.error.is_none(), "{:?}", created.error);
        let session_id = created.result.unwrap()["sessionId"].as_str().unwrap().to_string();
        let dir = adapter.mcp.dir_for(&session_id).unwrap().to_path_buf();
        let config: Value = serde_json::from_str(&fs::read_to_string(dir.join("mcp_config.json")).unwrap()).unwrap();
        assert_eq!(config["mcpServers"]["conduit"]["env"]["TOKEN"], "env-secret-value");
        assert_eq!(config["mcpServers"]["remote"]["headers"]["Authorization"], "header-secret-value");
        let manifest: Value = serde_json::from_str(&fs::read_to_string(dir.join("plugin.json")).unwrap()).unwrap();
        assert_eq!(manifest["name"], dir.file_name().unwrap().to_str().unwrap());

        adapter.persist_session(&session_id, Some("conv-1"), 3, None);
        adapter.sessions.clear();
        let loaded = adapter.handle_session_load(json!(2), &json!({
            "sessionId": session_id,
            "mcpServers": [{"name": "conduit", "command": "/bin/true", "args": []}]
        }));
        assert!(loaded.error.is_none(), "{:?}", loaded.error);
        let config: Value = serde_json::from_str(&fs::read_to_string(dir.join("mcp_config.json")).unwrap()).unwrap();
        assert!(config["mcpServers"].get("remote").is_none());
        assert_eq!(config["mcpServers"]["conduit"]["command"], "/bin/true");

        let closed = adapter.handle_session_close(json!(3), &json!({"sessionId": session_id}));
        assert!(closed.error.is_none(), "{:?}", closed.error);
        assert!(!dir.exists());
        assert!(!adapter.sessions.contains_key(&session_id));
    }

    #[test]
    fn session_new_rejects_sse_without_keeping_a_session() {
        let root = TempDirGuard(std::env::temp_dir().join(format!("agy-acp-sse-{}", Uuid::new_v4())));
        fs::create_dir_all(&*root).unwrap();
        let mut adapter = adapter_at(&root);
        let response = adapter.handle_session_new(json!(1), &json!({
            "mcpServers": [{
                "type": "sse",
                "name": "legacy",
                "url": "http://127.0.0.1:9/sse",
                "headers": [{"name": "Authorization", "value": "header-secret-value"}]
            }]
        }));
        let message = response.error.unwrap()["message"].as_str().unwrap().to_string();
        assert!(message.contains("sse"));
        assert!(!message.contains("header-secret-value"));
        assert!(adapter.sessions.is_empty());
        assert!(adapter.mcp.dir_for("legacy").is_none());
        let plugins = root.join(".gemini/config/plugins");
        if plugins.exists() {
            assert!(fs::read_dir(&plugins).unwrap().next().is_none());
        }
    }
}
