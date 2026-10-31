//! What the agent remembers after the conversation scrolls away.
//!
//! Two layers, and the split is what keeps the prompt small:
//!
//! - **entries** — one Markdown file per thing worth remembering, carrying frontmatter and a
//!   title. A file can be a page long, because a file is not in the prompt.
//! - **the index** — `memory/MEMORY.md`, one line per entry. This is the only part that enters a
//!   prompt, and the agent reads a body with the `read` tool when the index tells it there is
//!   something to read.
//!
//! Three rules are structural rather than stylistic:
//!
//! - **Retirement is a `status`, never a delete.** A deleted entry is a fact the agent can
//!   silently repeat; a `superseded` entry is one it can see it once believed something else.
//! - **The index is a pointer list.** One line per entry, and the entry's detail stays in the
//!   file — an index that accumulates detail is the second copy that eventually disagrees with
//!   the first.
//! - **A superseded entry keeps its row.** The row is how a later reader learns the fact changed.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The index's own file name, inside the memory directory.
pub const INDEX_NAME: &str = "MEMORY.md";

/// How many rows the index may hold before it has to be consolidated.
pub const MAX_INDEX_ROWS: usize = 100;

/// How many characters one index row may carry (detail belongs in the file, not the row).
pub const MAX_ROW_CHARS: usize = 512;

/// How many characters of a title a row carries. Well under [`MAX_ROW_CHARS`] so that the id, the
/// kind, the status and the timestamp still fit inside the row's own cap.
pub const MAX_TITLE_CHARS: usize = 160;

/// The index's header, written when the file is first created.
const INDEX_HEADER: &str = "# Memory Index\n\n\
| ID | Type | Title | Status | Updated |\n\
|----|------|-------|--------|---------|\n";

/// One memory entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntry {
    /// The file's stem, and how the index and the agent refer to this entry.
    pub id: String,
    /// The kind of thing it is: `task`, `project`, `feedback`, `decision`, …
    pub kind: String,
    pub title: String,
    /// `active`, `merged` or `superseded` — the retirement mechanism.
    pub status: String,
    /// When the event the entry is about happened, where that differs from when it was written.
    pub event_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// The body, without the title heading.
    pub body: String,
}

impl MemoryEntry {
    /// A new active entry, with its id derived from the title.
    pub fn new(kind: impl Into<String>, title: impl Into<String>, body: impl Into<String>) -> Self {
        let title = title.into();
        let now = super::session::now_iso();
        Self {
            id: slug_id(&title),
            kind: kind.into(),
            title,
            status: "active".to_string(),
            event_at: None,
            created_at: now.clone(),
            updated_at: now,
            body: body.into(),
        }
    }

    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    pub fn with_status(mut self, status: impl Into<String>) -> Self {
        self.status = status.into();
        self
    }

    /// The file's contents: frontmatter, the title as a heading, then the body.
    ///
    /// The title is written twice on purpose — once as a field a program reads, once as a heading
    /// a person reads — because a file opened in an editor should say what it is at the top.
    pub fn to_markdown(&self) -> String {
        let mut out = String::from("---\n");
        out.push_str(&format!("id: {}\n", self.id));
        out.push_str(&format!("type: {}\n", self.kind));
        out.push_str(&format!("title: {}\n", self.title));
        out.push_str(&format!("status: {}\n", self.status));
        if let Some(event_at) = &self.event_at {
            out.push_str(&format!("event_at: {event_at}\n"));
        }
        out.push_str(&format!("created_at: {}\n", self.created_at));
        out.push_str(&format!("updated_at: {}\n", self.updated_at));
        out.push_str("---\n\n");
        out.push_str(&format!("# {}\n\n", self.title));
        out.push_str(self.body.trim_end());
        out.push('\n');
        out
    }

    /// Read an entry back, or `None` when the text is not one.
    ///
    /// A file with no frontmatter is not an entry: it is something a user put there, and treating
    /// it as an entry would let this store overwrite it on the next save.
    pub fn parse(text: &str, filename: &str) -> Option<Self> {
        let mut lines = text.lines();
        if lines.next()?.trim() != "---" {
            return None;
        }
        let mut fields: Vec<(String, String)> = Vec::new();
        let mut closed = false;
        for line in lines.by_ref() {
            let line = line.trim_end();
            if line.trim() == "---" {
                closed = true;
                break;
            }
            if let Some((key, value)) = line.split_once(':') {
                fields.push((key.trim().to_string(), value.trim().to_string()));
            }
        }
        if !closed {
            return None;
        }
        let get = |name: &str| {
            fields
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        let body_lines: Vec<&str> = lines.collect();
        let mut body = body_lines.join("\n");
        // The heading repeats the title; drop it so the body is what the field does not say.
        let trimmed = body.trim_start_matches('\n');
        if trimmed.starts_with('#') {
            if let Some((_, rest)) = trimmed.split_once('\n') {
                body = rest.to_string();
            } else {
                body = String::new();
            }
        }
        Some(Self {
            id: get("id").unwrap_or_else(|| filename.trim_end_matches(".md").to_string()),
            kind: get("type").unwrap_or_else(|| "note".to_string()),
            title: get("title").unwrap_or_else(|| filename.to_string()),
            status: get("status").unwrap_or_else(|| "active".to_string()),
            event_at: get("event_at"),
            created_at: get("created_at").unwrap_or_default(),
            updated_at: get("updated_at").unwrap_or_default(),
            body: body.trim().to_string(),
        })
    }

    /// The one index line for this entry.
    ///
    /// Truncated rather than wrapped: a row that grows into a paragraph is the detail the file
    /// already holds.
    pub fn row(&self) -> String {
        let title = one_line(&self.title);
        let title = truncate(&title, MAX_TITLE_CHARS);
        format!(
            "| {} | {} | {} | {} | {} |",
            self.id, self.kind, title, self.status, self.updated_at
        )
    }

    /// Whether this entry has been retired.
    pub fn is_retired(&self) -> bool {
        self.status != "active"
    }
}

/// A filename-safe id from a title.
///
/// Deterministic on the title, so the same topic lands on the same file instead of accumulating
/// near-duplicates; a caller that wants a second entry on the same topic supplies its own id.
pub fn slug_id(title: &str) -> String {
    let mut out = String::new();
    let mut last_dash = true;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && out.chars().count() < 48 {
            out.push('-');
            last_dash = true;
        }
        if out.chars().count() >= 48 {
            break;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        format!("entry-{}", std::process::id())
    } else {
        trimmed
    }
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate(text: &str, budget: usize) -> String {
    if text.chars().count() <= budget {
        return text.to_string();
    }
    let kept: String = text.chars().take(budget.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// One app's memory directory.
#[derive(Debug, Clone)]
pub struct MemoryStore {
    dir: PathBuf,
}

impl MemoryStore {
    /// Open (creating if needed) a memory directory.
    pub fn open(dir: impl Into<PathBuf>) -> std::io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn index_path(&self) -> PathBuf {
        self.dir.join(INDEX_NAME)
    }

    /// The index text, created with its header on first use.
    pub fn index(&self) -> std::io::Result<String> {
        match fs::read_to_string(self.index_path()) {
            Ok(text) => Ok(text),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(INDEX_HEADER.to_string()),
            Err(err) => Err(err),
        }
    }

    /// The index text as it should appear in a prompt: nothing when there are no entries.
    ///
    /// Empty rather than a header with no rows, because a header on its own tells a model that
    /// there is a memory system to consult and nothing in it, which reads as "you have forgotten
    /// everything".
    pub fn index_for_prompt(&self) -> std::io::Result<String> {
        let index = self.index()?;
        let has_rows = index.lines().any(|line| {
            line.trim_start().starts_with('|') && !line.contains("---") && !line.contains("| ID |")
        });
        Ok(if has_rows { index } else { String::new() })
    }

    /// Write one entry and put its row in the index.
    ///
    /// The file first, then the index: a crash between them leaves an entry the index does not
    /// name (recoverable by a rebuild) rather than an index naming an entry that is not there.
    pub fn save(&self, entry: &MemoryEntry) -> std::io::Result<PathBuf> {
        let path = self.dir.join(format!("{}.md", entry.id));
        self.write_atomic(&path, entry.to_markdown().as_bytes())?;
        self.put_row(entry)?;
        Ok(path)
    }

    /// Retire an entry — set its status, keep the file and its row.
    pub fn retire(&self, id: &str, status: &str) -> std::io::Result<Option<MemoryEntry>> {
        let Some(mut entry) = self.load(id)? else {
            return Ok(None);
        };
        entry.status = status.to_string();
        entry.updated_at = super::session::now_iso();
        self.save(&entry)?;
        Ok(Some(entry))
    }

    /// One entry by id, or `None`.
    pub fn load(&self, id: &str) -> std::io::Result<Option<MemoryEntry>> {
        let path = self.dir.join(format!("{id}.md"));
        match fs::read_to_string(&path) {
            Ok(text) => Ok(MemoryEntry::parse(&text, &format!("{id}.md"))),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Every entry in the directory, in id order.
    pub fn entries(&self) -> std::io::Result<Vec<MemoryEntry>> {
        let mut entries = Vec::new();
        let Ok(read) = fs::read_dir(&self.dir) else {
            return Ok(entries);
        };
        let mut paths: Vec<PathBuf> = read
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension().and_then(|ext| ext.to_str()) == Some("md")
                    && path.file_name().and_then(|name| name.to_str()) != Some(INDEX_NAME)
            })
            .collect();
        paths.sort();
        for path in paths {
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_string();
            if let Some(entry) = MemoryEntry::parse(&text, &name) {
                entries.push(entry);
            }
        }
        Ok(entries)
    }

    /// What the index would be if it were rebuilt from the files present.
    ///
    /// This is the repair path: the files are the truth, the index is a projection of them.
    pub fn rebuilt_index(&self) -> std::io::Result<String> {
        let mut out = INDEX_HEADER.to_string();
        for entry in self.entries()? {
            out.push_str(&entry.row());
            out.push('\n');
        }
        Ok(out)
    }

    /// Rewrite the index from the files, replacing whatever was there.
    pub fn rebuild_index(&self) -> std::io::Result<()> {
        let text = self.rebuilt_index()?;
        self.write_atomic(&self.index_path(), text.as_bytes())
    }

    /// The index's own housekeeping: rows over the limit, rows over the length cap.
    ///
    /// Returned rather than acted on: consolidating entries is a judgement about what belongs
    /// together, and a store that merged rows mechanically would merge two facts that only look
    /// alike.
    pub fn index_warnings(&self) -> std::io::Result<Vec<String>> {
        let index = self.index()?;
        let mut warnings = Vec::new();
        let rows: Vec<&str> = index
            .lines()
            .filter(|line| line.trim_start().starts_with('|') && !line.contains("---"))
            .filter(|line| !line.starts_with("| ID |"))
            .collect();
        if rows.len() > MAX_INDEX_ROWS {
            warnings.push(format!(
                "the memory index has {} rows, over the {MAX_INDEX_ROWS} it may carry: merge \
                 related entries and mark the ones they replaced as merged or superseded",
                rows.len()
            ));
        }
        let truncated = rows.iter().filter(|row| row.contains('…')).count();
        if truncated > 0 {
            warnings.push(format!(
                "{truncated} memory index row(s) had to shorten their title: a row is a pointer, \
                 and the detail belongs in the entry's file rather than in the index"
            ));
        }
        for row in &rows {
            if row.chars().count() > MAX_ROW_CHARS {
                warnings.push(format!(
                    "one memory index row is {} characters, over the {MAX_ROW_CHARS} it may carry",
                    row.chars().count()
                ));
                break;
            }
        }
        Ok(warnings)
    }

    /// Replace this entry's row, or append it. A row's id is its identity, so an update edits.
    fn put_row(&self, entry: &MemoryEntry) -> std::io::Result<()> {
        let index = self.index()?;
        let marker = format!("| {} |", entry.id);
        let mut lines: Vec<String> = index.lines().map(|line| line.to_string()).collect();
        let existing = lines.iter().position(|line| line.starts_with(&marker));
        match existing {
            Some(index) => lines[index] = entry.row(),
            None => {
                // Keep the table contiguous: a row appended after a trailing blank line would
                // render as prose in every Markdown viewer.
                while lines
                    .last()
                    .map(|line| line.trim().is_empty())
                    .unwrap_or(false)
                {
                    lines.pop();
                }
                lines.push(entry.row());
            }
        }
        let mut text = lines.join("\n");
        text.push('\n');
        self.write_atomic(&self.index_path(), text.as_bytes())
    }

    /// Write a file through a temporary in the same directory, then rename.
    ///
    /// A half-written entry is worse than no entry: it parses as nothing, so its row in the index
    /// points at a body that is not there.
    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        let temporary = path.with_extension("tmp");
        {
            let mut file = fs::File::create(&temporary)?;
            file.write_all(bytes)?;
            file.flush()?;
        }
        fs::rename(&temporary, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch {
        base: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let base = std::env::temp_dir().join(format!(
                "deepnest-agent-memory-{name}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(&base).unwrap();
            Self { base }
        }

        fn store(&self) -> MemoryStore {
            MemoryStore::open(self.base.join("memory")).unwrap()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn an_entry_round_trips_through_its_file() {
        let entry = MemoryEntry::new("project", "The app layout", "Apps live under ~/.deepnest.");
        let text = entry.to_markdown();
        let parsed = MemoryEntry::parse(&text, "the-app-layout.md").unwrap();
        assert_eq!(parsed.id, entry.id);
        assert_eq!(parsed.kind, "project");
        assert_eq!(parsed.title, "The app layout");
        assert_eq!(parsed.status, "active");
        assert_eq!(parsed.body, "Apps live under ~/.deepnest.");
    }

    #[test]
    fn a_file_without_frontmatter_is_not_an_entry() {
        // Treating a user's own Markdown as an entry lets the store overwrite their file.
        assert!(MemoryEntry::parse("# my notes\n\nsomething\n", "notes.md").is_none());
        assert!(
            MemoryEntry::parse("---\ntitle: x\n", "broken.md").is_none(),
            "unterminated"
        );
    }

    #[test]
    fn saving_writes_the_file_and_puts_one_row_in_the_index() {
        let scratch = Scratch::new("save");
        let store = scratch.store();
        let entry = MemoryEntry::new(
            "feedback",
            "Do not background commands",
            "They never return.",
        );
        let path = store.save(&entry).unwrap();
        assert!(path.starts_with(store.dir()));
        let index = store.index().unwrap();
        assert!(index.contains("| Do not background commands |"), "{index}");
        assert_eq!(index.matches("| Do not background commands |").count(), 1);
    }

    #[test]
    fn saving_the_same_entry_twice_edits_its_row_rather_than_adding_one() {
        let scratch = Scratch::new("edit-row");
        let store = scratch.store();
        let mut entry = MemoryEntry::new("task", "Port the loop", "first");
        store.save(&entry).unwrap();
        entry.body = "second".to_string();
        entry.updated_at = "2026-10-31T00:00:00Z".to_string();
        store.save(&entry).unwrap();
        let index = store.index().unwrap();
        assert_eq!(index.matches("| Port the loop |").count(), 1, "{index}");
        assert!(index.contains("2026-10-31T00:00:00Z"), "{index}");
        let loaded = store.load(&entry.id).unwrap().unwrap();
        assert_eq!(loaded.body, "second");
    }

    #[test]
    fn retiring_keeps_the_file_and_the_row_and_only_changes_the_status() {
        let scratch = Scratch::new("retire");
        let store = scratch.store();
        let entry = MemoryEntry::new("task", "Old belief", "it used to be so");
        store.save(&entry).unwrap();
        let retired = store.retire(&entry.id, "superseded").unwrap().unwrap();
        assert_eq!(retired.status, "superseded");
        assert!(store.index().unwrap().contains("superseded"));
        // The body is still there: a later reader can see what was believed.
        assert!(store.load(&entry.id).unwrap().is_some());
        assert_eq!(store.entries().unwrap().len(), 1);
    }

    #[test]
    fn retiring_an_unknown_entry_answers_none_rather_than_creating_one() {
        let scratch = Scratch::new("retire-missing");
        let store = scratch.store();
        assert!(store
            .retire("nothing-here", "superseded")
            .unwrap()
            .is_none());
    }

    #[test]
    fn an_empty_memory_contributes_nothing_to_a_prompt() {
        let scratch = Scratch::new("empty-prompt");
        let store = scratch.store();
        assert_eq!(store.index_for_prompt().unwrap(), "");
    }

    #[test]
    fn a_memory_with_entries_contributes_its_index_and_not_its_bodies() {
        let scratch = Scratch::new("prompt");
        let store = scratch.store();
        let body = "z".repeat(4_000);
        store
            .save(&MemoryEntry::new("project", "Long body", body))
            .unwrap();
        let for_prompt = store.index_for_prompt().unwrap();
        assert!(for_prompt.contains("Long body"));
        assert!(
            !for_prompt.contains("zzzz"),
            "the body must not enter the prompt"
        );
    }

    #[test]
    fn the_index_can_be_rebuilt_from_the_files() {
        // The files are the truth; the index is a projection of them.
        let scratch = Scratch::new("rebuild");
        let store = scratch.store();
        store.save(&MemoryEntry::new("task", "One", "a")).unwrap();
        store.save(&MemoryEntry::new("task", "Two", "b")).unwrap();
        fs::write(store.index_path(), INDEX_HEADER).unwrap();
        assert!(!store.index().unwrap().contains("| One |"));
        store.rebuild_index().unwrap();
        let index = store.index().unwrap();
        assert!(
            index.contains("| One |") && index.contains("| Two |"),
            "{index}"
        );
    }

    #[test]
    fn an_entry_id_is_derived_from_its_title() {
        assert_eq!(slug_id("The app layout!"), "the-app-layout");
        assert_eq!(slug_id("  spaced   out  "), "spaced-out");
        assert!(!slug_id("").is_empty());
        // Two spellings of one topic land on one file rather than two near-duplicates.
        assert_eq!(
            slug_id("Do not background commands"),
            slug_id("do NOT background commands")
        );
    }

    #[test]
    fn a_body_never_reaches_the_index_however_long_it_is() {
        let scratch = Scratch::new("body-stays-out");
        let store = scratch.store();
        let long = "t".repeat(MAX_ROW_CHARS + 100);
        store
            .save(&MemoryEntry::new("task", "Big body", long.clone()))
            .unwrap();
        let index = store.index().unwrap();
        assert!(!index.contains(&long), "the index is a pointer list");
        assert!(
            store.index_warnings().unwrap().is_empty(),
            "one short row is fine"
        );
    }

    #[test]
    fn a_title_too_long_for_a_row_is_shortened_and_reported() {
        let scratch = Scratch::new("row-cap");
        let store = scratch.store();
        let title = "w".repeat(MAX_ROW_CHARS);
        store
            .save(&MemoryEntry::new("task", title, "body"))
            .unwrap();
        let index = store.index().unwrap();
        for row in index.lines().filter(|line| line.starts_with("| ")) {
            assert!(
                row.chars().count() <= MAX_ROW_CHARS,
                "row over the cap: {row}"
            );
        }
        let warnings = store.index_warnings().unwrap();
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("shorten their title")),
            "{warnings:?}"
        );
    }

    #[test]
    fn an_index_past_its_row_limit_is_reported_rather_than_accepted() {
        let scratch = Scratch::new("index-rows");
        let store = scratch.store();
        let mut index = INDEX_HEADER.to_string();
        for number in 0..=MAX_INDEX_ROWS {
            index.push_str(&format!(
                "| entry-{number} | task | Row {number} | active | now |\n"
            ));
        }
        fs::write(store.index_path(), index).unwrap();
        let warnings = store.index_warnings().unwrap();
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("over the 100")),
            "{warnings:?}"
        );
    }

    #[test]
    fn an_entry_written_by_someone_else_is_left_alone_by_a_save_of_another() {
        let scratch = Scratch::new("foreign");
        let store = scratch.store();
        fs::write(store.dir().join("notes.md"), "# my own notes\n").unwrap();
        store.save(&MemoryEntry::new("task", "Mine", "x")).unwrap();
        let notes = fs::read_to_string(store.dir().join("notes.md")).unwrap();
        assert_eq!(notes, "# my own notes\n");
        assert_eq!(
            store.entries().unwrap().len(),
            1,
            "only the real entry is counted"
        );
    }
}
