//! # 本地存储
//!
//! Onemore 把机器级数据放在用户主目录的 `.onemore/`：
//! - `config.toml` 是所有 workspace 共用的配置；
//! - `sessions/<uuid>.db` 是一会话一库的 SQLite 事实日志。
//!
//! ## 事实日志(schema v4)
//! 数据库不再保存"最终模型消息",而是保存 [`SessionEntry`] 事实:
//! 每条 entry 有 `id + parent_id + kind + payload`,payload 是厂商无关的
//! [`SessionEntryPayload`] JSON。模型看到什么由 `session::project_model_messages`
//! 单向投影决定,存储层从不反向修改事实。
//!
//! 三条硬性约束:
//! 1. **append-only**:正常运行只追加 entry;唯一的删除入口是用户显式 /clear。
//! 2. **entry、leaf、统计在同一事务提交**:崩溃后不会出现"entry 写了一半、
//!    leaf 指向不存在节点"的状态;带 ToolUse 的消息批在提交前还要过
//!    [`validate_new_message_batch`],半批直接拒绝。
//! 3. **旧库迁移原子化**:旧库在单个事务里迁移到当前 schema,
//!    任何一步失败都回滚,原库保持可用。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::message::{Block, CacheUsage, ChatMessage, Role, Usage};
use crate::plan::validate_plan_append;
use crate::session::{
    validate_new_message_batch, MessageRecord, SessionEntry, SessionEntryPayload,
};

pub const APP_HOME_ENV: &str = "ONEMORE_HOME";
pub const APP_DIR_NAME: &str = ".onemore";

/// 当前 schema 版本。v1 = 线性 messages 表(无版本号,user_version=0);
/// v2 = entries 事实日志 + session.leaf_id;
/// v3 = session cache_read_tokens/cache_write_tokens 累计值；
/// v4 = 严格校验追加式 PlanUpdated facts。
const SCHEMA_VERSION: i64 = 4;
const ENTRIES_SCHEMA_VERSION: i64 = 2;

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub root: PathBuf,
    pub config: PathBuf,
    pub sessions: PathBuf,
    pub workspaces: PathBuf,
}

impl AppPaths {
    /// `ONEMORE_HOME` 是测试和便携安装的显式覆盖；正常运行使用平台数据目录。
    pub fn discover() -> Result<Self> {
        let root = match std::env::var_os(APP_HOME_ENV) {
            Some(path) if !path.is_empty() => PathBuf::from(path),
            _ => platform_app_root()?,
        };
        Ok(Self::from_root(root))
    }

    pub fn from_root(root: PathBuf) -> Self {
        AppPaths {
            config: root.join("config.toml"),
            sessions: root.join("sessions"),
            workspaces: root.join("workspaces"),
            root,
        }
    }

    pub fn ensure(&self) -> Result<()> {
        std::fs::create_dir_all(&self.sessions)
            .with_context(|| format!("创建数据目录 {} 失败", self.sessions.display()))?;
        std::fs::create_dir_all(&self.workspaces)
            .with_context(|| format!("创建数据目录 {} 失败", self.workspaces.display()))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct WorkspacePreferencesFile {
    #[serde(default)]
    reasoning_efforts: BTreeMap<String, BTreeMap<String, String>>,
}

/// Workspace 级模型偏好。只保存用户偏离默认 `medium` 的选择；配置与会话事实
/// 仍各自负责能力目录和历史审计。
pub struct WorkspacePreferences {
    path: PathBuf,
    file: WorkspacePreferencesFile,
}

impl WorkspacePreferences {
    pub fn load(workspaces_dir: &Path, workspace: &Path) -> Result<Self> {
        std::fs::create_dir_all(workspaces_dir).with_context(|| {
            format!("创建 workspace 偏好目录 {} 失败", workspaces_dir.display())
        })?;
        let key = workspace_key(workspace);
        let digest = Sha256::digest(key.as_bytes());
        let hash = digest
            .iter()
            .map(|byte| format!("{:02x}", byte))
            .collect::<String>();
        let path = workspaces_dir.join(format!("{}.json", hash));
        let file = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("解析 workspace 偏好 {} 失败", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                WorkspacePreferencesFile::default()
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("读取 workspace 偏好 {} 失败", path.display()))
            }
        };
        Ok(Self { path, file })
    }

    pub fn effort(&self, provider: &str, model: &str) -> Option<&str> {
        self.file
            .reasoning_efforts
            .get(provider)
            .and_then(|models| models.get(model))
            .map(String::as_str)
    }

    pub fn reasoning_efforts(&self) -> BTreeMap<String, BTreeMap<String, String>> {
        self.file.reasoning_efforts.clone()
    }

    /// 每个模型的配置默认值不写入磁盘；切回默认值会删除已有覆盖。
    pub fn set_effort(
        &mut self,
        provider: &str,
        model: &str,
        effort: &str,
        default_effort: &str,
    ) -> Result<()> {
        let mut next = self.file.clone();
        if effort == default_effort {
            if let Some(models) = next.reasoning_efforts.get_mut(provider) {
                models.remove(model);
                if models.is_empty() {
                    next.reasoning_efforts.remove(provider);
                }
            }
        } else {
            next.reasoning_efforts
                .entry(provider.to_string())
                .or_default()
                .insert(model.to_string(), effort.to_string());
        }
        self.write_file(&next)?;
        self.file = next;
        Ok(())
    }

    fn write_file(&self, file: &WorkspacePreferencesFile) -> Result<()> {
        if file.reasoning_efforts.is_empty() {
            match std::fs::remove_file(&self.path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("删除 workspace 偏好 {} 失败", self.path.display())
                    })
                }
            }
            return Ok(());
        }
        let bytes = serde_json::to_vec_pretty(file)?;
        let temp = self
            .path
            .with_extension(format!("tmp-{}", std::process::id()));
        std::fs::write(&temp, bytes)
            .with_context(|| format!("写入 workspace 偏好临时文件 {} 失败", temp.display()))?;
        if self.path.exists() {
            std::fs::remove_file(&self.path)
                .with_context(|| format!("替换 workspace 偏好 {} 失败", self.path.display()))?;
        }
        std::fs::rename(&temp, &self.path).with_context(|| {
            format!(
                "提交 workspace 偏好 {} -> {} 失败",
                temp.display(),
                self.path.display()
            )
        })
    }
}

fn user_home_dir() -> Result<PathBuf> {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE").or_else(|| {
        match (std::env::var_os("HOMEDRIVE"), std::env::var_os("HOMEPATH")) {
            (Some(drive), Some(path)) => {
                let mut value = drive;
                value.push(path);
                Some(value)
            }
            _ => None,
        }
    });

    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");

    home.map(PathBuf::from)
        .ok_or_else(|| anyhow!("无法确定用户主目录，请设置 {}", APP_HOME_ENV))
}

fn platform_app_root() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(app_data) = std::env::var_os("APPDATA") {
            return Ok(PathBuf::from(app_data).join("onemore"));
        }
        Ok(user_home_dir()?
            .join("AppData")
            .join("Roaming")
            .join("onemore"))
    }

    #[cfg(not(windows))]
    {
        if let Some(data_home) = std::env::var_os("XDG_DATA_HOME") {
            return Ok(PathBuf::from(data_home).join("onemore"));
        }
        Ok(user_home_dir()?
            .join(".local")
            .join("share")
            .join("onemore"))
    }
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    /// message 类事实条数(不含 Notice/Compaction 等 UI/控制事实)。
    pub message_count: usize,
    pub updated_at: i64,
}

pub struct SessionManager {
    sessions_dir: PathBuf,
    workspace: String,
    current: SessionStore,
}

impl SessionManager {
    pub fn create(sessions_dir: PathBuf, workspace: &Path) -> Result<Self> {
        std::fs::create_dir_all(&sessions_dir)
            .with_context(|| format!("创建会话目录 {} 失败", sessions_dir.display()))?;
        let workspace = workspace_key(workspace);
        let current = SessionStore::create(&sessions_dir, &workspace)?;
        Ok(SessionManager {
            sessions_dir,
            workspace,
            current,
        })
    }

    pub fn current_id(&self) -> &str {
        &self.current.id
    }

    /// 把一批 payload 变成因果相连的 entry 并原子提交。
    /// 返回持久化成功的 entry(含分配的 id/parent),调用方以此为准更新内存日志;
    /// 返回 Err 时数据库与 leaf 均未变化。
    pub fn append_payloads(
        &mut self,
        payloads: Vec<SessionEntryPayload>,
        usage: Usage,
    ) -> Result<Vec<SessionEntry>> {
        self.current.append_payloads(payloads, usage)
    }

    pub fn clear(&mut self) -> Result<()> {
        self.current.clear()
    }

    pub fn list(&self) -> Result<Vec<SessionSummary>> {
        let mut sessions = Vec::new();
        for entry in std::fs::read_dir(&self.sessions_dir)
            .with_context(|| format!("读取会话目录 {} 失败", self.sessions_dir.display()))?
        {
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("db") {
                continue;
            }
            match SessionStore::read_summary(&path, &self.workspace) {
                Ok(Some(summary)) => sessions.push(summary),
                Ok(None) => {}
                Err(_) => continue, // 一个损坏的库不应阻止找回其他会话。
            }
        }
        sessions.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        Ok(sessions)
    }

    /// 接受完整 UUID 或当前 workspace 内唯一的 UUID 前缀。
    pub fn load(&mut self, requested_id: &str) -> Result<(Vec<SessionEntry>, Usage)> {
        let id = self.resolve_id(requested_id)?;
        let next = SessionStore::open(&self.sessions_dir, &id, &self.workspace)?;
        let loaded = next.load_entries()?;
        self.current = next;
        Ok(loaded)
    }

    fn resolve_id(&self, requested_id: &str) -> Result<String> {
        let requested_id = requested_id.trim();
        if requested_id.is_empty()
            || !requested_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            bail!("无效的会话 ID {:?}", requested_id);
        }
        let matches: Vec<String> = self
            .list()?
            .into_iter()
            .filter(|s| s.id.starts_with(requested_id))
            .map(|s| s.id)
            .collect();
        match matches.as_slice() {
            [] => bail!("当前 workspace 找不到会话 {}", requested_id),
            [id] => Ok(id.clone()),
            _ => bail!("会话 ID 前缀 {} 不唯一，请输入更多字符", requested_id),
        }
    }
}

struct SessionStore {
    id: String,
    connection: Connection,
    /// 当前链尾。append 时校验并推进;与数据库在同一事务中保持一致。
    leaf_id: Option<String>,
}

impl SessionStore {
    fn create(sessions_dir: &Path, workspace: &str) -> Result<Self> {
        let id = uuid::Uuid::new_v4().to_string();
        let path = sessions_dir.join(format!("{}.db", id));
        let mut connection = Connection::open(&path)
            .with_context(|| format!("创建会话数据库 {} 失败", path.display()))?;
        initialize(&mut connection)?;
        let now = unix_timestamp();
        connection.execute(
            "INSERT INTO session (id, workspace, title, created_at, updated_at, input_tokens, output_tokens, \
             cache_read_tokens, cache_write_tokens, leaf_id) \
             VALUES (?1, ?2, '', ?3, ?3, 0, 0, NULL, NULL, NULL)",
            params![id, workspace, now],
        )?;
        Ok(SessionStore {
            id,
            connection,
            leaf_id: None,
        })
    }

    fn open(sessions_dir: &Path, id: &str, workspace: &str) -> Result<Self> {
        let path = sessions_dir.join(format!("{}.db", id));
        let mut connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("打开会话数据库 {} 失败", path.display()))?;
        initialize(&mut connection)?;
        let row: Option<(String, Option<String>)> = connection
            .query_row(
                "SELECT workspace, leaf_id FROM session WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let leaf_id = match row {
            Some((stored, leaf)) if stored == workspace => leaf,
            Some(_) => bail!("会话 {} 不属于当前 workspace", id),
            None => bail!("会话数据库 {} 缺少元数据", path.display()),
        };
        Ok(SessionStore {
            id: id.to_string(),
            connection,
            leaf_id,
        })
    }

    fn append_payloads(
        &mut self,
        payloads: Vec<SessionEntryPayload>,
        usage: Usage,
    ) -> Result<Vec<SessionEntry>> {
        if payloads.is_empty() {
            return Ok(Vec::new());
        }
        // 提交边界的最后防线:带 ToolUse 的消息批必须在本批内配对完整。
        validate_new_message_batch(&payloads).map_err(|reason| anyhow!(reason))?;
        // 计划事实同样必须从当前合法快照严格推进。并发写者即使在这次读取后
        // 插入数据，下面事务内的 leaf 一致性校验也会拒绝本批提交。
        let existing = self.load_entries()?.0;
        validate_plan_append(&existing, &payloads).map_err(|error| anyhow!(error.message))?;

        let now = unix_timestamp();
        let mut parent = self.leaf_id.clone();
        let mut entries = Vec::with_capacity(payloads.len());
        for payload in payloads {
            let entry = SessionEntry {
                id: uuid::Uuid::new_v4().to_string(),
                parent_id: parent.clone(),
                created_at: now,
                payload,
            };
            parent = Some(entry.id.clone());
            entries.push(entry);
        }

        let tx = self.connection.transaction()?;
        // leaf 一致性:内存视角与数据库必须指向同一链尾,否则说明有并发写者或损坏。
        let stored_leaf: Option<String> = tx.query_row(
            "SELECT leaf_id FROM session WHERE id = ?1",
            [&self.id],
            |row| row.get(0),
        )?;
        if stored_leaf != self.leaf_id {
            bail!(
                "会话链尾不一致(内存 {:?},库内 {:?}),拒绝追加",
                self.leaf_id,
                stored_leaf
            );
        }
        let mut first_user_text = String::new();
        for entry in &entries {
            let payload = serde_json::to_string(&entry.payload).context("序列化会话事实失败")?;
            tx.execute(
                "INSERT INTO entries (id, parent_id, kind, payload, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    entry.id,
                    entry.parent_id,
                    entry.payload.kind(),
                    payload,
                    entry.created_at
                ],
            )?;
            if first_user_text.is_empty() {
                if let SessionEntryPayload::Message(record) = &entry.payload {
                    if record.message.role == Role::User {
                        first_user_text = record
                            .message
                            .blocks
                            .iter()
                            .find_map(|block| match block {
                                Block::Text(text) if !text.trim().is_empty() => Some(text.trim()),
                                _ => None,
                            })
                            .unwrap_or("")
                            .chars()
                            .take(80)
                            .collect();
                    }
                }
            }
        }
        let leaf = entries.last().map(|entry| entry.id.clone());
        tx.execute(
            "UPDATE session SET \
             title = CASE WHEN title = '' AND ?1 <> '' THEN ?1 ELSE title END, \
             updated_at = ?2, input_tokens = ?3, output_tokens = ?4, \
             cache_read_tokens = ?5, cache_write_tokens = ?6, leaf_id = ?7 WHERE id = ?8",
            params![
                first_user_text,
                now,
                usage.input_tokens,
                usage.output_tokens,
                usage.cache.map(|cache| cache.read_tokens),
                usage.cache.map(|cache| cache.write_tokens),
                leaf,
                self.id
            ],
        )?;
        tx.commit()?;
        self.leaf_id = leaf;
        Ok(entries)
    }

    /// 用户显式清空。这是事实日志唯一的删除入口。
    fn clear(&mut self) -> Result<()> {
        let tx = self.connection.transaction()?;
        tx.execute("DELETE FROM entries", [])?;
        tx.execute(
            "UPDATE session SET title = '', updated_at = ?1, input_tokens = 0, output_tokens = 0, \
             cache_read_tokens = NULL, cache_write_tokens = NULL, leaf_id = NULL WHERE id = ?2",
            params![unix_timestamp(), self.id],
        )?;
        tx.commit()?;
        self.leaf_id = None;
        Ok(())
    }

    fn load_entries(&self) -> Result<(Vec<SessionEntry>, Usage)> {
        let mut statement = self
            .connection
            .prepare("SELECT id, parent_id, payload, created_at FROM entries ORDER BY sequence")?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        let mut entries = Vec::new();
        for row in rows {
            let (id, parent_id, payload, created_at) = row?;
            let payload: SessionEntryPayload =
                serde_json::from_str(&payload).context("会话数据库含有无法解析的事实")?;
            entries.push(SessionEntry {
                id,
                parent_id,
                created_at,
                payload,
            });
        }
        let usage = self.connection.query_row(
            "SELECT input_tokens, output_tokens, cache_read_tokens, cache_write_tokens \
             FROM session WHERE id = ?1",
            [&self.id],
            |row| {
                Ok(Usage {
                    input_tokens: row.get(0)?,
                    output_tokens: row.get(1)?,
                    cache: match (row.get(2)?, row.get(3)?) {
                        (None, None) => None,
                        (read_tokens, write_tokens) => Some(CacheUsage {
                            read_tokens: read_tokens.unwrap_or(0),
                            write_tokens: write_tokens.unwrap_or(0),
                        }),
                    },
                })
            },
        )?;
        Ok((entries, usage))
    }

    fn read_summary(path: &Path, workspace: &str) -> Result<Option<SessionSummary>> {
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        // 只读打开不做迁移;v1 库同样能被列出(按旧 messages 表计数)。
        let version = schema_version(&connection)?;
        let count_sql = if version >= ENTRIES_SCHEMA_VERSION {
            "SELECT COUNT(*) FROM entries WHERE kind = 'message'"
        } else {
            "SELECT COUNT(*) FROM messages"
        };
        connection
            .query_row(
                &format!(
                    "SELECT id, title, updated_at, ({}) FROM session WHERE workspace = ?1 LIMIT 1",
                    count_sql
                ),
                [workspace],
                |row| {
                    Ok(SessionSummary {
                        id: row.get(0)?,
                        title: row.get(1)?,
                        updated_at: row.get(2)?,
                        message_count: row.get::<_, i64>(3)? as usize,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }
}

fn schema_version(connection: &Connection) -> Result<i64> {
    connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(Into::into)
}

fn has_table(connection: &Connection, name: &str) -> Result<bool> {
    let count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [name],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// 建表与迁移。全部结构变更在单个事务内完成:
/// - 全新库直接建当前 schema;
/// - v1 库把 messages 逐条包装成 Message 事实迁入 entries,任何解析/写入失败
///   都会回滚,原库保持 v1 可用(验收:迁移失败保留原库)。
fn initialize(connection: &mut Connection) -> Result<()> {
    connection.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
    let version = schema_version(connection)?;
    if version >= SCHEMA_VERSION {
        return Ok(());
    }
    let legacy = version == 0 && has_table(connection, "messages")?;

    let tx = connection.transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS session (
             id TEXT PRIMARY KEY,
             workspace TEXT NOT NULL,
             title TEXT NOT NULL,
             created_at INTEGER NOT NULL,
             updated_at INTEGER NOT NULL,
             input_tokens INTEGER NOT NULL,
             output_tokens INTEGER NOT NULL,
             cache_read_tokens INTEGER,
             cache_write_tokens INTEGER
         );
         CREATE TABLE IF NOT EXISTS entries (
             sequence INTEGER PRIMARY KEY AUTOINCREMENT,
             id TEXT NOT NULL UNIQUE,
             parent_id TEXT,
             kind TEXT NOT NULL,
             payload TEXT NOT NULL,
             created_at INTEGER NOT NULL
         );",
    )?;
    if !column_exists(&tx, "session", "leaf_id")? {
        tx.execute("ALTER TABLE session ADD COLUMN leaf_id TEXT", [])?;
    }
    if !column_exists(&tx, "session", "cache_read_tokens")? {
        tx.execute(
            "ALTER TABLE session ADD COLUMN cache_read_tokens INTEGER",
            [],
        )?;
    }
    if !column_exists(&tx, "session", "cache_write_tokens")? {
        tx.execute(
            "ALTER TABLE session ADD COLUMN cache_write_tokens INTEGER",
            [],
        )?;
    }
    if legacy {
        migrate_v1_messages(&tx)?;
        tx.execute("DROP TABLE messages", [])?;
    }
    tx.execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION))?;
    tx.commit()?;
    Ok(())
}

fn column_exists(connection: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({})", table))?;
    let names = statement.query_map([], |row| row.get::<_, String>(1))?;
    for name in names {
        if name? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn migrate_v1_messages(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    let rows: Vec<(i64, String)> = {
        let mut statement =
            tx.prepare("SELECT sequence, payload FROM messages ORDER BY sequence")?;
        let mapped = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        mapped.collect::<rusqlite::Result<_>>()?
    };
    let created_at: i64 = tx
        .query_row("SELECT updated_at FROM session LIMIT 1", [], |row| {
            row.get(0)
        })
        .unwrap_or_else(|_| unix_timestamp());
    let mut parent: Option<String> = None;
    let mut leaf: Option<String> = None;
    for (sequence, payload) in rows {
        let message: ChatMessage = serde_json::from_str(&payload)
            .with_context(|| format!("v1 消息 #{} 无法解析,迁移中止", sequence))?;
        let entry_payload = SessionEntryPayload::Message(MessageRecord {
            message,
            usage: None, // v1 没有逐消息 usage,只有会话累计。
            prompt_fingerprint: None,
        });
        let id = uuid::Uuid::new_v4().to_string();
        tx.execute(
            "INSERT INTO entries (id, parent_id, kind, payload, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                id,
                parent,
                entry_payload.kind(),
                serde_json::to_string(&entry_payload)?,
                created_at
            ],
        )?;
        parent = Some(id.clone());
        leaf = Some(id);
    }
    tx.execute("UPDATE session SET leaf_id = ?1", params![leaf])?;
    Ok(())
}

fn workspace_key(path: &Path) -> String {
    let absolute = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let value = absolute.to_string_lossy().to_string();
    if cfg!(windows) {
        value.to_lowercase()
    } else {
        value
    }
}

fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{PlanItem, PlanSnapshot, PlanStatus};
    use crate::session::{NoticeLevel, NoticeRecord};

    fn temp_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "onemore-storage-{}-{}-{}",
            name,
            std::process::id(),
            uuid::Uuid::new_v4()
        ))
    }

    fn message_payload(message: ChatMessage) -> SessionEntryPayload {
        SessionEntryPayload::message(message, None)
    }

    #[test]
    fn workspace_reasoning_preferences_only_store_model_overrides() {
        let root = temp_root("reasoning-preferences");
        let workspaces = root.join("preferences");
        let workspace = root.join("workspace-a");
        let other_workspace = root.join("workspace-b");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&other_workspace).unwrap();

        let mut preferences = WorkspacePreferences::load(&workspaces, &workspace).unwrap();
        assert_eq!(preferences.effort("openai", "gpt-5"), None);
        preferences
            .set_effort("openai", "gpt-5", "high", "low")
            .unwrap();
        assert!(preferences.path.exists());

        let reloaded = WorkspacePreferences::load(&workspaces, &workspace).unwrap();
        assert_eq!(reloaded.effort("openai", "gpt-5"), Some("high"));
        let other = WorkspacePreferences::load(&workspaces, &other_workspace).unwrap();
        assert_eq!(other.effort("openai", "gpt-5"), None);

        preferences
            .set_effort("openai", "gpt-5", "low", "low")
            .unwrap();
        assert_eq!(preferences.effort("openai", "gpt-5"), None);
        assert!(!preferences.path.exists(), "切回模型默认值应删除空偏好文件");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn persists_lists_loads_and_clears_session() {
        let root = temp_root("roundtrip");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let sessions = root.join("sessions");
        let mut manager = SessionManager::create(sessions.clone(), &workspace).unwrap();
        let id = manager.current_id().to_string();
        let appended = manager
            .append_payloads(
                vec![
                    message_payload(ChatMessage::user_text("第一条问题")),
                    SessionEntryPayload::message_with_prompt(
                        ChatMessage {
                            role: Role::Assistant,
                            blocks: vec![Block::Text("回答".into())],
                        },
                        Usage::default(),
                        Some("sha256:test".into()),
                    ),
                    SessionEntryPayload::Notice(NoticeRecord {
                        text: "仅 UI 可见".into(),
                        level: NoticeLevel::Info,
                    }),
                ],
                Usage {
                    input_tokens: 12,
                    output_tokens: 7,
                    cache: Some(CacheUsage {
                        read_tokens: 8,
                        write_tokens: 3,
                    }),
                },
            )
            .unwrap();
        // parent 链:第一条无 parent,之后逐条相连。
        assert_eq!(appended[0].parent_id, None);
        assert_eq!(appended[1].parent_id, Some(appended[0].id.clone()));
        assert_eq!(appended[2].parent_id, Some(appended[1].id.clone()));

        let listed = manager.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].title, "第一条问题");
        assert_eq!(listed[0].message_count, 2, "Notice 不计入消息数");

        let mut other = SessionManager::create(sessions, &workspace).unwrap();
        let (loaded, usage) = other.load(&id[..8]).unwrap();
        assert_eq!(loaded.len(), 3);
        assert!(matches!(loaded[2].payload, SessionEntryPayload::Notice(_)));
        assert_eq!(usage.input_tokens, 12);
        assert_eq!(usage.cache.unwrap().read_tokens, 8);
        let SessionEntryPayload::Message(assistant) = &loaded[1].payload else {
            panic!("第二条事实应是 assistant message");
        };
        assert_eq!(assistant.prompt_fingerprint.as_deref(), Some("sha256:test"));
        other.clear().unwrap();
        assert!(other.load(&id).unwrap().0.is_empty());
        // 清空后可以继续追加(leaf 已复位)。
        other
            .append_payloads(
                vec![message_payload(ChatMessage::user_text("再来"))],
                Usage::default(),
            )
            .unwrap();

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn sessions_are_scoped_to_workspace() {
        let root = temp_root("workspace");
        let first = root.join("first");
        let second = root.join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let sessions = root.join("sessions");
        let first_manager = SessionManager::create(sessions.clone(), &first).unwrap();
        let first_id = first_manager.current_id().to_string();
        let mut second_manager = SessionManager::create(sessions, &second).unwrap();
        assert_eq!(second_manager.list().unwrap().len(), 1);
        assert!(second_manager.load(&first_id).is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn preserves_tool_messages_as_provider_neutral_json() {
        let root = temp_root("tools");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let sessions = root.join("sessions");
        let mut manager = SessionManager::create(sessions.clone(), &workspace).unwrap();
        let id = manager.current_id().to_string();
        manager
            .append_payloads(
                vec![
                    message_payload(ChatMessage::user_text("读取文件")),
                    message_payload(ChatMessage {
                        role: Role::Assistant,
                        blocks: vec![
                            Block::Thinking {
                                text: "先读取".into(),
                                provider_kind: Some("responses".into()),
                                raw: Some(serde_json::json!({"id": "reasoning-1"})),
                            },
                            Block::ToolUse {
                                id: "call-1".into(),
                                name: "read_file".into(),
                                input: serde_json::json!({"path": "README.md"}),
                            },
                        ],
                    }),
                    message_payload(ChatMessage {
                        role: Role::User,
                        blocks: vec![Block::ToolResult {
                            tool_use_id: "call-1".into(),
                            content: "file body".into(),
                            is_error: false,
                        }],
                    }),
                ],
                Usage::default(),
            )
            .unwrap();

        let mut reopened = SessionManager::create(sessions, &workspace).unwrap();
        let (loaded, _) = reopened.load(&id).unwrap();
        assert_eq!(loaded.len(), 3);
        let SessionEntryPayload::Message(assistant) = &loaded[1].payload else {
            panic!("应为 Message 事实");
        };
        assert_eq!(assistant.message.tool_uses()[0].0, "call-1");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn half_tool_batches_are_rejected_at_commit() {
        let root = temp_root("halfbatch");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let mut manager = SessionManager::create(root.join("sessions"), &workspace).unwrap();
        let orphan = message_payload(ChatMessage {
            role: Role::Assistant,
            blocks: vec![Block::ToolUse {
                id: "call-1".into(),
                name: "read_file".into(),
                input: serde_json::json!({}),
            }],
        });
        assert!(manager
            .append_payloads(vec![orphan], Usage::default())
            .is_err());
        // 拒绝后日志与 leaf 未变化,正常批仍可提交。
        let appended = manager
            .append_payloads(
                vec![message_payload(ChatMessage::user_text("ok"))],
                Usage::default(),
            )
            .unwrap();
        assert_eq!(appended[0].parent_id, None);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn plan_facts_are_validated_atomically_at_commit() {
        let root = temp_root("plan-atomic");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let sessions = root.join("sessions");
        let mut manager = SessionManager::create(sessions.clone(), &workspace).unwrap();
        let invalid = SessionEntryPayload::PlanUpdated(PlanSnapshot {
            revision: 2,
            items: vec![PlanItem {
                id: "inspect".into(),
                text: "Inspect the code".into(),
                status: PlanStatus::InProgress,
            }],
            explanation: None,
        });
        assert!(manager
            .append_payloads(
                vec![
                    message_payload(ChatMessage::user_text("before")),
                    invalid,
                    message_payload(ChatMessage::user_text("after")),
                ],
                Usage::default(),
            )
            .is_err());

        let valid = SessionEntryPayload::PlanUpdated(PlanSnapshot {
            revision: 1,
            items: Vec::new(),
            explanation: Some("clear".into()),
        });
        let appended = manager
            .append_payloads(vec![valid], Usage::default())
            .unwrap();
        assert_eq!(appended.len(), 1, "rejected batch must leave no entries");
        assert_eq!(
            appended[0].parent_id, None,
            "leaf must not advance on failure"
        );

        let mut reopened = SessionManager::create(sessions, &workspace).unwrap();
        let (loaded, _) = reopened.load(manager.current_id()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(matches!(
            loaded[0].payload,
            SessionEntryPayload::PlanUpdated(PlanSnapshot { revision: 1, .. })
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    /// 手工构造一个 v1 库(线性 messages 表,user_version=0)。
    fn create_v1_database(
        sessions: &Path,
        workspace: &Path,
        payloads: &[&str],
    ) -> (String, PathBuf) {
        std::fs::create_dir_all(sessions).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let path = sessions.join(format!("{}.db", id));
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE session (
                     id TEXT PRIMARY KEY,
                     workspace TEXT NOT NULL,
                     title TEXT NOT NULL,
                     created_at INTEGER NOT NULL,
                     updated_at INTEGER NOT NULL,
                     input_tokens INTEGER NOT NULL,
                     output_tokens INTEGER NOT NULL
                 );
                 CREATE TABLE messages (
                     sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                     payload TEXT NOT NULL
                 );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO session VALUES (?1, ?2, '旧标题', 100, 200, 3, 4)",
                params![id, workspace_key(workspace)],
            )
            .unwrap();
        for payload in payloads {
            connection
                .execute("INSERT INTO messages (payload) VALUES (?1)", [payload])
                .unwrap();
        }
        (id, path)
    }

    #[test]
    fn v1_databases_migrate_to_entries_preserving_order() {
        let root = temp_root("migrate-ok");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let sessions = root.join("sessions");
        let user = serde_json::to_string(&ChatMessage::user_text("旧问题")).unwrap();
        let assistant = serde_json::to_string(&ChatMessage {
            role: Role::Assistant,
            blocks: vec![Block::Text("旧回答".into())],
        })
        .unwrap();
        let (id, _path) = create_v1_database(&sessions, &workspace, &[&user, &assistant]);

        let mut manager = SessionManager::create(sessions, &workspace).unwrap();
        let (entries, usage) = manager.load(&id).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].parent_id, None);
        assert_eq!(entries[1].parent_id, Some(entries[0].id.clone()));
        let SessionEntryPayload::Message(first) = &entries[0].payload else {
            panic!("应迁移成 Message 事实");
        };
        assert_eq!(first.message.text(), "旧问题");
        assert_eq!(first.usage, None, "v1 无逐消息 usage");
        assert_eq!(usage.input_tokens, 3);
        // 迁移后可以继续追加。
        manager
            .append_payloads(
                vec![message_payload(ChatMessage::user_text("新问题"))],
                usage,
            )
            .unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn v2_schema_gains_nullable_cache_usage_columns() {
        let mut connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE session (
                     id TEXT PRIMARY KEY,
                     workspace TEXT NOT NULL,
                     title TEXT NOT NULL,
                     created_at INTEGER NOT NULL,
                     updated_at INTEGER NOT NULL,
                     input_tokens INTEGER NOT NULL,
                     output_tokens INTEGER NOT NULL,
                     leaf_id TEXT
                 );
                 CREATE TABLE entries (
                     sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                     id TEXT NOT NULL UNIQUE,
                     parent_id TEXT,
                     kind TEXT NOT NULL,
                     payload TEXT NOT NULL,
                     created_at INTEGER NOT NULL
                 );
                 PRAGMA user_version = 2;",
            )
            .unwrap();

        initialize(&mut connection).unwrap();
        assert_eq!(schema_version(&connection).unwrap(), SCHEMA_VERSION);
        assert!(column_exists(&connection, "session", "cache_read_tokens").unwrap());
        assert!(column_exists(&connection, "session", "cache_write_tokens").unwrap());
    }

    #[test]
    fn failed_migration_rolls_back_and_keeps_v1_database() {
        let root = temp_root("migrate-fail");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let sessions = root.join("sessions");
        let good = serde_json::to_string(&ChatMessage::user_text("好消息")).unwrap();
        let (id, path) = create_v1_database(&sessions, &workspace, &[&good, "{not json"]);

        let mut manager = SessionManager::create(sessions, &workspace).unwrap();
        let error = manager.load(&id).unwrap_err();
        assert!(
            format!("{:#}", error).contains("迁移中止"),
            "错误应说明迁移失败: {:#}",
            error
        );

        // 原库应保持 v1:messages 表仍在,user_version 仍为 0,数据完整。
        let connection = Connection::open(&path).unwrap();
        assert_eq!(schema_version(&connection).unwrap(), 0);
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);
        let _ = std::fs::remove_dir_all(root);
    }
}
