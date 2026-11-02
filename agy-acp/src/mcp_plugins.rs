//! Per-session agy plugins for ACP `mcpServers`.
//!
//! agy discovers `$HOME/.gemini/config/plugins/<name>/` when that directory
//! contains `plugin.json`. See `agy-acp/WORKROOM_FORK.md`.

use fs2::FileExt;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const DIR_PREFIX: &str = "acp-session-";
const MARKER_NAME: &str = ".agy-acp-owned";
const MARKER_BODY: &str = "agy-acp-owned\n";
/// Linux `O_NOFOLLOW`. Do not follow a symlink at the final path component.
#[cfg(target_os = "linux")]
const O_NOFOLLOW: i32 = 0o400000;

#[derive(Debug, PartialEq, Eq)]
pub enum McpError {
    Invalid { message: String },
    Failed { message: String },
}

impl McpError {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid { message: message.into() }
    }

    fn failed(message: impl Into<String>) -> Self {
        Self::Failed { message: message.into() }
    }

    fn io(err: io::Error) -> Self {
        Self::failed(format!("failed to install MCP plugin: {err}"))
    }

    pub fn message(&self) -> &str {
        match self {
            Self::Invalid { message } | Self::Failed { message } => message,
        }
    }

    pub fn code(&self) -> i64 {
        match self {
            Self::Invalid { .. } => -32602,
            Self::Failed { .. } => -32000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Transport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TranslatedServer {
    name: String,
    transport: Transport,
}

struct LivePlugin {
    dir: PathBuf,
    lock: File,
}

enum Claim {
    Available,
    Occupied,
}

enum StageError {
    Collision,
    Fatal(McpError),
}

pub struct PluginStore {
    home: PathBuf,
    live: HashMap<String, LivePlugin>,
}

impl PluginStore {
    /// Open a store and remove unlocked plugins left by a dead adapter.
    pub fn new(home: PathBuf) -> Self {
        let store = Self::detached(home);
        store.cleanup_stale();
        store
    }

    /// Do not scan the filesystem. Tests use this when no plugin is installed.
    pub fn detached(home: PathBuf) -> Self {
        Self { home, live: HashMap::new() }
    }

    pub fn plugins_dir(&self) -> PathBuf {
        self.home.join(".gemini").join("config").join("plugins")
    }

    fn staging_root(&self) -> PathBuf {
        self.home.join(".openab").join("agy-acp").join("mcp-staging")
    }

    #[cfg(test)]
    pub fn dir_for(&self, session_id: &str) -> Option<&Path> {
        self.live.get(session_id).map(|live| live.dir.as_path())
    }

    /// Returns the line written to stderr, when a plugin was installed or updated.
    pub fn sync(&mut self, session_id: &str, servers: &[Value]) -> Result<Option<String>, McpError> {
        let translated = translate_servers(servers)?;
        if translated.is_empty() {
            self.remove_session(session_id);
            return Ok(None);
        }
        if let Some(dir) = self.live.get(session_id).map(|live| live.dir.clone()) {
            let name = component_name(&dir)?;
            write_plugin_files(&dir, &name, &translated).map_err(McpError::io)?;
            let line = server_log_line(&name, "updated", &translated);
            eprintln!("{line}");
            return Ok(Some(line));
        }
        let dir = self.publish_new(session_id, &translated)?;
        let name = component_name(&dir)?;
        let line = server_log_line(&name, "installed", &translated);
        eprintln!("{line}");
        Ok(Some(line))
    }

    pub fn remove_session(&mut self, session_id: &str) {
        let Some(live) = self.live.remove(session_id) else {
            return;
        };
        let name = live.dir.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| DIR_PREFIX.to_string());
        if let Err(err) = remove_owned_dir(&live.dir) {
            eprintln!("[agy-acp] mcp plugin remove failed for {name}: {err}");
        } else {
            eprintln!("[agy-acp] mcp plugin {name} removed");
        }
        drop(live.lock);
    }

    pub fn remove_all(&mut self) {
        let ids: Vec<String> = self.live.keys().cloned().collect();
        for id in ids {
            self.remove_session(&id);
        }
    }

    /// Drop the lock and leave the directory in place, as a crashed process would.
    #[cfg(test)]
    pub fn release_without_removing(&mut self, session_id: &str) -> Option<PathBuf> {
        self.live.remove(session_id).map(|live| live.dir)
    }

    fn publish_new(&mut self, session_id: &str, servers: &[TranslatedServer]) -> Result<PathBuf, McpError> {
        let base = format!("{DIR_PREFIX}{}", sanitize_session_id(session_id));
        for n in 0..1024 {
            let name = if n == 0 { base.clone() } else { format!("{base}-{}", n + 1) };
            if !is_single_component(&name) {
                return Err(McpError::failed("plugin directory name is not a single path component"));
            }
            let dest = self.plugins_dir().join(&name);
            match self.prepare_destination(&dest)? {
                Claim::Occupied => continue,
                Claim::Available => match self.stage_into(session_id, &dest, &name, servers) {
                    Ok(dir) => return Ok(dir),
                    Err(StageError::Collision) => continue,
                    Err(StageError::Fatal(err)) => return Err(err),
                },
            }
        }
        Err(McpError::failed("too many MCP plugin directory collisions"))
    }

    fn prepare_destination(&self, dest: &Path) -> Result<Claim, McpError> {
        match fs::symlink_metadata(dest) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Claim::Available),
            Err(err) => Err(McpError::io(err)),
            Ok(meta) => {
                if meta.file_type().is_symlink() || !meta.is_dir() {
                    return Ok(Claim::Occupied);
                }
                if !marker_matches(dest) {
                    return Ok(Claim::Occupied);
                }
                match try_lock_marker(dest) {
                    Ok(lock) => {
                        remove_owned_dir(dest).map_err(McpError::io)?;
                        drop(lock);
                        Ok(Claim::Available)
                    }
                    Err(err) if is_would_block(&err) => Ok(Claim::Occupied),
                    Err(err) => Err(McpError::io(err)),
                }
            }
        }
    }

    fn stage_into(
        &mut self,
        session_id: &str,
        dest: &Path,
        name: &str,
        servers: &[TranslatedServer],
    ) -> Result<PathBuf, StageError> {
        let fatal = |err: io::Error| StageError::Fatal(McpError::io(err));
        ensure_dir(&self.plugins_dir()).map_err(fatal)?;
        let staging_root = self.staging_root();
        ensure_dir(&staging_root).map_err(fatal)?;
        let staging = staging_root.join(format!("{name}-{}", Uuid::new_v4()));
        create_dir_0700(&staging).map_err(fatal)?;
        let staged = (|| -> Result<(PathBuf, File), StageError> {
            write_atomic(&staging, MARKER_NAME, MARKER_BODY.as_bytes()).map_err(fatal)?;
            let lock = lock_marker(&staging).map_err(fatal)?;
            if let Err(err) = write_plugin_files(&staging, name, servers) {
                drop(lock);
                let _ = remove_owned_dir(&staging);
                return Err(fatal(err));
            }
            match fs::rename(&staging, dest) {
                Ok(()) => Ok((dest.to_path_buf(), lock)),
                Err(err) => {
                    drop(lock);
                    let _ = remove_owned_dir(&staging);
                    if dest.exists() {
                        Err(StageError::Collision)
                    } else {
                        Err(fatal(err))
                    }
                }
            }
        })();
        match staged {
            Ok((dir, lock)) => {
                self.live.insert(session_id.to_string(), LivePlugin { dir: dir.clone(), lock });
                Ok(dir)
            }
            Err(err) => {
                if staging.exists() {
                    let _ = remove_owned_dir(&staging);
                }
                Err(err)
            }
        }
    }

    fn cleanup_stale(&self) {
        self.cleanup_owned_children(&self.plugins_dir());
        self.cleanup_owned_children(&self.staging_root());
    }

    fn cleanup_owned_children(&self, root: &Path) {
        let entries = match fs::read_dir(root) {
            Ok(entries) => entries,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !name.starts_with(DIR_PREFIX) {
                continue;
            }
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => continue,
            };
            if file_type.is_symlink() || !file_type.is_dir() {
                continue;
            }
            let path = entry.path();
            if !marker_matches(&path) {
                continue;
            }
            match try_lock_marker(&path) {
                Ok(lock) => {
                    if let Err(err) = remove_owned_dir(&path) {
                        eprintln!("[agy-acp] mcp cleanup failed for {name}: {err}");
                    } else {
                        eprintln!("[agy-acp] mcp plugin {name} removed stale");
                    }
                    drop(lock);
                }
                Err(err) if is_would_block(&err) => {}
                Err(err) => eprintln!("[agy-acp] mcp cleanup skipped {name}: {err}"),
            }
        }
    }
}

fn translate_servers(servers: &[Value]) -> Result<Vec<TranslatedServer>, McpError> {
    let mut translated = Vec::with_capacity(servers.len());
    for (index, server) in servers.iter().enumerate() {
        translated.push(translate_one(index, server)?);
    }
    assign_names(translated)
}

fn translate_one(index: usize, server: &Value) -> Result<TranslatedServer, McpError> {
    let obj = server.as_object().ok_or_else(|| McpError::invalid(format!("mcpServers[{index}] must be an object")))?;
    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| McpError::invalid(format!("mcpServers[{index}] is missing a name")))?
        .to_string();
    let transport = match obj.get("type").and_then(Value::as_str) {
        Some("stdio") => translate_stdio(&name, obj)?,
        Some("http") => translate_http(&name, obj)?,
        Some("sse") => {
            return Err(McpError::invalid(format!(
                "MCP server \"{name}\" uses sse, which agy does not support"
            )))
        }
        Some(other) => {
            return Err(McpError::invalid(format!(
                "MCP server \"{name}\" has unsupported type \"{other}\""
            )))
        }
        None => {
            let has_command = obj.get("command").and_then(Value::as_str).is_some();
            let has_url = obj.get("url").and_then(Value::as_str).is_some();
            if has_command && !has_url {
                translate_stdio(&name, obj)?
            } else if has_url && !has_command {
                translate_http(&name, obj)?
            } else {
                return Err(McpError::invalid(format!(
                    "MCP server \"{name}\" needs a stdio command or an http url"
                )));
            }
        }
    };
    Ok(TranslatedServer { name, transport })
}

fn translate_stdio(name: &str, obj: &Map<String, Value>) -> Result<Transport, McpError> {
    let command = required_string(obj, "command", name)?;
    let args = string_array(obj.get("args"), name, "args")?;
    let env = pair_object(obj.get("env"), name, "env")?;
    Ok(Transport::Stdio { command, args, env })
}

fn translate_http(name: &str, obj: &Map<String, Value>) -> Result<Transport, McpError> {
    let url = required_string(obj, "url", name)?;
    let headers = pair_object(obj.get("headers"), name, "headers")?;
    Ok(Transport::Http { url, headers })
}

fn required_string(obj: &Map<String, Value>, field: &str, server: &str) -> Result<String, McpError> {
    match obj.get(field).and_then(Value::as_str) {
        Some(value) if !value.is_empty() => Ok(value.to_string()),
        _ => Err(McpError::invalid(format!("MCP server \"{server}\" is missing {field}"))),
    }
}

fn string_array(value: Option<&Value>, server: &str, field: &str) -> Result<Vec<String>, McpError> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for (index, item) in items.iter().enumerate() {
                let Some(text) = item.as_str() else {
                    return Err(McpError::invalid(format!(
                        "MCP server \"{server}\" {field}[{index}] must be a string"
                    )));
                };
                out.push(text.to_string());
            }
            Ok(out)
        }
        Some(_) => Err(McpError::invalid(format!("MCP server \"{server}\" {field} must be an array of strings"))),
    }
}

fn pair_object(value: Option<&Value>, server: &str, field: &str) -> Result<BTreeMap<String, String>, McpError> {
    match value {
        None | Some(Value::Null) => Ok(BTreeMap::new()),
        Some(Value::Array(items)) => {
            let mut out = BTreeMap::new();
            for (index, item) in items.iter().enumerate() {
                let Some(obj) = item.as_object() else {
                    return Err(McpError::invalid(format!(
                        "MCP server \"{server}\" {field}[{index}] must be an object"
                    )));
                };
                let Some(pair_name) = obj.get("name").and_then(Value::as_str).filter(|name| !name.is_empty()) else {
                    return Err(McpError::invalid(format!(
                        "MCP server \"{server}\" {field}[{index}] is missing a name"
                    )));
                };
                let Some(pair_value) = obj.get("value").and_then(Value::as_str) else {
                    return Err(McpError::invalid(format!(
                        "MCP server \"{server}\" {field} \"{pair_name}\" is missing a string value"
                    )));
                };
                out.insert(pair_name.to_string(), pair_value.to_string());
            }
            Ok(out)
        }
        Some(_) => Err(McpError::invalid(format!(
            "MCP server \"{server}\" {field} must be an array of {{name, value}}"
        ))),
    }
}

fn assign_names(mut servers: Vec<TranslatedServer>) -> Result<Vec<TranslatedServer>, McpError> {
    let mut used = BTreeMap::<String, ()>::new();
    for server in &mut servers {
        let base = server.name.clone();
        let mut candidate = base.clone();
        let mut suffix = 2u32;
        while used.contains_key(&candidate) {
            candidate = format!("{base}-{suffix}");
            suffix = suffix.checked_add(1).ok_or_else(|| McpError::failed("too many MCP server name collisions"))?;
            if suffix > 10_000 {
                return Err(McpError::failed(format!("too many MCP server name collisions for \"{base}\"")));
            }
        }
        used.insert(candidate.clone(), ());
        server.name = candidate;
    }
    Ok(servers)
}

fn mcp_config_value(servers: &[TranslatedServer]) -> Value {
    let mut mcp_servers = Map::new();
    for server in servers {
        let mut entry = Map::new();
        match &server.transport {
            Transport::Stdio { command, args, env } => {
                entry.insert("command".to_string(), json!(command));
                entry.insert("args".to_string(), json!(args));
                if !env.is_empty() {
                    entry.insert("env".to_string(), json!(env));
                }
            }
            Transport::Http { url, headers } => {
                entry.insert("url".to_string(), json!(url));
                if !headers.is_empty() {
                    entry.insert("headers".to_string(), json!(headers));
                }
            }
        }
        mcp_servers.insert(server.name.clone(), Value::Object(entry));
    }
    json!({ "mcpServers": mcp_servers })
}

fn server_log_line(dir_name: &str, action: &str, servers: &[TranslatedServer]) -> String {
    format!("[agy-acp] mcp plugin {dir_name} {action}: {}", describe_servers(servers))
}

fn describe_servers(servers: &[TranslatedServer]) -> String {
    servers
        .iter()
        .map(|server| match &server.transport {
            Transport::Stdio { env, .. } => {
                format!("stdio {} env=[{}]", server.name, join_keys(env))
            }
            Transport::Http { headers, .. } => {
                format!("http {} headers=[{}]", server.name, join_keys(headers))
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn join_keys(map: &BTreeMap<String, String>) -> String {
    map.keys().cloned().collect::<Vec<_>>().join(",")
}

fn sanitize_session_id(session_id: &str) -> String {
    let mut out = String::new();
    for ch in session_id.chars() {
        if out.len() >= 64 {
            break;
        }
        if ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    let trimmed = out.trim_start_matches('.').to_string();
    if trimmed.is_empty() { "session".to_string() } else { trimmed }
}

fn is_single_component(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}

fn component_name(path: &Path) -> Result<String, McpError> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| is_single_component(name) && name.starts_with(DIR_PREFIX))
        .ok_or_else(|| McpError::failed("MCP plugin path is not an adapter directory"))?;
    Ok(name.to_string())
}

fn write_plugin_files(dir: &Path, plugin_name: &str, servers: &[TranslatedServer]) -> io::Result<()> {
    let manifest = serde_json::to_vec_pretty(&json!({ "name": plugin_name }))
        .map_err(|err| io::Error::new(io::ErrorKind::Other, format!("failed to encode plugin.json: {err}")))?;
    let mut manifest = manifest;
    manifest.push(b'\n');
    write_atomic(dir, "plugin.json", &manifest)?;
    let config = serde_json::to_vec_pretty(&mcp_config_value(servers))
        .map_err(|err| io::Error::new(io::ErrorKind::Other, format!("failed to encode mcp_config.json: {err}")))?;
    let mut config = config;
    config.push(b'\n');
    write_atomic(dir, "mcp_config.json", &config)?;
    Ok(())
}

fn write_atomic(dir: &Path, file_name: &str, bytes: &[u8]) -> io::Result<()> {
    if !is_single_component(file_name) && file_name != MARKER_NAME {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid file name"));
    }
    if file_name.contains('/') || file_name.contains('\\') || file_name.contains('\0') {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid file name"));
    }
    let dest = dir.join(file_name);
    let tmp = dir.join(format!(".{file_name}.{}.tmp", std::process::id()));
    if let Ok(meta) = fs::symlink_metadata(&tmp) {
        if meta.file_type().is_symlink() || meta.is_file() {
            fs::remove_file(&tmp)?;
        } else {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "temp path is a directory"));
        }
    }
    let write_result = (|| -> io::Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
            #[cfg(target_os = "linux")]
            options.custom_flags(O_NOFOLLOW);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        }
        fs::rename(&tmp, &dest)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dest, fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    write_result
}

fn create_dir_0700(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn ensure_dir(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(io::Error::new(io::ErrorKind::Other, "refusing to use a symlink directory"));
            }
            if meta.is_dir() {
                return Ok(());
            }
            Err(io::Error::new(io::ErrorKind::AlreadyExists, "path exists and is not a directory"))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                if parent != path && !parent.as_os_str().is_empty() {
                    ensure_dir(parent)?;
                }
            }
            create_dir_0700(path)
        }
        Err(err) => Err(err),
    }
}

fn marker_matches(dir: &Path) -> bool {
    read_marker(dir).ok().as_deref() == Some(MARKER_BODY)
}

fn read_marker(dir: &Path) -> io::Result<String> {
    let path = dir.join(MARKER_NAME);
    let meta = fs::symlink_metadata(&path)?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "marker is not a regular file"));
    }
    if meta.len() > 64 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "marker is unexpected"));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(O_NOFOLLOW);
    }
    let mut file = options.open(&path)?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    Ok(buf)
}

fn try_lock_marker(dir: &Path) -> io::Result<File> {
    let file = open_marker(dir)?;
    file.try_lock_exclusive()?;
    Ok(file)
}

fn lock_marker(dir: &Path) -> io::Result<File> {
    let file = open_marker(dir)?;
    file.lock_exclusive()?;
    Ok(file)
}

fn open_marker(dir: &Path) -> io::Result<File> {
    let path = dir.join(MARKER_NAME);
    let meta = fs::symlink_metadata(&path)?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "marker is not a regular file"));
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(O_NOFOLLOW);
    }
    options.open(&path)
}

fn remove_owned_dir(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(io::Error::new(io::ErrorKind::Other, "refusing to remove a path that is not a directory"));
    }
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("");
    if !name.starts_with(DIR_PREFIX) || !is_single_component(name) {
        return Err(io::Error::new(io::ErrorKind::Other, "refusing to remove a directory this adapter did not name"));
    }
    if !marker_matches(path) {
        return Err(io::Error::new(io::ErrorKind::Other, "refusing to remove a plugin this adapter did not create"));
    }
    fs::remove_dir_all(path)
}

fn is_would_block(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::WouldBlock
}

#[cfg(all(unix, test))]
fn mode_bits(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDirGuard(PathBuf);
    impl std::ops::Deref for TempDirGuard {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temp_home() -> TempDirGuard {
        let path = std::env::temp_dir().join(format!("agy-acp-mcp-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        TempDirGuard(path)
    }

    fn sample_servers() -> Vec<Value> {
        vec![
            json!({
                "name": "conduit",
                "command": "/bin/true",
                "args": ["--flag"],
                "env": [{"name": "TOKEN", "value": "env-secret-value"}, {"name": "TOKEN", "value": "env-secret-replaced"}]
            }),
            json!({
                "type": "http",
                "name": "remote",
                "url": "http://127.0.0.1:9/mcp",
                "headers": [
                    {"name": "Authorization", "value": "header-secret-value"},
                    {"name": "X-Request-Id", "value": "req-1"}
                ]
            }),
        ]
    }

    #[test]
    fn translate_converts_env_and_headers_to_objects() {
        let translated = translate_servers(&sample_servers()).unwrap();
        assert_eq!(translated[0].name, "conduit");
        match &translated[0].transport {
            Transport::Stdio { command, args, env } => {
                assert_eq!(command, "/bin/true");
                assert_eq!(args, &vec!["--flag".to_string()]);
                assert_eq!(env.get("TOKEN").map(String::as_str), Some("env-secret-replaced"));
            }
            Transport::Http { .. } => panic!("stdio server translated as http"),
        }
        match &translated[1].transport {
            Transport::Http { url, headers } => {
                assert_eq!(url, "http://127.0.0.1:9/mcp");
                assert_eq!(headers.get("Authorization").map(String::as_str), Some("header-secret-value"));
                assert_eq!(headers.get("X-Request-Id").map(String::as_str), Some("req-1"));
            }
            Transport::Stdio { .. } => panic!("http server translated as stdio"),
        }
    }

    #[test]
    fn translate_rejects_sse_and_does_not_echo_secrets() {
        let err = translate_servers(&[json!({
            "type": "sse",
            "name": "legacy",
            "url": "http://127.0.0.1:9/sse",
            "headers": [{"name": "Authorization", "value": "header-secret-value"}]
        })])
        .unwrap_err();
        assert_eq!(err.code(), -32602);
        assert!(err.message().contains("sse"));
        assert!(err.message().contains("legacy"));
        assert!(!err.message().contains("header-secret-value"));
        assert!(!err.message().contains("127.0.0.1"));
    }

    #[test]
    fn translate_rejects_ambiguous_and_malformed_servers() {
        let both = translate_servers(&[json!({"name": "both", "command": "/bin/true", "url": "http://127.0.0.1:9/mcp"})]).unwrap_err();
        assert!(both.message().contains("both"));
        assert!(!both.message().contains("127.0.0.1"));
        let missing = translate_servers(&[json!({"command": "/bin/true"})]).unwrap_err();
        assert!(missing.message().contains("missing a name"));
        let bad_env = translate_servers(&[json!({"name": "conduit", "command": "/bin/true", "env": {"TOKEN": "env-secret-value"}})]).unwrap_err();
        assert!(bad_env.message().contains("env"));
        assert!(!bad_env.message().contains("env-secret-value"));
    }

    #[test]
    fn duplicate_server_names_get_stable_suffixes() {
        let translated = translate_servers(&[
            json!({"name": "conduit", "command": "/bin/true"}),
            json!({"name": "conduit", "command": "/bin/false"}),
            json!({"name": "conduit-2", "command": "/bin/echo"}),
        ])
        .unwrap();
        let names: Vec<_> = translated.iter().map(|server| server.name.as_str()).collect();
        assert_eq!(names, vec!["conduit", "conduit-2", "conduit-2-2"]);
    }

    #[test]
    fn describe_servers_logs_names_only() {
        let translated = translate_servers(&sample_servers()).unwrap();
        let line = server_log_line("acp-session-abc", "installed", &translated);
        assert!(line.contains("stdio conduit env=[TOKEN]"));
        assert!(line.contains("http remote headers=[Authorization,X-Request-Id]"));
        assert!(!line.contains("env-secret"));
        assert!(!line.contains("header-secret"));
        assert!(!line.contains("/bin/true"));
        assert!(!line.contains("127.0.0.1"));
    }

    #[test]
    fn install_is_atomic_private_and_logs_names_only() {
        let home = temp_home();
        let mut store = PluginStore::new(home.to_path_buf());
        let line = store.sync("sess/../1", &sample_servers()).unwrap().unwrap();
        assert!(line.contains("env=[TOKEN]"));
        assert!(!line.contains("env-secret"));
        assert!(!line.contains("header-secret"));
        let dir = store.dir_for("sess/../1").unwrap().to_path_buf();
        assert_eq!(dir.parent(), Some(store.plugins_dir().as_path()));
        assert!(dir.file_name().unwrap().to_str().unwrap().starts_with(DIR_PREFIX));
        assert!(!dir.file_name().unwrap().to_str().unwrap().contains('/'));

        let manifest: Value = serde_json::from_str(&fs::read_to_string(dir.join("plugin.json")).unwrap()).unwrap();
        assert_eq!(manifest["name"], dir.file_name().unwrap().to_str().unwrap());
        let config: Value = serde_json::from_str(&fs::read_to_string(dir.join("mcp_config.json")).unwrap()).unwrap();
        assert_eq!(config["mcpServers"]["conduit"]["command"], "/bin/true");
        assert_eq!(config["mcpServers"]["conduit"]["args"], json!(["--flag"]));
        assert_eq!(config["mcpServers"]["conduit"]["env"]["TOKEN"], "env-secret-replaced");
        assert_eq!(config["mcpServers"]["remote"]["url"], "http://127.0.0.1:9/mcp");
        assert_eq!(config["mcpServers"]["remote"]["headers"]["Authorization"], "header-secret-value");
        assert!(config["mcpServers"]["remote"].get("type").is_none());
        assert_eq!(fs::read_to_string(dir.join(MARKER_NAME)).unwrap(), MARKER_BODY);

        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");

        #[cfg(unix)]
        {
            assert_eq!(mode_bits(&dir), 0o700);
            assert_eq!(mode_bits(&dir.join("plugin.json")), 0o600);
            assert_eq!(mode_bits(&dir.join("mcp_config.json")), 0o600);
            assert_eq!(mode_bits(&dir.join(MARKER_NAME)), 0o600);
            assert_eq!(mode_bits(&home.join(".gemini")), 0o700);
            assert_eq!(mode_bits(&store.plugins_dir()), 0o700);
        }
    }

    #[test]
    fn foreign_plugin_and_symlink_are_not_overwritten() {
        let home = temp_home();
        let mut store = PluginStore::new(home.to_path_buf());
        let plugins = store.plugins_dir();
        fs::create_dir_all(&plugins).unwrap();
        let foreign = plugins.join("acp-session-foreign");
        fs::create_dir(&foreign).unwrap();
        fs::write(foreign.join("notes.txt"), "keep-me").unwrap();
        let user = plugins.join("user-plugin");
        fs::create_dir(&user).unwrap();
        fs::write(user.join("plugin.json"), "{\"name\":\"user-plugin\"}\n").unwrap();
        let outside = home.join("outside");
        fs::create_dir(&outside).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, plugins.join("acp-session-link")).unwrap();
        }

        store.sync("foreign", &[json!({"name": "conduit", "command": "/bin/true"})]).unwrap();
        store.sync("link", &[json!({"name": "conduit", "command": "/bin/true"})]).unwrap();

        assert_eq!(fs::read_to_string(foreign.join("notes.txt")).unwrap(), "keep-me");
        assert!(!foreign.join("mcp_config.json").exists());
        assert!(user.join("plugin.json").exists());
        assert!(!outside.join("mcp_config.json").exists());
        let foreign_plugin = store.dir_for("foreign").unwrap();
        assert_eq!(foreign_plugin.file_name().unwrap(), "acp-session-foreign-2");
        #[cfg(unix)]
        {
            let link = fs::symlink_metadata(plugins.join("acp-session-link")).unwrap();
            assert!(link.file_type().is_symlink());
            assert_ne!(store.dir_for("link").unwrap(), plugins.join("acp-session-link"));
        }
    }

    #[test]
    fn colliding_sanitized_ids_get_distinct_directories() {
        let home = temp_home();
        let mut store = PluginStore::new(home.to_path_buf());
        let servers = vec![json!({"name": "conduit", "command": "/bin/true"})];
        store.sync("Foo/Bar", &servers).unwrap();
        store.sync("Foo_Bar", &servers).unwrap();
        let first = store.dir_for("Foo/Bar").unwrap().file_name().unwrap().to_str().unwrap().to_string();
        let second = store.dir_for("Foo_Bar").unwrap().file_name().unwrap().to_str().unwrap().to_string();
        assert_eq!(first, "acp-session-Foo_Bar");
        assert_eq!(second, "acp-session-Foo_Bar-2");
    }

    #[test]
    fn update_replaces_the_same_directory_and_empty_list_removes_it() {
        let home = temp_home();
        let mut store = PluginStore::new(home.to_path_buf());
        store.sync("sess-1", &[json!({"name": "conduit", "command": "/bin/true"})]).unwrap();
        let dir = store.dir_for("sess-1").unwrap().to_path_buf();
        store.sync("sess-1", &[json!({"type": "http", "name": "remote", "url": "http://127.0.0.1:9/mcp"})]).unwrap();
        assert_eq!(store.dir_for("sess-1").unwrap(), dir.as_path());
        let config: Value = serde_json::from_str(&fs::read_to_string(dir.join("mcp_config.json")).unwrap()).unwrap();
        assert!(config["mcpServers"].get("conduit").is_none());
        assert_eq!(config["mcpServers"]["remote"]["url"], "http://127.0.0.1:9/mcp");
        assert!(config["mcpServers"]["remote"].get("headers").is_none());
        assert!(store.sync("sess-1", &[]).unwrap().is_none());
        assert!(!dir.exists());
        assert!(store.dir_for("sess-1").is_none());
    }

    #[test]
    fn startup_removes_stale_owned_plugins_only() {
        let home = temp_home();
        let mut store = PluginStore::new(home.to_path_buf());
        store.sync("sess-1", &[json!({"name": "conduit", "command": "/bin/true", "env": [{"name": "TOKEN", "value": "env-secret-value"}]})]).unwrap();
        let owned = store.release_without_removing("sess-1").unwrap();
        assert!(owned.join("mcp_config.json").exists());

        let plugins = store.plugins_dir();
        let foreign = plugins.join("acp-session-foreign");
        fs::create_dir(&foreign).unwrap();
        fs::write(foreign.join("notes.txt"), "keep-me").unwrap();
        let marked_user = plugins.join("user-plugin");
        fs::create_dir(&marked_user).unwrap();
        fs::write(marked_user.join(MARKER_NAME), MARKER_BODY).unwrap();
        fs::write(marked_user.join("plugin.json"), "{\"name\":\"user-plugin\"}\n").unwrap();

        let cleaned = PluginStore::new(home.to_path_buf());
        assert!(!owned.exists());
        assert_eq!(fs::read_to_string(foreign.join("notes.txt")).unwrap(), "keep-me");
        assert!(marked_user.join("plugin.json").exists());
        drop(cleaned);

        let mut live = PluginStore::new(home.to_path_buf());
        live.sync("sess-2", &[json!({"name": "conduit", "command": "/bin/true"})]).unwrap();
        let live_dir = live.dir_for("sess-2").unwrap().to_path_buf();
        let other = PluginStore::new(home.to_path_buf());
        assert!(live_dir.exists());
        drop(other);
        assert!(live_dir.exists());
        live.remove_all();
        assert!(!live_dir.exists());
        assert!(foreign.join("notes.txt").exists());
    }

    #[test]
    fn escaped_session_id_cannot_leave_the_plugins_directory() {
        let home = temp_home();
        let mut store = PluginStore::new(home.to_path_buf());
        store.sync("../../etc/passwd", &[json!({"name": "conduit", "command": "/bin/true"})]).unwrap();
        let dir = store.dir_for("../../etc/passwd").unwrap();
        assert_eq!(dir.parent(), Some(store.plugins_dir().as_path()));
        let name = dir.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("acp-session-"));
        assert!(!name.contains('/'));
        assert!(!Path::new("/etc/passwd-mcp").exists());
    }
}
