//! # 本地存储
//!
//! Onemore 把机器级数据放在用户主目录的 `.onemore/`：
//! - `config.toml` 是所有 workspace 共用的配置；
//! - `sessions/<uuid>.db` 是一会话一库的 SQLite 历史。
//!
//! 数据库只保存厂商无关的 [`ChatMessage`] JSON。这样 provider 报文格式、
//! TUI 渲染方式发生变化时，存储格式仍然只依赖项目自己的统一消息模型。
//! 多条有因果约束的消息可以通过 [`SessionStore::append_batch`] 原子提交，
//! 尤其用于保证 ToolUse 与 ToolResult 不会因进程中断只写入一半。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

use crate::message::{Block, ChatMessage, Role, Usage};

pub const APP_HOME_ENV: &str = "ONEMORE_HOME";
pub const APP_DIR_NAME: &str = ".onemore";

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub root: PathBuf,
    pub config: PathBuf,
    pub sessions: PathBuf,
}

impl AppPaths {
    /// `ONEMORE_HOME` 是测试和便携安装的显式覆盖；正常运行使用 `~/.onemore`。
    pub fn discover() -> Result<Self> {
        let root = match std::env::var_os(APP_HOME_ENV) {
            Some(path) if !path.is_empty() => PathBuf::from(path),
            _ => user_home_dir()?.join(APP_DIR_NAME),
        };
        Ok(Self::from_root(root))
    }

    pub fn from_root(root: PathBuf) -> Self {
        AppPaths {
            config: root.join("config.toml"),
            sessions: root.join("sessions"),
            root,
        }
    }

    pub fn ensure(&self) -> Result<()> {
        std::fs::create_dir_all(&self.sessions)
            .with_context(|| format!("创建数据目录 {} 失败", self.sessions.display()))
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

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
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

    pub fn append_batch(&mut self, messages: &[ChatMessage], usage: Usage) -> Result<()> {
        self.current.append_batch(messages, usage)
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
    pub fn load(&mut self, requested_id: &str) -> Result<(Vec<ChatMessage>, Usage)> {
        let id = self.resolve_id(requested_id)?;
        let next = SessionStore::open(&self.sessions_dir, &id, &self.workspace)?;
        let loaded = next.load_messages()?;
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
}

impl SessionStore {
    fn create(sessions_dir: &Path, workspace: &str) -> Result<Self> {
        let id = uuid::Uuid::new_v4().to_string();
        let path = sessions_dir.join(format!("{}.db", id));
        let connection = Connection::open(&path)
            .with_context(|| format!("创建会话数据库 {} 失败", path.display()))?;
        initialize(&connection)?;
        let now = unix_timestamp();
        connection.execute(
            "INSERT INTO session (id, workspace, title, created_at, updated_at, input_tokens, output_tokens) \
             VALUES (?1, ?2, '', ?3, ?3, 0, 0)",
            params![id, workspace, now],
        )?;
        Ok(SessionStore { id, connection })
    }

    fn open(sessions_dir: &Path, id: &str, workspace: &str) -> Result<Self> {
        let path = sessions_dir.join(format!("{}.db", id));
        let connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("打开会话数据库 {} 失败", path.display()))?;
        initialize(&connection)?;
        let stored_workspace: Option<String> = connection
            .query_row("SELECT workspace FROM session WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()?;
        match stored_workspace {
            Some(stored) if stored == workspace => {}
            Some(_) => bail!("会话 {} 不属于当前 workspace", id),
            None => bail!("会话数据库 {} 缺少元数据", path.display()),
        }
        Ok(SessionStore {
            id: id.to_string(),
            connection,
        })
    }

    fn append_batch(&mut self, messages: &[ChatMessage], usage: Usage) -> Result<()> {
        if messages.is_empty() {
            return Ok(());
        }
        let tx = self.connection.transaction()?;
        let mut first_user_text = String::new();
        for message in messages {
            let payload = serde_json::to_string(message).context("序列化会话消息失败")?;
            tx.execute("INSERT INTO messages (payload) VALUES (?1)", [payload])?;
            if first_user_text.is_empty() && message.role == Role::User {
                first_user_text = message
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
        tx.execute(
            "UPDATE session SET \
             title = CASE WHEN title = '' AND ?1 <> '' THEN ?1 ELSE title END, \
             updated_at = ?2, input_tokens = ?3, output_tokens = ?4 WHERE id = ?5",
            params![
                first_user_text,
                unix_timestamp(),
                usage.input_tokens,
                usage.output_tokens,
                self.id
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn clear(&mut self) -> Result<()> {
        let tx = self.connection.transaction()?;
        tx.execute("DELETE FROM messages", [])?;
        tx.execute(
            "UPDATE session SET title = '', updated_at = ?1, input_tokens = 0, output_tokens = 0 \
             WHERE id = ?2",
            params![unix_timestamp(), self.id],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn load_messages(&self) -> Result<(Vec<ChatMessage>, Usage)> {
        let mut statement = self
            .connection
            .prepare("SELECT payload FROM messages ORDER BY sequence")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut messages = Vec::new();
        for row in rows {
            let payload = row?;
            messages.push(serde_json::from_str(&payload).context("会话数据库含有无法解析的消息")?);
        }
        let usage = self.connection.query_row(
            "SELECT input_tokens, output_tokens FROM session WHERE id = ?1",
            [&self.id],
            |row| {
                Ok(Usage {
                    input_tokens: row.get(0)?,
                    output_tokens: row.get(1)?,
                })
            },
        )?;
        Ok((messages, usage))
    }

    fn read_summary(path: &Path, workspace: &str) -> Result<Option<SessionSummary>> {
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection
            .query_row(
                "SELECT id, title, updated_at, (SELECT COUNT(*) FROM messages) \
                 FROM session WHERE workspace = ?1 LIMIT 1",
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

fn initialize(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS session (
             id TEXT PRIMARY KEY,
             workspace TEXT NOT NULL,
             title TEXT NOT NULL,
             created_at INTEGER NOT NULL,
             updated_at INTEGER NOT NULL,
             input_tokens INTEGER NOT NULL,
             output_tokens INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS messages (
             sequence INTEGER PRIMARY KEY AUTOINCREMENT,
             payload TEXT NOT NULL
         );",
    )?;
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

    fn temp_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "onemore-storage-{}-{}-{}",
            name,
            std::process::id(),
            uuid::Uuid::new_v4()
        ))
    }

    #[test]
    fn persists_lists_loads_and_clears_session() {
        let root = temp_root("roundtrip");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let sessions = root.join("sessions");
        let mut manager = SessionManager::create(sessions.clone(), &workspace).unwrap();
        let id = manager.current_id().to_string();
        let messages = vec![
            ChatMessage::user_text("第一条问题"),
            ChatMessage {
                role: Role::Assistant,
                blocks: vec![Block::Text("回答".into())],
            },
        ];
        manager
            .append_batch(
                &messages,
                Usage {
                    input_tokens: 12,
                    output_tokens: 7,
                },
            )
            .unwrap();

        let listed = manager.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].title, "第一条问题");
        assert_eq!(listed[0].message_count, 2);

        let mut other = SessionManager::create(sessions, &workspace).unwrap();
        let (loaded, usage) = other.load(&id[..8]).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].text(), "第一条问题");
        assert_eq!(usage.input_tokens, 12);
        other.clear().unwrap();
        assert!(other.load(&id).unwrap().0.is_empty());

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
        let messages = vec![
            ChatMessage::user_text("读取文件"),
            ChatMessage {
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
            },
            ChatMessage {
                role: Role::User,
                blocks: vec![Block::ToolResult {
                    tool_use_id: "call-1".into(),
                    content: "file body".into(),
                    is_error: false,
                }],
            },
        ];
        manager.append_batch(&messages, Usage::default()).unwrap();

        let mut reopened = SessionManager::create(sessions, &workspace).unwrap();
        let (loaded, _) = reopened.load(&id).unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[1].tool_uses()[0].0, "call-1");
        match &loaded[2].blocks[0] {
            Block::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id, "call-1");
                assert_eq!(content, "file body");
                assert!(!is_error);
            }
            other => panic!("应恢复 ToolResult，得到 {:?}", other),
        }
        let _ = std::fs::remove_dir_all(root);
    }
}
