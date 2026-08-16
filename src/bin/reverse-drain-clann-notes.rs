//! Reverse-drain: tack-server -> clann-server's own SurrealDB. The rollback
//! half of `backfill-clann-notes.rs` -- replays whatever happened in
//! tack-server after a Clann frontend cutover back into `research_note`/
//! `research_folder`, so the old SurrealDB-backed UI can resume from an
//! accurate state if the migration needs to be rolled back. Lives here for
//! the same reason `backfill-clann-notes.rs` does (see its own doc comment
//! and `ullav-helm`'s `docs/notes-backfill-runbook.md`) -- clann-server's
//! own migration plan is the first case in this org needing a reverse-
//! drain at all (awe-server's cutover didn't), so there's no prior
//! location precedent for *this* specific tool the way there is for the
//! forward direction; co-locating it with `backfill-clann-notes.rs` keeps
//! the whole migration pipeline in one place rather than splitting related
//! logic across two repos.
//!
//! Scope: `TEAM_IDS` (required, comma-separated) -- the operator's own
//! record of which teams were actually cut over. This tool has no way to
//! discover that on its own.
//!
//! Reconciliation, per note:
//! - Already in the forward backfill's own `--state-file` (source id ->
//!   tack id) -- originally migrated forward. Its `research_note` row is
//!   UPDATEd (title, body, is_shared, trees) to reflect any post-cutover
//!   edit. `created_by`/`created_at` are never touched -- tack has no way
//!   to edit a note's author after creation (`UpdateNoteRequest` has no
//!   such field), so the original migrated authorship stays correct.
//! - Not in the forward state file -- created fresh in tack after cutover.
//!   A new `research_note` row is CREATEd, tracked in this tool's own
//!   `--state-file` (tack id -> new surreal id) for idempotency on re-run.
//! - In the forward state file but no longer resolves in tack (deleted
//!   post-cutover) -- flagged in the log as needing manual review, never
//!   auto-deleted. Silently deleting a Clann user's historical data on a
//!   guessed intent is a worse failure mode than a stale row a human has
//!   to clear.
//!
//! `description` is recovered from `tack_note_meta` when a sidecar row
//! exists for that tack note id, dropped (logged) only when it doesn't --
//! true for every note originally migrated forward, and for any note
//! created post-cutover through a Phase 3-repointed handler that writes
//! the sidecar symmetrically to `backfill-clann-notes.rs`'s own write.
//!
//! Replies are position-matched, not content-matched: for a given note,
//! the first N tack replies (N = however many `research_note` reply rows
//! already exist for that parent) are assumed to be the original ones;
//! anything past that is new and gets created. A known, documented
//! heuristic -- robust to new replies appended at the end, not robust to
//! an existing early reply being edited in tack.
//!
//! `--dry-run`: full reconciliation and reporting, zero writes.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use surrealdb::{engine::any, opt::auth::Root};
use uuid::Uuid;

const UNKNOWN_AUTHOR_PLACEHOLDER: &str = "clann-migration-rollback";

#[derive(Default, Serialize, Deserialize)]
struct DrainState {
    /// tack id -> newly-created surreal id, for notes/folders created fresh
    /// in tack after cutover (not present in the forward backfill's own
    /// state file).
    drained: HashMap<String, String>,
}

impl DrainState {
    fn load(path: &PathBuf) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let data = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&data)?)
    }
    fn record(&mut self, path: &PathBuf, tack_id: &str, surreal_id: &str) -> Result<()> {
        self.drained.insert(tack_id.to_string(), surreal_id.to_string());
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}

/// The forward backfill's own `--state-file` shape (`backfill-clann-notes.
/// rs`'s `State`) -- read-only here, never written.
#[derive(Deserialize)]
struct ForwardState {
    migrated: HashMap<String, Uuid>,
}

fn str_field<'a>(row: &'a Value, field: &str) -> Option<&'a str> {
    row.get(field)?.as_str()
}

#[derive(Deserialize)]
struct TackFolder {
    id: Uuid,
    name: String,
}
#[derive(Deserialize)]
struct TackFoldersPage {
    folders: Vec<TackFolder>,
}
#[derive(Deserialize)]
struct TackNote {
    id: Uuid,
    title: String,
    body_markdown: String,
    visibility: String,
    folder_id: Option<Uuid>,
    created_by: Uuid,
}
#[derive(Deserialize)]
struct TackNotesPage {
    notes: Vec<TackNote>,
}
#[derive(Deserialize)]
struct TackAttachment {
    owning_service: String,
    entity_type: String,
    entity_id: String,
}
#[derive(Deserialize)]
struct AdminUserById {
    username: String,
}

async fn resolve_username(http: &reqwest::Client, uum_base: &str, uum_token: &str, user_id: Uuid) -> Option<String> {
    let resp = http.get(format!("{uum_base}/admin/users/{user_id}")).bearer_auth(uum_token).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<AdminUserById>().await.ok().map(|u| u.username)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let _ = dotenvy::dotenv();

    let args: Vec<String> = std::env::args().collect();
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let state_path: PathBuf = args
        .iter()
        .position(|a| a == "--state-file")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("reverse-drain-clann-notes-state.json"));
    let forward_state_path: PathBuf = args
        .iter()
        .position(|a| a == "--forward-state-file")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("backfill-clann-notes-state.json"));

    let clann_db_url = std::env::var("CLANN_DB_URL").context("CLANN_DB_URL must be set")?;
    let clann_db_ns = std::env::var("CLANN_DB_NAMESPACE").unwrap_or_else(|_| "clann".to_string());
    let clann_db_db = std::env::var("CLANN_DB_DATABASE").unwrap_or_else(|_| "ancestry".to_string());
    let clann_db_user = std::env::var("CLANN_DB_USERNAME").context("CLANN_DB_USERNAME must be set")?;
    let clann_db_pass = std::env::var("CLANN_DB_PASSWORD").context("CLANN_DB_PASSWORD must be set")?;
    let uum_url = std::env::var("UUM_URL").unwrap_or_else(|_| "http://localhost:8081".into());
    let uum_admin_token = std::env::var("UUM_ADMIN_TOKEN").context("UUM_ADMIN_TOKEN must be set")?;
    let tack_api_url = std::env::var("TACK_API_URL").unwrap_or_else(|_| "http://localhost:8087".into());
    let tack_admin_token = std::env::var("TACK_ADMIN_TOKEN").context("TACK_ADMIN_TOKEN must be set")?;
    let team_ids: Vec<Uuid> = std::env::var("TEAM_IDS").unwrap_or_default().split(',').filter_map(|s| Uuid::parse_str(s.trim()).ok()).collect();

    if team_ids.is_empty() {
        anyhow::bail!("TEAM_IDS is required (comma-separated team UUIDs that were actually cut over)");
    }

    let forward: ForwardState = {
        let data = std::fs::read_to_string(&forward_state_path)
            .with_context(|| format!("reading forward state file {} -- run backfill-clann-notes first", forward_state_path.display()))?;
        serde_json::from_str(&data)?
    };
    // tack id -> surreal id (reverse of the forward map). The forward
    // state file's `migrated` map is flat across both kinds -- a note's key
    // is its plain clann ULID, a folder's key is `"folder:<id>|<team>"`
    // (see backfill-clann-notes.rs's own `State`) -- so `forward_reverse`
    // (used for both folder lookups and note reconciliation) keeps both,
    // but `forward_reverse_notes_only` exists specifically for the
    // deleted-post-cutover check below, which calls `GET /notes/{id}` and
    // would misfire on a folder's tack id (a syntactically valid UUID that
    // just happens to belong to a folder, not a note -- a real bug hit
    // live during rehearsal before this filter existed).
    let forward_reverse: HashMap<String, String> = forward.migrated.iter().map(|(k, v)| (v.to_string(), k.clone())).collect();
    let forward_reverse_notes_only: HashMap<String, String> =
        forward.migrated.iter().filter(|(k, _)| !k.starts_with("folder:")).map(|(k, v)| (v.to_string(), k.clone())).collect();
    let mut drain_state = DrainState::load(&state_path)?;

    tracing::info!(dry_run, teams = team_ids.len(), originally_migrated = forward.migrated.len(), "starting tack-server -> clann-server reverse drain");

    let clann = any::connect(&clann_db_url).await?;
    clann.signin(Root { username: clann_db_user, password: clann_db_pass }).await?;
    clann.use_ns(&clann_db_ns).use_db(&clann_db_db).await?;

    let http = reqwest::Client::new();

    let tree_rows: Vec<Value> = clann.query("SELECT name, meta::id(id) AS tree_id FROM family_tree").await?.take(0)?;
    let tree_id_to_name: HashMap<String, String> = tree_rows
        .iter()
        .filter_map(|r| Some((str_field(r, "tree_id")?.to_string(), str_field(r, "name")?.to_string())))
        .collect();

    let (mut folders_created, mut notes_created, mut notes_updated, mut replies_created) = (0u32, 0u32, 0u32, 0u32);
    let mut needs_review: Vec<String> = Vec::new();
    let mut descriptions_lost: Vec<String> = Vec::new();

    for team_id in &team_ids {
        tracing::info!(%team_id, "processing team");

        // ── Folders ──────────────────────────────────────────────────────
        let folders_resp = http
            .get(format!("{tack_api_url}/note-folders"))
            .bearer_auth(&tack_admin_token)
            .query(&[("team_id", team_id.to_string()), ("limit", "200".into())])
            .send()
            .await?;
        let folders_page: TackFoldersPage = folders_resp.json().await.context("parsing folders page")?;

        let mut folder_reverse: HashMap<Uuid, String> = HashMap::new();
        for f in &folders_page.folders {
            let key = f.id.to_string();
            if let Some(surreal_id) = forward_reverse.get(&key).or_else(|| drain_state.drained.get(&key)) {
                folder_reverse.insert(f.id, surreal_id.clone());
                continue;
            }
            if dry_run {
                tracing::info!(name = %f.name, tack_id = %f.id, "[dry-run] would create research_folder for new tack folder");
                continue;
            }
            let created: Vec<Value> = clann
                .query("CREATE research_folder SET name = $name, created_by = $creator")
                .bind(("name", f.name.clone()))
                .bind(("creator", UNKNOWN_AUTHOR_PLACEHOLDER.to_string()))
                .await?
                .take(0)?;
            let Some(surreal_id) = created.first().and_then(|r| str_field(r, "id")).map(str::to_string) else { continue };
            drain_state.record(&state_path, &key, &surreal_id)?;
            folder_reverse.insert(f.id, surreal_id);
            folders_created += 1;
            tracing::info!(name = %f.name, tack_id = %f.id, "created research_folder for new tack folder");
        }

        // ── Notes ────────────────────────────────────────────────────────
        let notes_resp = http
            .get(format!("{tack_api_url}/notes"))
            .bearer_auth(&tack_admin_token)
            .query(&[("team_id", team_id.to_string()), ("limit", "100".into())])
            .send()
            .await?;
        let notes_page: TackNotesPage = notes_resp.json().await.context("parsing notes page")?;
        let seen_tack_ids: HashSet<String> = notes_page.notes.iter().map(|n| n.id.to_string()).collect();

        for note in &notes_page.notes {
            let tack_id = note.id.to_string();

            let attachments_resp = http
                .get(format!("{tack_api_url}/notes/{}/attachments", note.id))
                .bearer_auth(&tack_admin_token)
                .send()
                .await;
            let attachments: Vec<TackAttachment> = match attachments_resp {
                Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
                _ => Vec::new(),
            };
            let trees_for_note: Vec<String> = attachments
                .iter()
                .filter(|a| a.owning_service == "clann" && a.entity_type == "tree")
                .filter_map(|a| tree_id_to_name.get(&a.entity_id).cloned())
                .collect();
            let is_shared = note.visibility != "private";
            let folder_legacy_id = note.folder_id.and_then(|fid| folder_reverse.get(&fid).cloned());

            let meta: Vec<Value> = clann
                .query("SELECT description FROM tack_note_meta WHERE tack_note_id = $tid LIMIT 1")
                .bind(("tid", tack_id.clone()))
                .await?
                .take(0)?;
            let description: Option<String> = match meta.first().and_then(|r| r.get("description")) {
                Some(Value::String(s)) => Some(s.clone()),
                _ => {
                    descriptions_lost.push(tack_id.clone());
                    None
                }
            };

            if let Some(surreal_id) = forward_reverse.get(&tack_id) {
                if dry_run {
                    tracing::info!(surreal_id, tack_id, "[dry-run] would reconcile existing note");
                } else {
                    let raw_id = surreal_id.strip_prefix("research_note:").unwrap_or(surreal_id);
                    clann
                        .query("UPDATE type::record('research_note', $id) SET title = $title, body = $body, trees = $trees, is_shared = $is_shared")
                        .bind(("id", raw_id.to_string()))
                        .bind(("title", note.title.clone()))
                        .bind(("body", note.body_markdown.clone()))
                        .bind(("trees", trees_for_note.clone()))
                        .bind(("is_shared", is_shared))
                        .await?;
                    notes_updated += 1;
                }
            } else if !drain_state.drained.contains_key(&tack_id) {
                let author_username = resolve_username(&http, &uum_url, &uum_admin_token, note.created_by)
                    .await
                    .unwrap_or_else(|| UNKNOWN_AUTHOR_PLACEHOLDER.to_string());
                if dry_run {
                    tracing::info!(title = %note.title, tack_id, "[dry-run] would create research_note for new tack note");
                } else {
                    let created: Vec<Value> = clann
                        .query(
                            "CREATE research_note SET title = $title, description = $desc, body = $body, \
                             trees = $trees, folder_id = $folder_id, created_by = $creator, is_shared = $shared",
                        )
                        .bind(("title", note.title.clone()))
                        .bind(("desc", description.clone()))
                        .bind(("body", note.body_markdown.clone()))
                        .bind(("trees", trees_for_note.clone()))
                        .bind(("folder_id", folder_legacy_id.clone()))
                        .bind(("creator", author_username))
                        .bind(("shared", is_shared))
                        .await?
                        .take(0)?;
                    let Some(surreal_id) = created.first().and_then(|r| str_field(r, "id")).map(str::to_string) else { continue };
                    drain_state.record(&state_path, &tack_id, &surreal_id)?;
                    notes_created += 1;
                    tracing::info!(title = %note.title, tack_id, surreal_id, "created research_note for new tack note");
                }
            }

            // Replies -- position-matched, see this file's own doc comment.
            let surreal_parent = forward_reverse.get(&tack_id).cloned().or_else(|| drain_state.drained.get(&tack_id).cloned());
            if let Some(parent_id) = surreal_parent {
                let raw_parent = parent_id.strip_prefix("research_note:").unwrap_or(&parent_id).to_string();
                let existing_replies: Vec<Value> = clann
                    .query("SELECT count() AS n FROM research_note WHERE parent_id = type::record('research_note', $pid) GROUP ALL")
                    .bind(("pid", raw_parent.clone()))
                    .await?
                    .take(0)?;
                let existing_count = existing_replies.first().and_then(|r| r.get("n")).and_then(Value::as_u64).unwrap_or(0) as usize;

                let replies_resp = http.get(format!("{tack_api_url}/notes/{}/replies", note.id)).bearer_auth(&tack_admin_token).send().await;
                let tack_replies: Vec<TackNote> = match replies_resp {
                    Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
                    _ => Vec::new(),
                };

                for reply in tack_replies.iter().skip(existing_count) {
                    if dry_run {
                        tracing::info!(tack_id, "[dry-run] would create reply");
                        continue;
                    }
                    let reply_author = resolve_username(&http, &uum_url, &uum_admin_token, reply.created_by)
                        .await
                        .unwrap_or_else(|| UNKNOWN_AUTHOR_PLACEHOLDER.to_string());
                    clann
                        .query(
                            "CREATE research_note SET title = $title, body = $body, trees = $trees, \
                             created_by = $creator, is_shared = true, parent_id = type::record('research_note', $pid)",
                        )
                        .bind(("title", format!("Re: {tack_id}")))
                        .bind(("body", reply.body_markdown.clone()))
                        .bind(("trees", trees_for_note.clone()))
                        .bind(("creator", reply_author))
                        .bind(("pid", raw_parent.clone()))
                        .await?;
                    replies_created += 1;
                }
            }
        }

        // Deleted-post-cutover detection.
        for (tack_id, surreal_id) in &forward_reverse_notes_only {
            if seen_tack_ids.contains(tack_id) {
                continue;
            }
            let Ok(tid) = Uuid::parse_str(tack_id) else { continue };
            let resp = http.get(format!("{tack_api_url}/notes/{tid}")).bearer_auth(&tack_admin_token).send().await;
            if matches!(resp, Ok(r) if r.status() == reqwest::StatusCode::NOT_FOUND) {
                needs_review.push(surreal_id.clone());
            }
        }
    }

    tracing::info!(
        dry_run, folders_created, notes_created, notes_updated, replies_created,
        needs_review = needs_review.len(), descriptions_lost = descriptions_lost.len(),
        "reverse drain complete"
    );
    if !needs_review.is_empty() {
        tracing::warn!(?needs_review, "notes deleted in tack post-cutover -- NOT auto-deleted here, review manually");
    }

    Ok(())
}

