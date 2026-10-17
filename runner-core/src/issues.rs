//! Tauri-independent local Markdown issue records and persistence.
//!
//! Frontmatter values use the JSON-compatible YAML subset emitted by the current
//! GP-Grimoire importer. Unsupported YAML is rejected instead of being silently
//! rewritten. Unknown keys and the Markdown body are retained.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::NamedTempFile;

const CURRENT_SCHEMA: u64 = 1;
const MAX_INDEX_ENTRIES: usize = 5000;
const MAX_INDEX_BYTES: u64 = 32 * 1024 * 1024;
const MAX_INDEX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_PAGE_SIZE: usize = 100;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IssueRecord {
    pub frontmatter: Map<String, Value>,
    pub body: String,
}

impl IssueRecord {
    pub fn id(&self) -> &str {
        self.frontmatter["id"].as_str().unwrap_or("")
    }
    pub fn title(&self) -> &str {
        self.frontmatter["title"].as_str().unwrap_or("")
    }
    pub fn status(&self) -> &str {
        self.frontmatter["status"].as_str().unwrap_or("")
    }
    pub fn issue_type(&self) -> &str {
        self.frontmatter["type"].as_str().unwrap_or("")
    }
    pub fn dependencies(&self) -> Vec<&str> {
        self.frontmatter
            .get("depends_on")
            .and_then(Value::as_array)
            .map(|v| v.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListedIssue {
    pub record: IssueRecord,
    pub revision: String,
    /// Missing references are reported explicitly and are never considered complete.
    pub unresolved_dependencies: Vec<String>,
    pub unresolved_parents: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IssueDetail {
    pub record: IssueRecord,
    pub revision: String,
    pub unresolved_dependencies: Vec<String>,
    pub unresolved_parents: Vec<String>,
    /// Exact UTF-8 Markdown used to compute `revision`, for immutable job snapshots.
    pub raw_markdown: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IssueSummary {
    pub id: String,
    pub title: String,
    pub status: String,
    #[serde(rename = "type")]
    pub issue_type: String,
    pub priority: Option<Value>,
    pub parent: Vec<String>,
    pub depends_on: Vec<String>,
    pub revision: String,
    pub unresolved_dependencies: Vec<String>,
    pub unresolved_parents: Vec<String>,
    /// Present only for epics. Omitted when absent to keep older clients compatible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epic_progress: Option<EpicProgress>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EpicProgress {
    pub total_children: usize,
    pub accepted_children: usize,
    pub incomplete_children: Vec<String>,
    /// Empty epics are intentionally not complete.
    pub complete: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IssuePage {
    pub items: Vec<IssueSummary>,
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EpicCompletion {
    pub complete: bool,
    pub incomplete_children: Vec<String>,
}

pub fn derive_epic_completion(epic: &IssueRecord, issues: &[IssueRecord]) -> EpicCompletion {
    let progress = derive_epic_progress(epic, issues);
    EpicCompletion {
        complete: progress.complete,
        incomplete_children: progress.incomplete_children,
    }
}

pub fn derive_epic_progress(epic: &IssueRecord, issues: &[IssueRecord]) -> EpicProgress {
    let mut progress = EpicProgress::default();
    for child in issues.iter().filter(|child| {
        child
            .frontmatter
            .get("parent")
            .and_then(Value::as_array)
            .is_some_and(|parents| parents.iter().any(|p| p.as_str() == Some(epic.id())))
    }) {
        progress.total_children += 1;
        if acceptance_checklist_complete(&child.body) {
            progress.accepted_children += 1;
        } else {
            progress.incomplete_children.push(child.id().to_owned());
        }
    }
    progress.incomplete_children.sort();
    progress.complete =
        progress.total_children > 0 && progress.accepted_children == progress.total_children;
    progress
}

fn acceptance_checklist_complete(body: &str) -> bool {
    let Some(start) = body
        .lines()
        .position(|line| line.trim().eq_ignore_ascii_case("## acceptance criteria"))
    else {
        return false;
    };
    let mut found = false;
    for line in body.lines().skip(start + 1) {
        if line.starts_with("## ") {
            break;
        }
        let item = line.trim_start();
        if item.starts_with("- [") || item.starts_with("* [") {
            found = true;
            if !(item.starts_with("- [x]")
                || item.starts_with("- [X]")
                || item.starts_with("* [x]")
                || item.starts_with("* [X]"))
            {
                return false;
            }
        }
    }
    found
}

#[derive(Debug)]
pub enum IssueError {
    Io(std::io::Error),
    Sql(rusqlite::Error),
    Invalid(String),
    UnsupportedSchema(u64),
    Conflict(String),
    Duplicate(String),
    NotFound(String),
}

impl std::fmt::Display for IssueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "issue I/O error: {e}"),
            Self::Sql(e) => write!(f, "issue journal error: {e}"),
            Self::Invalid(e) => write!(f, "invalid issue: {e}"),
            Self::UnsupportedSchema(v) => write!(
                f,
                "issue schema version {v} is newer than supported version {CURRENT_SCHEMA}"
            ),
            Self::Conflict(id) => write!(
                f,
                "issue {id} changed since it was read; reload before updating"
            ),
            Self::Duplicate(id) => write!(f, "issue ID {id} already exists"),
            Self::NotFound(id) => write!(f, "issue {id} was not found"),
        }
    }
}
impl std::error::Error for IssueError {}
impl From<std::io::Error> for IssueError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<rusqlite::Error> for IssueError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sql(e)
    }
}

pub struct IssueStore {
    root: PathBuf,
    journal: PathBuf,
}

impl IssueStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, IssueError> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        let store = Self {
            journal: root.join(".issue-ids.sqlite3"),
            root,
        };
        let conn = store.connection()?;
        conn.execute_batch("CREATE TABLE IF NOT EXISTS allocator (scope TEXT PRIMARY KEY, next_id INTEGER NOT NULL);")?;
        Ok(store)
    }

    /// Open without creating the issue folder or ID journal. Read operations can
    /// therefore inspect missing or existing backlogs without filesystem writes.
    pub fn open_read_only(root: impl Into<PathBuf>) -> Result<Self, IssueError> {
        let root = root.into();
        if !root.is_dir() {
            return Err(IssueError::NotFound(root.display().to_string()));
        }
        Ok(Self {
            journal: root.join(".issue-ids.sqlite3"),
            root,
        })
    }

    fn connection(&self) -> Result<Connection, IssueError> {
        let c = Connection::open(&self.journal)?;
        c.busy_timeout(Duration::from_secs(10))?;
        Ok(c)
    }

    fn issue_path(&self, id: &str) -> Result<PathBuf, IssueError> {
        validate_id(id)?;
        Ok(self.root.join(format!("{id}.md")))
    }

    pub fn list(&self) -> Result<Vec<ListedIssue>, IssueError> {
        self.list_inner(false)
    }

    fn list_inner(&self, bounded: bool) -> Result<Vec<ListedIssue>, IssueError> {
        let mut records = Vec::new();
        let mut ids = HashSet::new();
        let mut aggregate_bytes = 0u64;
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().and_then(|x| x.to_str()) != Some("md") {
                continue;
            }
            let Some(file_id) = path.file_stem().and_then(|x| x.to_str()) else {
                continue;
            };
            if !is_issue_id(file_id) {
                continue;
            }
            let mut file = fs::File::open(&path)?;
            let mut raw = String::new();
            if bounded {
                if records.len() >= MAX_INDEX_ENTRIES {
                    return Err(IssueError::Invalid(format!(
                        "issue index exceeds the {MAX_INDEX_ENTRIES}-file limit"
                    )));
                }
                let mut bytes = Vec::new();
                Read::by_ref(&mut file)
                    .take(MAX_INDEX_FILE_BYTES + 1)
                    .read_to_end(&mut bytes)?;
                if bytes.len() as u64 > MAX_INDEX_FILE_BYTES {
                    return Err(IssueError::Invalid(format!(
                        "issue {} exceeds the 2 MiB index-file limit",
                        file_id
                    )));
                }
                aggregate_bytes = aggregate_bytes.saturating_add(bytes.len() as u64);
                if aggregate_bytes > MAX_INDEX_BYTES {
                    return Err(IssueError::Invalid(
                        "issue index exceeds the 32 MiB aggregate read limit".into(),
                    ));
                }
                raw = String::from_utf8(bytes)
                    .map_err(|_| IssueError::Invalid(format!("issue {file_id} is not UTF-8")))?;
            } else {
                file.read_to_string(&mut raw)?;
            }
            let record = parse_markdown(&raw)?;
            validate_record(&record)?;
            if record.id() != file_id {
                return Err(IssueError::Invalid(format!(
                    "filename {file_id}.md does not match frontmatter ID {}",
                    record.id()
                )));
            }
            if !ids.insert(record.id().to_owned()) {
                return Err(IssueError::Duplicate(record.id().into()));
            }
            records.push((record, revision(&raw)));
        }
        validate_graph(
            &records
                .iter()
                .map(|(record, _)| record.clone())
                .collect::<Vec<_>>(),
        )?;
        let known: HashSet<_> = records.iter().map(|(r, _)| r.id().to_owned()).collect();
        let mut result: Vec<_> = records
            .into_iter()
            .map(|(record, file_revision)| ListedIssue {
                unresolved_dependencies: record
                    .dependencies()
                    .into_iter()
                    .filter(|id| !known.contains(*id))
                    .map(str::to_owned)
                    .collect(),
                unresolved_parents: record
                    .frontmatter
                    .get("parent")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .filter(|id| !known.contains(*id))
                    .map(str::to_owned)
                    .collect(),
                revision: file_revision,
                record,
            })
            .collect();
        result.sort_by(|a, b| a.record.id().cmp(b.record.id()));
        Ok(result)
    }

    /// Bounded, body-free page for queue views. The whole dependency index is
    /// still validated so totals and orphan metadata remain trustworthy.
    pub fn page(&self, offset: usize, limit: usize) -> Result<IssuePage, IssueError> {
        if limit == 0 || limit > MAX_PAGE_SIZE {
            return Err(IssueError::Invalid(format!(
                "page limit must be between 1 and {MAX_PAGE_SIZE}"
            )));
        }
        let all = self.list_inner(true)?;
        let total = all.len();
        // Build progress in one pass over the bounded issue index. Acceptance is
        // determined solely by the same checklist parser used by epic validation;
        // status labels such as `done` do not count as accepted evidence.
        let epic_ids: HashSet<String> = all
            .iter()
            .filter(|issue| issue.record.issue_type() == "epic")
            .map(|issue| issue.record.id().to_owned())
            .collect();
        let mut epic_progress: HashMap<String, EpicProgress> = epic_ids
            .into_iter()
            .map(|id| (id, EpicProgress::default()))
            .collect();
        for child in &all {
            let accepted = acceptance_checklist_complete(&child.record.body);
            let mut counted_parents = HashSet::new();
            for parent_id in child
                .record
                .frontmatter
                .get("parent")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                if !counted_parents.insert(parent_id) {
                    continue;
                }
                if let Some(progress) = epic_progress.get_mut(parent_id) {
                    progress.total_children += 1;
                    if accepted {
                        progress.accepted_children += 1;
                    } else {
                        progress
                            .incomplete_children
                            .push(child.record.id().to_owned());
                    }
                }
            }
        }
        for progress in epic_progress.values_mut() {
            progress.incomplete_children.sort();
            progress.complete = progress.total_children > 0
                && progress.accepted_children == progress.total_children;
        }
        let items = all
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|issue| IssueSummary {
                id: issue.record.id().to_owned(),
                title: issue.record.title().to_owned(),
                status: issue.record.status().to_owned(),
                issue_type: issue.record.issue_type().to_owned(),
                epic_progress: epic_progress.get(issue.record.id()).cloned(),
                priority: issue.record.frontmatter.get("priority").cloned(),
                parent: issue
                    .record
                    .frontmatter
                    .get("parent")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
                depends_on: issue
                    .record
                    .dependencies()
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
                revision: issue.revision,
                unresolved_dependencies: issue.unresolved_dependencies,
                unresolved_parents: issue.unresolved_parents,
            })
            .collect();
        Ok(IssuePage {
            items,
            total,
            offset,
            limit,
        })
    }

    pub fn show(&self, id: &str) -> Result<IssueDetail, IssueError> {
        let path = self.issue_path(id)?;
        let listed = self
            .list_inner(true)?
            .into_iter()
            .find(|item| item.record.id() == id)
            .ok_or_else(|| IssueError::NotFound(id.into()))?;
        let mut bytes = Vec::new();
        fs::File::open(&path)?
            .take(MAX_INDEX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_INDEX_FILE_BYTES {
            return Err(IssueError::Invalid(format!(
                "issue {id} exceeds the 2 MiB index-file limit"
            )));
        }
        let raw_markdown = String::from_utf8(bytes)
            .map_err(|_| IssueError::Invalid(format!("issue {id} is not UTF-8")))?;
        if revision(&raw_markdown) != listed.revision {
            return Err(IssueError::Conflict(id.into()));
        }
        Ok(IssueDetail {
            record: listed.record,
            revision: listed.revision,
            unresolved_dependencies: listed.unresolved_dependencies,
            unresolved_parents: listed.unresolved_parents,
            raw_markdown,
        })
    }

    /// Allocate a durable E (epic) or S (story) ID, then atomically publish it.
    /// The SQLite IMMEDIATE transaction serializes separate processes. The journal
    /// advances before file creation, so a crash can leave a gap but cannot reuse an ID.
    pub fn create(&self, record: IssueRecord) -> Result<IssueRecord, IssueError> {
        self.create_in_scope(record, "FAB")
    }

    pub fn create_in_scope(
        &self,
        mut record: IssueRecord,
        scope: &str,
    ) -> Result<IssueRecord, IssueError> {
        validate_scope(scope)?;
        let kind = if record.issue_type() == "epic" {
            'E'
        } else {
            'S'
        };
        let namespace = format!("{scope}-{kind}");
        record
            .frontmatter
            .insert("schema_version".into(), Value::from(CURRENT_SCHEMA));
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let next: Option<i64> = tx
            .query_row(
                "SELECT next_id FROM allocator WHERE scope=?1",
                [&namespace],
                |r| r.get(0),
            )
            .optional()?;
        let max_existing = self
            .list()?
            .iter()
            .filter_map(|item| scoped_number(item.record.id(), scope, kind))
            .max()
            .unwrap_or(0);
        let n = next.unwrap_or(1).max(max_existing.saturating_add(1));
        let following = n
            .checked_add(1)
            .ok_or_else(|| IssueError::Invalid("issue ID sequence exhausted".into()))?;
        let id = format!("{scope}-{kind}{n:03}");
        record
            .frontmatter
            .insert("id".into(), Value::String(id.clone()));
        validate_record(&record)?;
        let existing = self.list()?;
        let mut proposed = existing
            .iter()
            .map(|x| x.record.clone())
            .collect::<Vec<_>>();
        proposed.push(record.clone());
        validate_graph(&proposed)?;
        tx.execute("INSERT INTO allocator(scope,next_id) VALUES(?1,?2) ON CONFLICT(scope) DO UPDATE SET next_id=excluded.next_id", rusqlite::params![namespace,following])?;
        tx.commit()?;
        let path = self.issue_path(&id)?;
        atomic_create(&path, &render_markdown(&record)?)?;
        Ok(record)
    }

    /// Replace a record only if its current bytes still match the caller's revision.
    pub fn update(
        &self,
        record: IssueRecord,
        expected_revision: &str,
    ) -> Result<String, IssueError> {
        validate_record(&record)?;
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let path = self.issue_path(record.id())?;
        let current = fs::read_to_string(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                IssueError::NotFound(record.id().into())
            } else {
                IssueError::Io(e)
            }
        })?;
        if revision(&current) != expected_revision {
            return Err(IssueError::Conflict(record.id().into()));
        }
        let records = self
            .list()?
            .into_iter()
            .map(|x| x.record)
            .filter(|r| r.id() != record.id())
            .chain(std::iter::once(record.clone()))
            .collect::<Vec<_>>();
        validate_graph(&records)?;
        let previous = parse_markdown(&current)?;
        validate_transition(previous.status(), record.status())?;
        if record.issue_type() == "epic" && matches!(record.status(), "done" | "closed") {
            let completion = derive_epic_completion(&record, &records);
            if !completion.complete {
                return Err(IssueError::Invalid(format!(
                    "epic cannot be marked complete; child acceptance is incomplete: {}",
                    completion.incomplete_children.join(", ")
                )));
            }
        }
        let rendered = render_markdown(&record)?;
        let mut temp = NamedTempFile::new_in(&self.root)?;
        temp.write_all(rendered.as_bytes())?;
        temp.as_file().sync_all()?;
        temp.persist(&path).map_err(|e| IssueError::Io(e.error))?;
        tx.commit()?;
        Ok(revision(&rendered))
    }

    pub fn delete(&self, id: &str, expected_revision: &str) -> Result<(), IssueError> {
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let path = self.issue_path(id)?;
        let raw = fs::read_to_string(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                IssueError::NotFound(id.into())
            } else {
                IssueError::Io(e)
            }
        })?;
        if revision(&raw) != expected_revision {
            return Err(IssueError::Conflict(id.into()));
        }
        if let Some((scope, number)) = scoped_id_parts(id) {
            tx.execute("INSERT INTO allocator(scope,next_id) VALUES(?1,?2) ON CONFLICT(scope) DO UPDATE SET next_id=MAX(allocator.next_id,excluded.next_id)", rusqlite::params![scope, number.saturating_add(1)])?;
        }
        fs::remove_file(path)?;
        tx.commit()?;
        Ok(())
    }
}

pub fn revision(raw: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    raw.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn parse_markdown(raw: &str) -> Result<IssueRecord, IssueError> {
    let normalized = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let rest = normalized
        .strip_prefix("---\r\n")
        .or_else(|| normalized.strip_prefix("---\n"))
        .ok_or_else(|| IssueError::Invalid("missing YAML frontmatter opener".into()))?;
    let (end, close_len) = if let Some(i) = rest.find("\r\n---\r\n") {
        (i, "\r\n---\r\n".len())
    } else if let Some(i) = rest.find("\n---\n") {
        (i, "\n---\n".len())
    } else {
        return Err(IssueError::Invalid(
            "missing YAML frontmatter closer".into(),
        ));
    };
    let yaml = &rest[..end];
    let body_start = end + close_len;
    let body = rest[body_start..].to_owned();
    let mut map = Map::new();
    for (line_no, line) in yaml.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let (key, value) = line.split_once(':').ok_or_else(|| {
            IssueError::Invalid(format!(
                "frontmatter line {} is outside the JSON-compatible YAML subset",
                line_no + 1
            ))
        })?;
        let key = key.trim();
        if key.is_empty() || map.contains_key(key) {
            return Err(IssueError::Invalid(format!(
                "empty or duplicate frontmatter key {key:?}"
            )));
        }
        let value = value.trim();
        let parsed = serde_json::from_str(value).map_err(|_| {
            IssueError::Invalid(format!(
                "frontmatter key {key:?} must use a JSON-compatible YAML value"
            ))
        })?;
        map.insert(key.to_owned(), parsed);
    }
    Ok(IssueRecord {
        frontmatter: map,
        body,
    })
}

/// Parse authoritative issue Markdown for downstream verification identity checks.
pub fn parse_issue_markdown(raw: &str) -> Result<IssueRecord, IssueError> {
    parse_markdown(raw)
}

fn render_markdown(record: &IssueRecord) -> Result<String, IssueError> {
    let mut out = String::from("\u{feff}---\n");
    let ordered: BTreeMap<_, _> = record.frontmatter.iter().collect();
    for (key, value) in ordered {
        out.push_str(key);
        out.push_str(": ");
        out.push_str(
            &serde_json::to_string(value).map_err(|e| IssueError::Invalid(e.to_string()))?,
        );
        out.push('\n');
    }
    out.push_str("---\n");
    out.push_str(&record.body);
    Ok(out)
}

fn validate_id(id: &str) -> Result<(), IssueError> {
    if !is_issue_id(id) {
        return Err(IssueError::Invalid(format!("unsafe issue ID {id:?}")));
    }
    Ok(())
}

fn is_issue_id(id: &str) -> bool {
    let parts: Vec<_> = id.split('-').collect();
    if parts.len() < 2 {
        return false;
    }
    let prefix_valid = |items: &[&str]| {
        items.iter().all(|p| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        })
    };
    let numeric_tail = parts
        .last()
        .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
    let scoped_tail = parts
        .last()
        .and_then(|n| n.strip_prefix(['L', 'E', 'S']))
        .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
    (numeric_tail || scoped_tail) && prefix_valid(&parts[..parts.len() - 1])
}
fn validate_scope(scope: &str) -> Result<(), IssueError> {
    if scope.is_empty()
        || !scope
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    {
        Err(IssueError::Invalid(format!(
            "invalid issue ID scope {scope:?}"
        )))
    } else {
        Ok(())
    }
}
pub fn validate_scope_name(scope: &str) -> Result<(), String> {
    validate_scope(scope).map_err(|e| e.to_string())
}
fn scoped_number(id: &str, scope: &str, kind: char) -> Option<i64> {
    let suffix = id.strip_prefix(&format!("{scope}-{kind}"))?;
    if suffix.is_empty() || !suffix.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    suffix.parse().ok()
}

fn scoped_id_parts(id: &str) -> Option<(String, i64)> {
    let (scope, suffix) = id.rsplit_once('-')?;
    validate_scope(scope).ok()?;
    let kind = suffix.chars().next()?;
    if !matches!(kind, 'L' | 'E' | 'S') {
        return None;
    }
    let number = scoped_number(id, scope, kind)?;
    // Keep legacy allocator rows intact; new kinds have independent keys.
    let namespace = if kind == 'L' {
        scope.to_string()
    } else {
        format!("{scope}-{kind}")
    };
    Some((namespace, number))
}

fn validate_record(r: &IssueRecord) -> Result<(), IssueError> {
    let get = |key: &str| {
        r.frontmatter
            .get(key)
            .ok_or_else(|| IssueError::Invalid(format!("missing {key}")))
    };
    let version = get("schema_version")?
        .as_u64()
        .ok_or_else(|| IssueError::Invalid("schema_version must be an integer".into()))?;
    if version > CURRENT_SCHEMA {
        return Err(IssueError::UnsupportedSchema(version));
    }
    if version != CURRENT_SCHEMA {
        return Err(IssueError::Invalid(format!(
            "schema_version {version} requires migration"
        )));
    }
    let issue_id = get("id")?
        .as_str()
        .ok_or_else(|| IssueError::Invalid("id must be a string".into()))?;
    validate_id(issue_id)?;
    if get("title")?.as_str().is_none_or(str::is_empty) {
        return Err(IssueError::Invalid(
            "title must be a non-empty string".into(),
        ));
    }
    if get("type")?.as_str().is_none_or(str::is_empty) {
        return Err(IssueError::Invalid(
            "type must be a non-empty string".into(),
        ));
    }
    let status = get("status")?
        .as_str()
        .ok_or_else(|| IssueError::Invalid("status must be a string".into()))?;
    if ![
        "open",
        "ready",
        "in_progress",
        "in_review",
        "blocked",
        "done",
        "closed",
    ]
    .contains(&status)
    {
        return Err(IssueError::Invalid(format!(
            "unsupported issue status {status:?}"
        )));
    }
    if let Some(p) = r.frontmatter.get("priority") {
        if !p.is_null() && p.as_str().is_none() {
            return Err(IssueError::Invalid(
                "priority must be a string or explicit null".into(),
            ));
        }
    }
    for key in ["parent", "depends_on"] {
        if let Some(v) = r.frontmatter.get(key) {
            if !v.is_array() || v.as_array().unwrap().iter().any(|x| x.as_str().is_none()) {
                return Err(IssueError::Invalid(format!("{key} must be a list of IDs")));
            }
            for related in v.as_array().unwrap().iter().filter_map(Value::as_str) {
                validate_id(related)?;
                if related == issue_id {
                    return Err(IssueError::Invalid(format!(
                        "{key} cannot reference the issue itself"
                    )));
                }
            }
        }
    }
    if !r.frontmatter.contains_key("parent") {
        return Err(IssueError::Invalid("missing parent".into()));
    }
    if !r.frontmatter.contains_key("depends_on") {
        return Err(IssueError::Invalid("missing depends_on".into()));
    }
    Ok(())
}

fn validate_transition(old: &str, new: &str) -> Result<(), IssueError> {
    if old == new {
        return Ok(());
    }
    let allowed = match old {
        "open" => ["ready", "blocked", "in_progress"].contains(&new),
        "ready" => ["open", "blocked", "in_progress"].contains(&new),
        "in_progress" => ["ready", "blocked", "in_review"].contains(&new),
        "in_review" => ["in_progress", "ready", "blocked", "done"].contains(&new),
        "blocked" => ["ready", "open", "in_progress"].contains(&new),
        "done" => new == "closed",
        "closed" => new == "open",
        _ => false,
    };
    if allowed {
        Ok(())
    } else {
        Err(IssueError::Invalid(format!(
            "invalid status transition {old:?} -> {new:?}"
        )))
    }
}

fn validate_graph(records: &[IssueRecord]) -> Result<(), IssueError> {
    let by_id: HashMap<String, &IssueRecord> =
        records.iter().map(|r| (r.id().to_owned(), r)).collect();
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    fn visit(
        id: &str,
        by: &HashMap<String, &IssueRecord>,
        edge: &str,
        visiting: &mut HashSet<String>,
        visited: &mut HashSet<String>,
    ) -> Result<(), IssueError> {
        if visited.contains(id) {
            return Ok(());
        }
        if !visiting.insert(id.to_owned()) {
            return Err(IssueError::Invalid(format!("{edge} cycle includes {id}")));
        }
        if let Some(r) = by.get(id) {
            let related = r
                .frontmatter
                .get(edge)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str);
            for other in related {
                if by.contains_key(other) {
                    visit(other, by, edge, visiting, visited)?;
                }
            }
        }
        visiting.remove(id);
        visited.insert(id.to_owned());
        Ok(())
    }
    for edge in ["depends_on", "parent"] {
        visiting.clear();
        visited.clear();
        for id in by_id.keys() {
            visit(id, &by_id, edge, &mut visiting, &mut visited)?;
        }
    }
    Ok(())
}

fn atomic_create(path: &Path, contents: &str) -> Result<(), IssueError> {
    let mut tmp = NamedTempFile::new_in(
        path.parent()
            .ok_or_else(|| IssueError::Invalid("issue path has no parent".into()))?,
    )?;
    tmp.write_all(contents.as_bytes())?;
    tmp.as_file().sync_all()?;
    tmp.persist_noclobber(path).map_err(|e| {
        if e.error.kind() == std::io::ErrorKind::AlreadyExists {
            IssueError::Duplicate(
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown")
                    .into(),
            )
        } else {
            IssueError::Io(e.error)
        }
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record(id: &str, deps: &[&str]) -> IssueRecord {
        let mut frontmatter = Map::new();
        for (k, v) in [
            ("schema_version", Value::from(1)),
            ("id", Value::String(id.into())),
            ("title", Value::String(format!("Issue {id}"))),
            ("type", Value::String("enhancement".into())),
            ("status", Value::String("open".into())),
            ("priority", Value::Null),
            ("parent", Value::Array(vec![])),
            (
                "depends_on",
                Value::Array(deps.iter().map(|s| Value::String((*s).into())).collect()),
            ),
            ("github_number", Value::from(105)),
        ] {
            frontmatter.insert(k.into(), v);
        }
        IssueRecord {
            frontmatter,
            body: format!("# {id}\n\n<!-- imported body markers -->\nbody\n"),
        }
    }
    #[test]
    fn round_trip_keeps_null_unknown_fields_and_body() {
        let r = record("GP-105", &["GP-104"]);
        let raw = render_markdown(&r).unwrap();
        let parsed = parse_markdown(&raw).unwrap();
        assert_eq!(parsed, r);
        assert!(parsed.frontmatter["priority"].is_null());
        assert_eq!(parsed.frontmatter["github_number"], 105);
        assert!(parsed.body.contains("imported body markers"));
    }
    #[test]
    fn round_trip_preserves_blank_body_prefix_for_lf_and_crlf() {
        for delimiter in ["\n", "\r\n"] {
            let raw = format!("---{0}schema_version: 1{0}id: \"GP-105\"{0}title: \"Title\"{0}type: \"task\"{0}status: \"open\"{0}parent: []{0}depends_on: []{0}---{0}{0}# Body starts after a blank line{0}", delimiter);
            let parsed = parse_markdown(&raw).unwrap();
            assert_eq!(
                parsed.body,
                format!("{0}# Body starts after a blank line{0}", delimiter)
            );
            assert_eq!(
                parse_markdown(&render_markdown(&parsed).unwrap())
                    .unwrap()
                    .body,
                parsed.body
            );
        }
    }
    #[test]
    fn unresolved_dependencies_are_reported() {
        let d = tempfile::tempdir().unwrap();
        let s = IssueStore::open(d.path()).unwrap();
        let r = record("GP-105", &["GP-104"]);
        atomic_create(
            &s.issue_path("GP-105").unwrap(),
            &render_markdown(&r).unwrap(),
        )
        .unwrap();
        assert_eq!(
            s.show("GP-105").unwrap().unresolved_dependencies,
            vec!["GP-104"]
        );
    }
    #[test]
    fn page_returns_only_bounded_summaries_with_total_and_dependency_metadata() {
        let d = tempfile::tempdir().unwrap();
        let s = IssueStore::open(d.path()).unwrap();
        let mut first = record("FAB-L001", &["FAB-L999"]);
        first
            .frontmatter
            .insert("priority".into(), Value::String("P1".into()));
        first.frontmatter.insert(
            "parent".into(),
            Value::Array(vec![Value::String("FAB-L000".into())]),
        );
        atomic_create(
            &s.issue_path("FAB-L001").unwrap(),
            &render_markdown(&first).unwrap(),
        )
        .unwrap();
        atomic_create(
            &s.issue_path("FAB-L002").unwrap(),
            &render_markdown(&record("FAB-L002", &[])).unwrap(),
        )
        .unwrap();
        let page = s.page(1, 1).unwrap();
        assert_eq!((page.total, page.offset, page.limit), (2, 1, 1));
        assert_eq!(page.items[0].id, "FAB-L002");
        assert!(serde_json::to_value(&page).unwrap()["items"][0]
            .get("body")
            .is_none());
        let first = s.page(0, 1).unwrap().items.remove(0);
        assert_eq!(first.priority, Some(Value::String("P1".into())));
        assert_eq!(first.parent, vec!["FAB-L000"]);
        assert_eq!(first.depends_on, vec!["FAB-L999"]);
        assert_eq!(first.unresolved_dependencies, vec!["FAB-L999"]);
        assert_eq!(first.unresolved_parents, vec!["FAB-L000"]);
        assert!(s.page(0, 101).is_err());
    }
    #[test]
    fn epic_page_progress_counts_checklist_acceptance_and_keeps_empty_epics_incomplete() {
        let d = tempfile::tempdir().unwrap();
        let s = IssueStore::open(d.path()).unwrap();
        let mut epic = record("FAB-L010", &[]);
        epic.frontmatter
            .insert("type".into(), Value::String("epic".into()));
        let mut empty_epic = record("FAB-L011", &[]);
        empty_epic
            .frontmatter
            .insert("type".into(), Value::String("epic".into()));

        let mut accepted = record("FAB-L012", &[]);
        accepted.frontmatter.insert(
            "parent".into(),
            Value::Array(vec![Value::String(epic.id().into())]),
        );
        accepted
            .frontmatter
            .insert("status".into(), Value::String("closed".into()));
        accepted.body = "## Acceptance criteria\n- [x] Verified behavior\n".into();

        let mut status_only = record("FAB-L013", &[]);
        status_only.frontmatter.insert(
            "parent".into(),
            Value::Array(vec![Value::String(epic.id().into())]),
        );
        status_only
            .frontmatter
            .insert("status".into(), Value::String("done".into()));
        status_only.body = "## Acceptance criteria\n- [ ] Still pending\n".into();

        for issue in [&epic, &empty_epic, &accepted, &status_only] {
            atomic_create(
                &s.issue_path(issue.id()).unwrap(),
                &render_markdown(issue).unwrap(),
            )
            .unwrap();
        }

        let page = s.page(0, 100).unwrap();
        let progress = page
            .items
            .iter()
            .find(|item| item.id == epic.id())
            .unwrap()
            .epic_progress
            .as_ref()
            .unwrap();
        assert_eq!(progress.total_children, 2);
        assert_eq!(progress.accepted_children, 1);
        assert_eq!(progress.incomplete_children, vec!["FAB-L013"]);
        assert!(
            !progress.complete,
            "done status alone must not count as acceptance"
        );
        let epic_summary = page.items.iter().find(|item| item.id == epic.id()).unwrap();
        let epic_json = serde_json::to_value(epic_summary).unwrap();
        assert_eq!(epic_json["epicProgress"]["totalChildren"], 2);
        assert_eq!(epic_json["epicProgress"]["acceptedChildren"], 1);
        assert_eq!(
            epic_json["epicProgress"]["incompleteChildren"][0],
            "FAB-L013"
        );

        let empty = page
            .items
            .iter()
            .find(|item| item.id == empty_epic.id())
            .unwrap()
            .epic_progress
            .as_ref()
            .unwrap();
        assert_eq!(empty.total_children, 0);
        assert_eq!(empty.accepted_children, 0);
        assert!(empty.incomplete_children.is_empty());
        assert!(
            !empty.complete,
            "an epic with no children cannot be complete"
        );
        let ordinary = page
            .items
            .iter()
            .find(|item| item.id == accepted.id())
            .unwrap();
        assert!(ordinary.epic_progress.is_none());

        let json = serde_json::to_value(&ordinary).unwrap();
        assert!(
            json.get("epicProgress").is_none(),
            "absent progress is omitted for compatibility"
        );
        let old_client_shape: IssueSummary = serde_json::from_value(json).unwrap();
        assert!(old_client_shape.epic_progress.is_none());
    }

    #[test]
    fn page_and_show_enforce_actual_read_caps() {
        let d = tempfile::tempdir().unwrap();
        let s = IssueStore::open(d.path()).unwrap();
        let oversized = format!("---\nschema_version: 1\nid: \"FAB-L001\"\ntitle: \"Large\"\ntype: \"task\"\nstatus: \"open\"\nparent: []\ndepends_on: []\n---\n{}", "x".repeat(MAX_INDEX_FILE_BYTES as usize + 1));
        fs::write(d.path().join("FAB-L001.md"), oversized).unwrap();
        assert!(s.page(0, 10).unwrap_err().to_string().contains("2 MiB"));
        assert!(s
            .show("FAB-L001")
            .unwrap_err()
            .to_string()
            .contains("2 MiB"));
    }

    #[test]
    fn show_returns_exact_markdown_matching_revision() {
        let d = tempfile::tempdir().unwrap();
        let s = IssueStore::open(d.path()).unwrap();
        let raw = render_markdown(&record("FAB-L001", &[])).unwrap();
        fs::write(d.path().join("FAB-L001.md"), &raw).unwrap();
        let detail = s.show("FAB-L001").unwrap();
        assert_eq!(detail.raw_markdown, raw);
        assert_eq!(detail.revision, revision(&detail.raw_markdown));
    }

    #[test]
    fn page_enforces_aggregate_bytes_during_reads() {
        let d = tempfile::tempdir().unwrap();
        let s = IssueStore::open(d.path()).unwrap();
        let body = "x".repeat((MAX_INDEX_BYTES / 17) as usize);
        for number in 1..=17 {
            let id = format!("FAB-L{number:03}");
            let raw = format!("---\nschema_version: 1\nid: \"{id}\"\ntitle: \"Large\"\ntype: \"task\"\nstatus: \"open\"\nparent: []\ndepends_on: []\n---\n{body}");
            fs::write(d.path().join(format!("{id}.md")), raw).unwrap();
        }
        assert!(s.page(0, 1).unwrap_err().to_string().contains("32 MiB"));
    }
    #[test]
    fn create_ids_are_durable_and_never_reused_after_delete() {
        let d = tempfile::tempdir().unwrap();
        let s = IssueStore::open(d.path()).unwrap();
        let a = s.create(record("placeholder", &[])).unwrap();
        let path = s.issue_path(a.id()).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        s.delete(a.id(), &revision(&raw)).unwrap();
        let b = s.create(record("placeholder", &[])).unwrap();
        assert_eq!(a.id(), "FAB-S001");
        assert_eq!(b.id(), "FAB-S002");
    }
    #[test]
    fn deleting_imported_highest_scoped_id_advances_first_journal_allocation() {
        let d = tempfile::tempdir().unwrap();
        let s = IssueStore::open(d.path()).unwrap();
        let r = record("GP-S011", &[]);
        let path = s.issue_path(r.id()).unwrap();
        atomic_create(&path, &render_markdown(&r).unwrap()).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        s.delete(r.id(), &revision(&raw)).unwrap();
        assert_eq!(
            s.create_in_scope(record("placeholder", &[]), "GP")
                .unwrap()
                .id(),
            "GP-S012"
        );
    }
    #[test]
    fn update_reports_concurrent_edit_and_rejects_invalid_transition() {
        let d = tempfile::tempdir().unwrap();
        let s = IssueStore::open(d.path()).unwrap();
        let r = record("FAB-L001", &[]);
        atomic_create(
            &s.issue_path(r.id()).unwrap(),
            &render_markdown(&r).unwrap(),
        )
        .unwrap();
        let mut changed = r.clone();
        changed.body.push_str("new");
        let raw = fs::read_to_string(s.issue_path(r.id()).unwrap()).unwrap();
        s.update(changed.clone(), &revision(&raw)).unwrap();
        assert!(matches!(
            s.update(r.clone(), &revision(&raw)),
            Err(IssueError::Conflict(_))
        ));
        changed
            .frontmatter
            .insert("status".into(), Value::String("done".into()));
        let raw = fs::read_to_string(s.issue_path(r.id()).unwrap()).unwrap();
        assert!(s.update(changed, &revision(&raw)).is_err());
    }
    #[test]
    fn rejects_dependency_cycles() {
        let a = record("A", &["B"]);
        let b = record("B", &["A"]);
        assert!(validate_graph(&[a, b]).is_err());
    }
    #[test]
    fn rejects_parent_cycles_self_references_and_reports_missing_parents() {
        let mut a = record("A-1", &[]);
        let mut b = record("B-1", &[]);
        a.frontmatter.insert(
            "parent".into(),
            Value::Array(vec![Value::String("B-1".into())]),
        );
        b.frontmatter.insert(
            "parent".into(),
            Value::Array(vec![Value::String("A-1".into())]),
        );
        assert!(validate_graph(&[a.clone(), b]).is_err());
        a.frontmatter.insert(
            "parent".into(),
            Value::Array(vec![Value::String("A-1".into())]),
        );
        assert!(validate_record(&a).is_err());
        let d = tempfile::tempdir().unwrap();
        let s = IssueStore::open(d.path()).unwrap();
        let mut orphan = record("GP-105", &[]);
        orphan.frontmatter.insert(
            "parent".into(),
            Value::Array(vec![Value::String("GP-999".into())]),
        );
        atomic_create(
            &s.issue_path(orphan.id()).unwrap(),
            &render_markdown(&orphan).unwrap(),
        )
        .unwrap();
        assert_eq!(s.show("GP-105").unwrap().unresolved_parents, vec!["GP-999"]);
    }
    #[test]
    fn json_compatible_yaml_subset_parses_actual_import_shape() {
        let raw="---\nschema_version: 1\nid: \"GP-109\"\ntitle: \"Imported title\"\nstatus: \"open\"\npriority: null\ntype: \"enhancement\"\nparent: [\"GP-107\"]\ndepends_on: [\"GP-108\"]\ngithub_url: \"https://example.test/issues/109\"\n---\n# GP-109: Imported title\n<!-- github-body:start -->\ntext\n<!-- github-body:end -->\n";
        let parsed = parse_markdown(raw).unwrap();
        validate_record(&parsed).unwrap();
        assert_eq!(parsed.dependencies(), vec!["GP-108"]);
        assert!(parsed.body.contains("github-body:end"));
    }
    #[test]
    fn allocator_uses_scope_high_watermark_not_first_gap() {
        let d = tempfile::tempdir().unwrap();
        let s = IssueStore::open(d.path()).unwrap();
        for n in [1, 3, 11] {
            let r = record(&format!("FAB-S{n:03}"), &[]);
            atomic_create(
                &s.issue_path(r.id()).unwrap(),
                &render_markdown(&r).unwrap(),
            )
            .unwrap();
        }
        let next = s.create(record("placeholder", &[])).unwrap();
        assert_eq!(next.id(), "FAB-S012");
        let gp = s.create_in_scope(record("placeholder", &[]), "GP").unwrap();
        assert_eq!(gp.id(), "GP-S001");
    }

    #[test]
    fn epic_story_sequences_are_independent_and_legacy_ids_survive() {
        let d = tempfile::tempdir().unwrap();
        let s = IssueStore::open(d.path()).unwrap();
        for id in ["GP-105", "GP-L099"] {
            atomic_create(
                &s.issue_path(id).unwrap(),
                &render_markdown(&record(id, &[])).unwrap(),
            )
            .unwrap();
        }
        // An older binary's journal must not advance either new sequence.
        s.connection()
            .unwrap()
            .execute("INSERT INTO allocator VALUES ('GP',100)", [])
            .unwrap();
        let mut epic = record("placeholder", &[]);
        epic.frontmatter
            .insert("type".into(), Value::String("epic".into()));
        let first = s.create_in_scope(epic.clone(), "GP").unwrap();
        assert_eq!(first.id(), "GP-E001");
        assert_eq!(
            s.create_in_scope(record("placeholder", &[]), "GP")
                .unwrap()
                .id(),
            "GP-S001"
        );
        let raw = s.show(first.id()).unwrap();
        s.delete(first.id(), &raw.revision).unwrap();
        drop(s);
        let s = IssueStore::open(d.path()).unwrap();
        assert_eq!(s.create_in_scope(epic, "GP").unwrap().id(), "GP-E002");
        assert_eq!(
            s.create_in_scope(record("placeholder", &[]), "GP")
                .unwrap()
                .id(),
            "GP-S002"
        );
        assert_eq!(s.show("GP-105").unwrap().record.id(), "GP-105");
        assert_eq!(s.show("GP-L099").unwrap().record.id(), "GP-L099");
    }

    #[test]
    fn imported_epic_high_watermark_is_not_reused_after_deletion() {
        let d = tempfile::tempdir().unwrap();
        let s = IssueStore::open(d.path()).unwrap();
        let mut epic = record("FAB-E011", &[]);
        epic.frontmatter
            .insert("type".into(), Value::String("epic".into()));
        atomic_create(
            &s.issue_path(epic.id()).unwrap(),
            &render_markdown(&epic).unwrap(),
        )
        .unwrap();
        let raw = s.show(epic.id()).unwrap();
        s.delete(epic.id(), &raw.revision).unwrap();
        assert_eq!(s.create(epic).unwrap().id(), "FAB-E012");
        assert_eq!(
            s.create(record("placeholder", &[])).unwrap().id(),
            "FAB-S001"
        );
    }
    #[test]
    fn list_skips_unrelated_markdown() {
        let d = tempfile::tempdir().unwrap();
        let s = IssueStore::open(d.path()).unwrap();
        fs::write(d.path().join("README.md"), "# notes").unwrap();
        assert!(s.list().unwrap().is_empty());
    }
    #[test]
    fn list_rejects_mismatched_and_duplicate_ids() {
        let d = tempfile::tempdir().unwrap();
        let s = IssueStore::open(d.path()).unwrap();
        let r = record("GP-105", &[]);
        atomic_create(
            &s.issue_path("GP-105").unwrap(),
            &render_markdown(&r).unwrap(),
        )
        .unwrap();
        fs::write(d.path().join("GP-106.md"), render_markdown(&r).unwrap()).unwrap();
        assert!(s.list().is_err());
    }
    #[test]
    fn rejects_newer_schema_and_non_json_yaml() {
        let mut r = record("GP-105", &[]);
        r.frontmatter
            .insert("schema_version".into(), Value::from(2));
        assert!(matches!(
            validate_record(&r),
            Err(IssueError::UnsupportedSchema(2))
        ));
        assert!(parse_markdown("---\nschema_version: one\n---\nbody").is_err());
    }
    #[test]
    fn concurrent_allocators_get_distinct_durable_ids() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().to_path_buf();
        let threads: Vec<_> = (0..6)
            .map(|_| {
                let root = root.clone();
                std::thread::spawn(move || {
                    IssueStore::open(root)
                        .unwrap()
                        .create(record("placeholder", &[]))
                        .unwrap()
                        .id()
                        .to_owned()
                })
            })
            .collect();
        let mut ids: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 6);
        assert_eq!(ids.first().unwrap(), "FAB-S001");
        assert_eq!(ids.last().unwrap(), "FAB-S006");
    }
    #[test]
    fn epic_completion_comes_from_child_acceptance_checklists() {
        let epic = record("FAB-L001", &[]);
        let mut child = record("FAB-L002", &[]);
        child.frontmatter.insert(
            "parent".into(),
            Value::Array(vec![Value::String(epic.id().into())]),
        );
        child.body = "## Acceptance criteria\n- [x] One\n- [X] Two\n".into();
        assert!(derive_epic_completion(&epic, &[epic.clone(), child.clone()]).complete);
        child.body = child.body.replace("[X]", "[ ]");
        let result = derive_epic_completion(&epic, &[epic.clone(), child]);
        assert!(!result.complete);
        assert_eq!(result.incomplete_children, vec!["FAB-L002"]);
    }
    #[test]
    fn epic_cannot_be_marked_done_before_child_acceptance_is_complete() {
        let d = tempfile::tempdir().unwrap();
        let s = IssueStore::open(d.path()).unwrap();
        let mut epic = record("FAB-L001", &[]);
        epic.frontmatter
            .insert("type".into(), Value::String("epic".into()));
        epic.frontmatter
            .insert("status".into(), Value::String("in_review".into()));
        let mut child = record("FAB-L002", &[]);
        child.frontmatter.insert(
            "parent".into(),
            Value::Array(vec![Value::String(epic.id().into())]),
        );
        child.body = "## Acceptance criteria\n- [ ] Pending\n".into();
        for r in [&epic, &child] {
            atomic_create(&s.issue_path(r.id()).unwrap(), &render_markdown(r).unwrap()).unwrap();
        }
        let raw = fs::read_to_string(s.issue_path(epic.id()).unwrap()).unwrap();
        let mut done = epic.clone();
        done.frontmatter
            .insert("status".into(), Value::String("done".into()));
        assert!(s.update(done.clone(), &revision(&raw)).is_err());
        let child_raw = fs::read_to_string(s.issue_path(child.id()).unwrap()).unwrap();
        child.body = child.body.replace("[ ]", "[x]");
        s.update(child.clone(), &revision(&child_raw)).unwrap();
        let epic_raw = fs::read_to_string(s.issue_path(epic.id()).unwrap()).unwrap();
        assert!(s.update(done, &revision(&epic_raw)).is_ok());
    }
    #[test]
    fn closed_dependency_is_resolved_but_missing_dependency_is_not() {
        let mut dep = record("GP-105", &[]);
        dep.frontmatter
            .insert("status".into(), Value::String("closed".into()));
        let issue = record("GP-106", &["GP-105", "GP-104"]);
        let d = tempfile::tempdir().unwrap();
        let s = IssueStore::open(d.path()).unwrap();
        for r in [&dep, &issue] {
            atomic_create(&s.issue_path(r.id()).unwrap(), &render_markdown(r).unwrap()).unwrap();
        }
        assert_eq!(
            s.show("GP-106").unwrap().unresolved_dependencies,
            vec!["GP-104"]
        );
    }
    #[test]
    fn reads_current_gp_snapshot_when_fixture_path_is_set() {
        if let Ok(dir) = std::env::var("FABRICA_GP_ISSUE_FIXTURES") {
            let source = Path::new(&dir);
            let expected = [105, 106, 107, 109, 110, 111, 112];
            let names: Vec<_> = fs::read_dir(source)
                .unwrap()
                .filter_map(Result::ok)
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.starts_with("GP-") && n.ends_with(".md"))
                .collect();
            assert_eq!(
                names
                    .iter()
                    .filter(|n| expected.iter().any(|v| n == &&format!("GP-{v}.md")))
                    .count(),
                expected.len()
            );
            let temp = tempfile::tempdir().unwrap();
            for n in expected {
                let name = format!("GP-{n}.md");
                let raw = fs::read_to_string(source.join(&name)).unwrap();
                let parsed = parse_markdown(&raw).unwrap();
                validate_record(&parsed).unwrap();
                assert_eq!(parsed.id(), format!("GP-{n}"));
                fs::write(temp.path().join(name), raw).unwrap();
            }
            let store = IssueStore::open(temp.path()).unwrap();
            let listed = store.list().unwrap();
            assert_eq!(listed.len(), 7);
            assert!(store.show("GP-105").is_ok());
            let gp109 = store.show("GP-109").unwrap();
            assert!(gp109.unresolved_dependencies.contains(&"GP-108".into()));
            let created = store
                .create_in_scope(record("placeholder", &[]), "GP")
                .unwrap();
            assert_eq!(created.id(), "GP-S001");
        }
    }
}
