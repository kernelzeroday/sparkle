//! Conversation history. Messages form a tree through `parent_id`, so regenerating or
//! editing a reply creates a branch and the earlier version is kept.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use sparkle_core::{Message, Role, Usage};
use rusqlite::{Connection, OptionalExtension, params};
use ulid::Ulid;

pub use rusqlite::Error;
pub type Result<T> = rusqlite::Result<T>;

const MIGRATIONS: &[&str] = &["
    CREATE TABLE conversations (
        id TEXT PRIMARY KEY,
        title TEXT NOT NULL,
        model TEXT NOT NULL,
        created_ms INTEGER NOT NULL,
        tip_id TEXT
    );
    CREATE TABLE messages (
        id TEXT PRIMARY KEY,
        conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
        parent_id TEXT REFERENCES messages(id),
        role TEXT NOT NULL,
        content TEXT NOT NULL,
        reasoning TEXT,
        model TEXT,
        input_tokens INTEGER,
        output_tokens INTEGER,
        duration_ms INTEGER,
        created_ms INTEGER NOT NULL
    );
    CREATE INDEX messages_conversation ON messages(conversation_id, created_ms);
"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationSummary {
    pub id: String,
    pub title: String,
    pub model: String,
    pub created_ms: i64,
    pub messages: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredMessage {
    pub id: String,
    pub role: Role,
    pub content: String,
    pub reasoning: Option<String>,
}

impl StoredMessage {
    pub fn to_message(&self) -> Message {
        Message::new(self.role, self.content.clone())
    }
}

/// Everything recorded about a message being appended.
#[derive(Clone, Copy, Debug)]
pub struct NewMessage<'a> {
    pub parent_id: Option<&'a str>,
    pub role: Role,
    pub content: &'a str,
    pub reasoning: Option<&'a str>,
    pub model: Option<&'a str>,
    pub usage: Option<Usage>,
    pub duration_ms: Option<u64>,
}

impl<'a> NewMessage<'a> {
    pub fn plain(parent_id: Option<&'a str>, role: Role, content: &'a str) -> Self {
        Self { parent_id, role, content, reasoning: None, model: None, usage: None, duration_ms: None }
    }
}

pub struct Store {
    connection: Connection,
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_millis() as i64)
}

fn title_from(prompt: &str) -> String {
    let line = prompt.lines().find(|line| !line.trim().is_empty()).unwrap_or("").trim();
    line.chars().take(60).collect()
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        Self::prepare(Connection::open(path)?)
    }

    pub fn in_memory() -> Result<Self> {
        Self::prepare(Connection::open_in_memory()?)
    }

    fn prepare(connection: Connection) -> Result<Self> {
        connection.pragma_update(None, "foreign_keys", "ON")?;
        let _: String = connection.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        for (index, migration) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            connection.execute_batch(migration)?;
            connection.pragma_update(None, "user_version", index as i64 + 1)?;
        }
        Ok(Self { connection })
    }

    pub fn create_conversation(&self, first_prompt: &str, model: &str) -> Result<String> {
        let id = Ulid::generate().to_string().to_lowercase();
        self.connection.execute(
            "INSERT INTO conversations (id, title, model, created_ms) VALUES (?1, ?2, ?3, ?4)",
            params![id, title_from(first_prompt), model, now_ms()],
        )?;
        Ok(id)
    }

    /// Appends a message and makes it the conversation's current tip.
    pub fn append(&self, conversation_id: &str, message: NewMessage<'_>) -> Result<String> {
        let id = Ulid::generate().to_string().to_lowercase();
        self.connection.execute(
            "INSERT INTO messages (id, conversation_id, parent_id, role, content, reasoning, model,
                                   input_tokens, output_tokens, duration_ms, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                id,
                conversation_id,
                message.parent_id,
                message.role.as_str(),
                message.content,
                message.reasoning,
                message.model,
                message.usage.map(|usage| usage.input_tokens as i64),
                message.usage.map(|usage| usage.output_tokens as i64),
                message.duration_ms.map(|value| value as i64),
                now_ms(),
            ],
        )?;
        self.set_tip(conversation_id, &id)?;
        Ok(id)
    }

    pub fn set_tip(&self, conversation_id: &str, message_id: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE conversations SET tip_id = ?2 WHERE id = ?1",
            params![conversation_id, message_id],
        )?;
        Ok(())
    }

    pub fn latest_conversation(&self) -> Result<Option<String>> {
        self.connection
            .query_row("SELECT id FROM conversations ORDER BY id DESC LIMIT 1", [], |row| row.get(0))
            .optional()
    }

    pub fn conversation_title(&self, conversation_id: &str) -> Result<Option<String>> {
        self.connection
            .query_row("SELECT title FROM conversations WHERE id = ?1", [conversation_id], |row| {
                row.get(0)
            })
            .optional()
    }

    pub fn set_title(&self, conversation_id: &str, title: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE conversations SET title = ?2 WHERE id = ?1",
            params![conversation_id, title],
        )?;
        Ok(())
    }

    pub fn conversation_model(&self, conversation_id: &str) -> Result<Option<String>> {
        self.connection
            .query_row("SELECT model FROM conversations WHERE id = ?1", [conversation_id], |row| {
                row.get(0)
            })
            .optional()
    }

    /// Returns the active branch from root to tip.
    pub fn thread(&self, conversation_id: &str) -> Result<Vec<StoredMessage>> {
        let mut statement = self.connection.prepare(
            "WITH RECURSIVE branch(id, parent_id, role, content, reasoning, depth) AS (
                 SELECT m.id, m.parent_id, m.role, m.content, m.reasoning, 0
                 FROM messages m JOIN conversations c ON c.tip_id = m.id
                 WHERE c.id = ?1
                 UNION ALL
                 SELECT m.id, m.parent_id, m.role, m.content, m.reasoning, branch.depth + 1
                 FROM messages m JOIN branch ON m.id = branch.parent_id
             )
             SELECT id, role, content, reasoning FROM branch ORDER BY depth DESC",
        )?;
        let rows = statement.query_map([conversation_id], |row| {
            let role: String = row.get(1)?;
            Ok(StoredMessage {
                id: row.get(0)?,
                role: Role::parse(&role).unwrap_or(Role::User),
                content: row.get(2)?,
                reasoning: row.get(3)?,
            })
        })?;
        rows.collect()
    }

    /// Returns the newest conversations. A limit of zero returns all of them.
    pub fn recent(&self, limit: usize) -> Result<Vec<ConversationSummary>> {
        let limit = if limit == 0 { -1 } else { limit as i64 };
        let mut statement = self.connection.prepare(
            "SELECT c.id, c.title, c.model, c.created_ms,
                    (SELECT COUNT(*) FROM messages m WHERE m.conversation_id = c.id)
             FROM conversations c ORDER BY c.id DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit], |row| {
            Ok(ConversationSummary {
                id: row.get(0)?,
                title: row.get(1)?,
                model: row.get(2)?,
                created_ms: row.get(3)?,
                messages: row.get(4)?,
            })
        })?;
        rows.collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_follows_the_active_branch_after_regeneration() {
        let store = Store::in_memory().unwrap();
        let conversation = store.create_conversation("hello\nworld", "mock").unwrap();
        let user = store
            .append(&conversation, NewMessage::plain(None, Role::User, "hello"))
            .unwrap();
        store
            .append(&conversation, NewMessage::plain(Some(&user), Role::Assistant, "first"))
            .unwrap();
        store
            .append(&conversation, NewMessage::plain(Some(&user), Role::Assistant, "second"))
            .unwrap();

        let thread = store.thread(&conversation).unwrap();
        let contents: Vec<_> = thread.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(contents, ["hello", "second"]);

        let recent = store.recent(5).unwrap();
        assert_eq!(recent[0].title, "hello");
        assert_eq!(recent[0].messages, 3);
    }

    #[test]
    fn titles_can_be_replaced() {
        let store = Store::in_memory().unwrap();
        let conversation = store.create_conversation("first words", "mock").unwrap();
        assert_eq!(store.conversation_title(&conversation).unwrap().unwrap(), "first words");
        store.set_title(&conversation, "Better name").unwrap();
        assert_eq!(store.conversation_title(&conversation).unwrap().unwrap(), "Better name");
        assert_eq!(store.conversation_title("missing").unwrap(), None);
    }

    #[test]
    fn latest_conversation_is_the_newest() {
        let store = Store::in_memory().unwrap();
        assert_eq!(store.latest_conversation().unwrap(), None);
        store.create_conversation("a", "mock").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let newest = store.create_conversation("b", "mock").unwrap();
        assert_eq!(store.latest_conversation().unwrap(), Some(newest));
    }
}
