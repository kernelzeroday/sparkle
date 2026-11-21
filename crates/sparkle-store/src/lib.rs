//! Conversation history. Messages form a tree through `parent_id`, so regenerating or
//! editing a reply creates a branch and the earlier version is kept. Compaction works the same
//! way: the summary starts a new branch, and the messages it replaces stay on the old one.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use sparkle_core::{Attachment, Message, Role, Usage};
use rusqlite::{Connection, OptionalExtension, params};
use ulid::Ulid;

pub use rusqlite::Error;
pub type Result<T> = rusqlite::Result<T>;

const MIGRATIONS: &[&str] = &[
    "
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
",
    // Set on a compaction summary, pointing at the tip of the branch it replaced.
    "ALTER TABLE messages ADD COLUMN compacted_from TEXT REFERENCES messages(id);",
    // Attachments sent alongside a message (URLs or inline base64 payloads).
    "
    CREATE TABLE message_attachments (
        id TEXT PRIMARY KEY,
        message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
        ord INTEGER NOT NULL,
        kind TEXT NOT NULL,
        mime TEXT,
        data TEXT NOT NULL
    );
    CREATE INDEX message_attachments_message ON message_attachments(message_id, ord);
",
    // Metadata recorded on inline compaction entries for display and resume.
    "
    ALTER TABLE messages ADD COLUMN compaction_method TEXT;
    ALTER TABLE messages ADD COLUMN compaction_tokens_before INTEGER;
    ALTER TABLE messages ADD COLUMN compaction_tokens_after INTEGER;
",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationSummary {
    pub id: String,
    pub title: String,
    pub model: String,
    pub created_ms: i64,
    pub messages: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactionInfo {
    pub method: String,
    pub tokens_before: u64,
    pub tokens_after: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredMessage {
    pub id: String,
    pub role: Role,
    pub content: String,
    pub reasoning: Option<String>,
    pub attachments: Vec<Attachment>,
    /// For a compaction summary, the last message before compaction took effect.
    pub compacted_from: Option<String>,
    /// Method and token counts recorded when this entry compacted earlier turns.
    pub compaction: Option<CompactionInfo>,
}

impl StoredMessage {
    pub fn is_compaction(&self) -> bool {
        self.compaction.is_some() || self.compacted_from.is_some()
    }

    pub fn to_message(&self) -> Message {
        Message {
            role: self.role,
            content: self.content.clone(),
            attachments: self.attachments.clone(),
        }
    }
}

/// Returns the non-system messages that belong in the active LLM context (starting at the latest
/// compaction boundary, if any).
pub fn active_context(history: &[StoredMessage]) -> Vec<&StoredMessage> {
    let start = history.iter().rposition(StoredMessage::is_compaction).unwrap_or(0);
    history[start..].iter().filter(|message| message.role != Role::System).collect()
}

/// Everything recorded about a message being appended.
#[derive(Clone, Copy, Debug)]
pub struct NewMessage<'a> {
    pub parent_id: Option<&'a str>,
    pub role: Role,
    pub content: &'a str,
    pub reasoning: Option<&'a str>,
    pub attachments: &'a [Attachment],
    pub model: Option<&'a str>,
    pub usage: Option<Usage>,
    pub duration_ms: Option<u64>,
}

impl<'a> NewMessage<'a> {
    pub fn plain(parent_id: Option<&'a str>, role: Role, content: &'a str) -> Self {
        Self {
            parent_id,
            role,
            content,
            reasoning: None,
            attachments: &[],
            model: None,
            usage: None,
            duration_ms: None,
        }
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

    /// Removes a conversation. The cascades on `conversation_id` and `message_id` take its
    /// messages and their attachments with it. Returns whether it existed.
    pub fn delete_conversation(&self, conversation_id: &str) -> Result<bool> {
        let removed = self
            .connection
            .execute("DELETE FROM conversations WHERE id = ?1", [conversation_id])?;
        Ok(removed > 0)
    }

    /// Removes every conversation along with everything stored in it. Returns how many there
    /// were.
    pub fn delete_all_conversations(&self) -> Result<usize> {
        Ok(self.connection.execute("DELETE FROM conversations", [])?)
    }

    /// Appends a message and makes it the conversation's current tip.
    pub fn append(&self, conversation_id: &str, message: NewMessage<'_>) -> Result<String> {
        let transaction = self.connection.unchecked_transaction()?;
        let id = self.insert(conversation_id, message, None, None)?;
        transaction.commit()?;
        Ok(id)
    }

    /// Appends an inline compaction entry onto the active branch. Prior messages stay on the same
    /// branch so the display transcript remains intact across resume, while LLM context resets at
    /// this entry.
    pub fn append_compaction(
        &self,
        conversation_id: &str,
        summary: NewMessage<'_>,
        compacted_from: &str,
        info: &CompactionInfo,
    ) -> Result<String> {
        let transaction = self.connection.unchecked_transaction()?;
        let id = self.insert(conversation_id, summary, Some(compacted_from), Some(info))?;
        transaction.commit()?;
        Ok(id)
    }

    /// Starts a new branch from a summary of the branch ending at `compacted_from`, followed by
    /// copies of the `kept` messages, whose own `parent_id` is ignored. Nothing is written unless
    /// everything is. Returns the ids of the summary and of the kept messages.
    pub fn compact(
        &self,
        conversation_id: &str,
        summary: NewMessage<'_>,
        compacted_from: &str,
        kept: &[NewMessage<'_>],
    ) -> Result<(String, Vec<String>)> {
        let transaction = self.connection.unchecked_transaction()?;
        let summary_id = self.insert(conversation_id, summary, Some(compacted_from), None)?;
        let mut kept_ids: Vec<String> = Vec::with_capacity(kept.len());
        for message in kept {
            let parent_id = Some(kept_ids.last().unwrap_or(&summary_id).as_str());
            kept_ids.push(self.insert(
                conversation_id,
                NewMessage { parent_id, ..*message },
                None,
                None,
            )?);
        }
        transaction.commit()?;
        Ok((summary_id, kept_ids))
    }

    fn insert(
        &self,
        conversation_id: &str,
        message: NewMessage<'_>,
        compacted_from: Option<&str>,
        compaction: Option<&CompactionInfo>,
    ) -> Result<String> {
        let id = Ulid::generate().to_string().to_lowercase();
        self.connection.execute(
            "INSERT INTO messages (id, conversation_id, parent_id, role, content, reasoning, model,
                                   input_tokens, output_tokens, duration_ms, created_ms,
                                   compacted_from, compaction_method, compaction_tokens_before,
                                   compaction_tokens_after)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
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
                compacted_from,
                compaction.map(|info| info.method.as_str()),
                compaction.map(|info| info.tokens_before as i64),
                compaction.map(|info| info.tokens_after as i64),
            ],
        )?;
        for (ord, attachment) in message.attachments.iter().enumerate() {
            let attachment_id = Ulid::generate().to_string().to_lowercase();
            let (kind, mime, data) = match attachment {
                Attachment::Url(url) => ("url", None, url.as_str()),
                Attachment::Inline { mime, base64 } => ("inline", Some(mime.as_str()), base64.as_str()),
            };
            self.connection.execute(
                "INSERT INTO message_attachments (id, message_id, ord, kind, mime, data)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![attachment_id, id, ord as i64, kind, mime, data],
            )?;
        }
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

    fn attachments_for(&self, message_id: &str) -> Result<Vec<Attachment>> {
        let mut statement = self.connection.prepare(
            "SELECT kind, mime, data FROM message_attachments
             WHERE message_id = ?1 ORDER BY ord ASC",
        )?;
        let rows = statement.query_map([message_id], |row| {
            let kind: String = row.get(0)?;
            let mime: Option<String> = row.get(1)?;
            let data: String = row.get(2)?;
            Ok(match kind.as_str() {
                "url" => Attachment::Url(data),
                _ => Attachment::Inline {
                    mime: mime.unwrap_or_else(|| "application/octet-stream".to_string()),
                    base64: data,
                },
            })
        })?;
        rows.collect()
    }

    /// Returns the active branch from root to tip.
    pub fn thread(&self, conversation_id: &str) -> Result<Vec<StoredMessage>> {
        let mut statement = self.connection.prepare(
            "WITH RECURSIVE branch(
                 id, parent_id, role, content, reasoning, compacted_from,
                 compaction_method, compaction_tokens_before, compaction_tokens_after, depth
             ) AS (
                 SELECT m.id, m.parent_id, m.role, m.content, m.reasoning, m.compacted_from,
                        m.compaction_method, m.compaction_tokens_before, m.compaction_tokens_after, 0
                 FROM messages m JOIN conversations c ON c.tip_id = m.id
                 WHERE c.id = ?1
                 UNION ALL
                 SELECT m.id, m.parent_id, m.role, m.content, m.reasoning, m.compacted_from,
                        m.compaction_method, m.compaction_tokens_before, m.compaction_tokens_after,
                        branch.depth + 1
                 FROM messages m JOIN branch ON m.id = branch.parent_id
             )
             SELECT id, role, content, reasoning, compacted_from,
                    compaction_method, compaction_tokens_before, compaction_tokens_after
             FROM branch ORDER BY depth DESC",
        )?;
        let mut messages: Vec<StoredMessage> = statement
            .query_map([conversation_id], |row| {
                let role: String = row.get(1)?;
                let compacted_from: Option<String> = row.get(4)?;
                let method: Option<String> = row.get(5)?;
                let tokens_before: Option<i64> = row.get(6)?;
                let tokens_after: Option<i64> = row.get(7)?;
                let compaction = method.map(|method| CompactionInfo {
                    method,
                    tokens_before: tokens_before.unwrap_or(0).max(0) as u64,
                    tokens_after: tokens_after.unwrap_or(0).max(0) as u64,
                });
                Ok(StoredMessage {
                    id: row.get(0)?,
                    role: Role::parse(&role).unwrap_or(Role::User),
                    content: row.get(2)?,
                    reasoning: row.get(3)?,
                    attachments: Vec::new(),
                    compacted_from,
                    compaction,
                })
            })?
            .collect::<Result<_>>()?;
        for message in &mut messages {
            message.attachments = self.attachments_for(&message.id)?;
        }
        Ok(messages)
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

    /// A conversation of system prompt, question and answer. Returns its id and those messages.
    fn exchange(store: &Store) -> (String, String, String) {
        let conversation = store.create_conversation("question", "mock").unwrap();
        let system =
            store.append(&conversation, NewMessage::plain(None, Role::System, "Be brief")).unwrap();
        let user = NewMessage::plain(Some(&system), Role::User, "question");
        let user = store.append(&conversation, user).unwrap();
        let reply = NewMessage::plain(Some(&user), Role::Assistant, "answer");
        let reply = store.append(&conversation, reply).unwrap();
        (conversation, system, reply)
    }

    #[test]
    fn compaction_starts_a_branch_and_keeps_the_original_messages() {
        let store = Store::in_memory().unwrap();
        let (conversation, system, tip) = exchange(&store);
        let summary = NewMessage::plain(Some(&system), Role::User, "summary");
        let kept = [NewMessage::plain(None, Role::User, "unanswered")];
        let (summary_id, kept_ids) = store.compact(&conversation, summary, &tip, &kept).unwrap();

        let thread = store.thread(&conversation).unwrap();
        let contents: Vec<_> = thread.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(contents, ["Be brief", "summary", "unanswered"]);
        assert_eq!(thread[1].id, summary_id);
        assert_eq!(thread[1].compacted_from.as_deref(), Some(tip.as_str()));
        assert_eq!(thread[2].id, kept_ids[0]);
        assert_eq!(thread[2].compacted_from, None);
        assert_eq!(store.recent(1).unwrap()[0].messages, 5, "the originals are still stored");

        store.set_tip(&conversation, &tip).unwrap();
        let original = store.thread(&conversation).unwrap();
        let contents: Vec<_> = original.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(contents, ["Be brief", "question", "answer"]);
    }

    #[test]
    fn a_failed_compaction_writes_nothing() {
        let store = Store::in_memory().unwrap();
        let (conversation, system, tip) = exchange(&store);
        let summary = NewMessage::plain(Some(&system), Role::User, "summary");
        // The foreign key on compacted_from rejects a message that does not exist.
        assert!(store.compact(&conversation, summary, "missing", &[]).is_err());

        let thread = store.thread(&conversation).unwrap();
        assert_eq!(thread.last().unwrap().id, tip);
        assert_eq!(store.recent(1).unwrap()[0].messages, 3);
    }

    #[test]
    fn databases_from_before_compaction_are_upgraded_in_place() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch(MIGRATIONS[0]).unwrap();
        connection.pragma_update(None, "user_version", 1).unwrap();
        connection
            .execute_batch(
                "INSERT INTO conversations (id, title, model, created_ms, tip_id)
                     VALUES ('c', 'Old chat', 'mock', 1, 'm');
                 INSERT INTO messages (id, conversation_id, role, content, created_ms)
                     VALUES ('m', 'c', 'user', 'old words', 1);",
            )
            .unwrap();

        let store = Store::prepare(connection).unwrap();
        let thread = store.thread("c").unwrap();
        assert_eq!(thread[0].content, "old words");
        assert_eq!(thread[0].compacted_from, None);
        let version: i64 =
            store.connection.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
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

    #[test]
    fn attachments_survive_round_trip_on_thread() {
        let store = Store::in_memory().unwrap();
        let conversation = store.create_conversation("look at this", "mock").unwrap();
        let attachments = vec![
            Attachment::Url("https://example.com/a.png".into()),
            Attachment::Inline { mime: "image/png".into(), base64: "AQID".into() },
        ];
        let message = NewMessage {
            attachments: &attachments,
            ..NewMessage::plain(None, Role::User, "look at this")
        };
        store.append(&conversation, message).unwrap();

        let thread = store.thread(&conversation).unwrap();
        assert_eq!(thread.len(), 1);
        assert_eq!(thread[0].attachments, attachments);
        assert_eq!(thread[0].to_message().attachments, attachments);
    }

    #[test]
    fn inline_compaction_keeps_full_thread_and_resets_active_context() {
        let store = Store::in_memory().unwrap();
        let (conversation, _system, tip) = exchange(&store);
        let info = CompactionInfo {
            method: "handoff".into(),
            tokens_before: 1200,
            tokens_after: 180,
        };
        let summary = NewMessage::plain(Some(&tip), Role::User, "summary");
        let summary_id = store.append_compaction(&conversation, summary, &tip, &info).unwrap();
        store.append(&conversation, NewMessage::plain(Some(&summary_id), Role::User, "next")).unwrap();

        let thread = store.thread(&conversation).unwrap();
        let contents: Vec<_> = thread.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(contents, ["Be brief", "question", "answer", "summary", "next"]);
        assert_eq!(thread[3].compaction.as_ref(), Some(&info));

        let active: Vec<_> = active_context(&thread).iter().map(|m| m.content.as_str()).collect();
        assert_eq!(active, ["summary", "next"]);
    }

    #[test]
    fn removing_a_conversation_takes_its_messages_and_attachments() {
        let store = Store::in_memory().unwrap();
        let (conversation, system, _tip) = exchange(&store);
        let attachments = vec![Attachment::Url("https://example.com/a.png".into())];
        let message = NewMessage {
            attachments: &attachments,
            ..NewMessage::plain(Some(&system), Role::User, "with a file")
        };
        store.append(&conversation, message).unwrap();

        assert!(store.delete_conversation(&conversation).unwrap());
        assert!(!store.delete_conversation(&conversation).unwrap(), "gone on the second pass");
        assert!(store.recent(10).unwrap().is_empty());
        let messages: i64 = store
            .connection
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .unwrap();
        let attachments: i64 = store
            .connection
            .query_row("SELECT COUNT(*) FROM message_attachments", [], |row| row.get(0))
            .unwrap();
        assert_eq!((messages, attachments), (0, 0));
    }

    #[test]
    fn removing_one_conversation_leaves_the_others() {
        let store = Store::in_memory().unwrap();
        let (kept, _, _) = exchange(&store);
        let (gone, _, _) = exchange(&store);
        assert!(store.delete_conversation(&gone).unwrap());
        let recent = store.recent(10).unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].id, kept);
    }

    #[test]
    fn removing_everything_reports_how_many_went() {
        let store = Store::in_memory().unwrap();
        exchange(&store);
        exchange(&store);
        assert_eq!(store.delete_all_conversations().unwrap(), 2);
        assert!(store.recent(10).unwrap().is_empty());
        assert_eq!(store.delete_all_conversations().unwrap(), 0);
    }
}
