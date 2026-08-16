//! One-off backfill: copies clann-server's `research_note`/`research_folder`
//! (SurrealDB) into tack-server, via tack-server's real `POST /notes`/
//! `POST /note-folders`/`POST /notes/:id/replies` API -- not a direct-to-
//! Postgres write, so this reuses `db::notes::create_note`'s actual
//! invariants (thread_path construction, the notes/note_bodies split,
//! `content_attachments`, indexing/embedding side effects) instead of
//! re-implementing them by hand in a script. Same shape as
//! `backfill-awe-notes.rs`/`backfill-awe-idea-boards.rs` in this same
//! `src/bin/` -- see `ullav-helm`'s `docs/notes-backfill-runbook.md`
//! ("the destination service owns its own import tooling") for why this
//! lives here and not in `clann-server`.
//!
//! Design template, copied from the runbook (read it before changing this
//! file): real API not direct-to-DB writes; admin-only `created_at`/
//! `created_by` overrides (verified directly against tack-server's own
//! `models/note.rs` -- `CreateNoteRequest`/`ReplyRequest` have them,
//! `CreateNoteFolderRequest` does NOT, so a migrated folder is always
//! attributed to whoever runs this, at the run's own timestamp -- a known,
//! accepted gap, no `@ullav-dev/tack-notes` component surfaces a folder's
//! own creator/date anywhere); `--dry-run` first, always; `--state-file`,
//! flushed after every write, resumable; skip and report, never guess.
//!
//! clann-server's own visibility model has no `team_id` on a note directly
//! -- it's derived via `research_note.trees[] -> family_tree.team_id`, and
//! that chain is lossy in several ways the Phase 0 census already named
//! (empty trees, an unresolvable slug, a tree with no team, trees spanning
//! more than one team). This script never resolves that ambiguity toward
//! *wider* exposure: a note whose trees resolve to more than one distinct
//! team still migrates -- picking the lexicographically-first team,
//! deterministic and reproducible across runs -- but always as
//! `Visibility::Private`, flagged in the log for a human to review
//! individually, exactly like a single ambiguous team choice would be.
//! Only "no team can be determined at all" is a hard skip (tack's own
//! `CreateNoteRequest.team_id` is a required `Uuid`).
//!
//! `description` (no column in tack's `Note` schema) is written to
//! clann-server's own `tack_note_meta` sidecar table (`002_tack_migration_
//! sidecar.surql`) via a direct SurrealDB write on the same connection used
//! to read the source rows -- this script already has that connection open,
//! and `tack_note_meta` is real, permanent Clann-side infrastructure (also
//! used by the future Phase 3 live-handler rewiring and by
//! `reverse-drain-clann-notes.rs`'s own description recovery), not
//! migration-run bookkeeping.
//!
//! Empty `body_markdown` is a real, hit-in-production failure mode (see the
//! runbook's Step 2/3): tack-server's `create_note`/`create_reply` both
//! reject an empty/whitespace-only body with a 400. clann's own
//! `research_note.body` is `Option<String>` with no non-empty constraint at
//! all, so this is skipped-and-reported here, never sent and never
//! papered over with fabricated content -- per the runbook, deciding what a
//! real empty-body row should contain (e.g. reusing its own title) is a
//! per-row human judgment call, not something this script should guess at.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use surrealdb::{engine::any, opt::auth::Root};
use uuid::Uuid;

/// clann-server's own record ids are ULID strings (e.g.
/// `"e37btfzau47anfoh7fmq"`, no `research_note:` prefix -- stripped at read
/// time via `meta::id()`), not UUIDs -- unlike awe-server's own backfill,
/// whose `State` keys are `Uuid`. One flat map for folders, notes, and
/// replies alike, same reasoning as `backfill-awe-notes.rs`'s own doc
/// comment on this.
#[derive(Default, Serialize, Deserialize)]
struct State {
    migrated: HashMap<String, Uuid>,
}

impl State {
    fn load(path: &PathBuf) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let data = std::fs::read_to_string(path).with_context(|| format!("reading state file {}", path.display()))?;
        serde_json::from_str(&data).with_context(|| format!("parsing state file {}", path.display()))
    }

    fn record(&mut self, path: &PathBuf, clann_id: &str, tack_id: Uuid) -> Result<()> {
        self.migrated.insert(clann_id.to_string(), tack_id);
        let data = serde_json::to_string_pretty(self).context("serializing state")?;
        std::fs::write(path, data).with_context(|| format!("writing state file {}", path.display()))
    }
}

// ── Source rows (SurrealDB) ──────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct ClannFolder {
    id: String,
    name: String,
    created_by: String,
}

#[derive(Debug, Clone)]
struct ClannNote {
    id: String,
    title: String,
    description: Option<String>,
    body: Option<String>,
    trees: Vec<String>,
    folder_id: Option<String>,
    created_by: Option<String>,
    created_at: Option<String>,
    is_shared: bool,
    parent_id: Option<String>,
}

fn str_field<'a>(row: &'a Value, field: &str) -> Option<&'a str> {
    row.get(field)?.as_str()
}
fn str_arr(row: &Value, field: &str) -> Vec<String> {
    row.get(field)
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}
fn bool_field(row: &Value, field: &str) -> bool {
    row.get(field).and_then(|v| v.as_bool()).unwrap_or(false)
}

fn parse_note(r: &Value) -> Option<ClannNote> {
    Some(ClannNote {
        id: str_field(r, "id")?.to_string(),
        title: str_field(r, "title").unwrap_or("").to_string(),
        description: str_field(r, "description").map(str::to_string),
        body: str_field(r, "body").map(str::to_string),
        trees: str_arr(r, "trees"),
        folder_id: str_field(r, "folder_id").map(str::to_string),
        created_by: str_field(r, "created_by").map(str::to_string),
        created_at: str_field(r, "created_at").map(str::to_string),
        is_shared: bool_field(r, "is_shared"),
        parent_id: str_field(r, "parent_id").map(str::to_string),
    })
}

// ── UUM resolution (bulk-loaded, before the per-note loop) ──────────────────

#[derive(Debug, Deserialize)]
struct AdminUserSearchRow {
    id: Uuid,
    username: String,
}
#[derive(Debug, Deserialize)]
struct AdminUsersPage {
    users: Vec<AdminUserSearchRow>,
}

/// username -> UUM UUID, via `GET /admin/users?search=`, filtered to an
/// exact case-insensitive match (`search` is a substring match server-side).
async fn resolve_usernames(
    http: &reqwest::Client,
    uum_base: &str,
    uum_token: &str,
    usernames: &HashSet<String>,
) -> HashMap<String, Uuid> {
    let mut resolved = HashMap::new();
    for username in usernames {
        let resp = http
            .get(format!("{uum_base}/admin/users"))
            .bearer_auth(uum_token)
            .query(&[("search", username.as_str()), ("page_size", "50")])
            .send()
            .await;
        let page: AdminUsersPage = match resp {
            Ok(r) if r.status().is_success() => match r.json().await {
                Ok(p) => p,
                Err(_) => continue,
            },
            _ => continue,
        };
        let matches: Vec<&AdminUserSearchRow> =
            page.users.iter().filter(|u| u.username.eq_ignore_ascii_case(username)).collect();
        match matches.as_slice() {
            [one] => {
                resolved.insert(username.clone(), one.id);
            }
            [] => tracing::warn!(%username, "skipping: no matching UUM user (unresolvable username)"),
            _ => tracing::warn!(%username, "skipping: more than one exact username match (should be impossible, username is UNIQUE)"),
        }
    }
    resolved
}

#[derive(Debug, Deserialize)]
struct AdminTeamLookup {
    organization_id: Option<Uuid>,
}

/// team UUID -> organization UUID, via `GET /admin/teams/{id}` -- the same
/// endpoint tack-server's own `resolve_team_organization_live` calls at
/// request time, so a team this resolves as "no organization" is exactly a
/// team that would 400 out of `POST /notes` if it slipped through
/// unfiltered.
async fn resolve_team_organizations(
    http: &reqwest::Client,
    uum_base: &str,
    uum_token: &str,
    team_ids: &HashSet<String>,
) -> HashMap<String, Uuid> {
    let mut resolved = HashMap::new();
    for team_id in team_ids {
        let resp = http.get(format!("{uum_base}/admin/teams/{team_id}")).bearer_auth(uum_token).send().await;
        match resp {
            Ok(r) if r.status().is_success() => match r.json::<AdminTeamLookup>().await {
                Ok(AdminTeamLookup { organization_id: Some(org_id) }) => {
                    resolved.insert(team_id.clone(), org_id);
                }
                Ok(AdminTeamLookup { organization_id: None }) => {
                    tracing::warn!(%team_id, "skipping: team has no organization assigned yet");
                }
                Err(_) => tracing::warn!(%team_id, "skipping: malformed team lookup response"),
            },
            Ok(r) if r.status() == reqwest::StatusCode::NOT_FOUND => {
                tracing::warn!(%team_id, "skipping: team no longer exists in UUM")
            }
            _ => tracing::warn!(%team_id, "skipping: team lookup failed (transport/auth)"),
        }
    }
    resolved
}

// ── Verification helpers ─────────────────────────────────────────────────────

fn sha256_hex(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    format!("{:x}", hasher.finalize())
}
fn dam_url_count(body: &str) -> usize {
    body.matches("](http").count()
}

#[derive(Deserialize)]
struct TackNote {
    id: Uuid,
    body_markdown: String,
}
#[derive(Deserialize)]
struct TackFolder {
    id: Uuid,
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
        .unwrap_or_else(|| PathBuf::from("backfill-clann-notes-state.json"));

    let clann_db_url = std::env::var("CLANN_DB_URL").context("CLANN_DB_URL must be set (e.g. ws://localhost:8000)")?;
    let clann_db_ns = std::env::var("CLANN_DB_NAMESPACE").unwrap_or_else(|_| "clann".to_string());
    let clann_db_db = std::env::var("CLANN_DB_DATABASE").unwrap_or_else(|_| "ancestry".to_string());
    let clann_db_user = std::env::var("CLANN_DB_USERNAME").context("CLANN_DB_USERNAME must be set")?;
    let clann_db_pass = std::env::var("CLANN_DB_PASSWORD").context("CLANN_DB_PASSWORD must be set")?;

    let uum_url = std::env::var("UUM_URL").unwrap_or_else(|_| "http://localhost:8081".into());
    let uum_admin_token = std::env::var("UUM_ADMIN_TOKEN").context("UUM_ADMIN_TOKEN must be set")?;
    let tack_api_url = std::env::var("TACK_API_URL").unwrap_or_else(|_| "http://localhost:8087".into());
    let tack_admin_token = std::env::var("TACK_ADMIN_TOKEN").context("TACK_ADMIN_TOKEN must be set")?;

    let mut state = State::load(&state_path)?;
    tracing::info!(dry_run, %tack_api_url, state_file = %state_path.display(), already_migrated = state.migrated.len(), "starting clann-server -> tack-server notes backfill");

    let clann = any::connect(&clann_db_url).await.context("connecting to clann-server's SurrealDB")?;
    clann.signin(Root { username: clann_db_user, password: clann_db_pass }).await?;
    clann.use_ns(&clann_db_ns).use_db(&clann_db_db).await?;

    let http = reqwest::Client::new();

    // ── Load source rows ──────────────────────────────────────────────────
    let tree_rows: Vec<Value> =
        clann.query("SELECT name, meta::id(id) AS tree_id, team_id FROM family_tree").await?.take(0)?;
    let folder_rows: Vec<Value> =
        clann.query("SELECT meta::id(id) AS id, name, created_by FROM research_folder").await?.take(0)?;
    // Split top-level/replies -- `meta::id()` requires a `record` argument
    // and errors outright on `NONE` (top-level notes have no parent_id at
    // all, not a record -- verified directly against a real instance).
    let top_level_rows: Vec<Value> = clann
        .query(
            "SELECT meta::id(id) AS id, title, description, body, trees, folder_id, \
             created_by, created_at, is_shared FROM research_note WHERE parent_id = NONE ORDER BY created_at ASC",
        )
        .await?
        .take(0)?;
    let reply_rows: Vec<Value> = clann
        .query(
            "SELECT meta::id(id) AS id, title, description, body, trees, folder_id, \
             created_by, created_at, is_shared, meta::id(parent_id) AS parent_id \
             FROM research_note WHERE parent_id != NONE ORDER BY created_at ASC",
        )
        .await?
        .take(0)?;

    let trees: HashMap<String, (String, Option<String>)> = tree_rows
        .iter()
        .filter_map(|r| Some((str_field(r, "name")?.to_string(), (str_field(r, "tree_id")?.to_string(), str_field(r, "team_id").map(str::to_string)))))
        .collect();
    let folders: Vec<ClannFolder> = folder_rows
        .iter()
        .filter_map(|r| {
            Some(ClannFolder {
                id: str_field(r, "id")?.to_string(),
                name: str_field(r, "name")?.to_string(),
                created_by: str_field(r, "created_by").unwrap_or("").to_string(),
            })
        })
        .collect();
    let top_level: Vec<ClannNote> = top_level_rows.iter().filter_map(parse_note).collect();
    let replies: Vec<ClannNote> = reply_rows.iter().filter_map(parse_note).collect();
    let replies_by_parent: HashMap<String, Vec<&ClannNote>> = {
        let mut m: HashMap<String, Vec<&ClannNote>> = HashMap::new();
        for r in &replies {
            if let Some(p) = &r.parent_id {
                m.entry(p.clone()).or_default().push(r);
            }
        }
        m
    };

    tracing::info!(trees = trees.len(), folders = folders.len(), top_level = top_level.len(), replies = replies.len(), "loaded from clann-server");

    // ── Bulk resolution ──────────────────────────────────────────────────
    let mut usernames: HashSet<String> = HashSet::new();
    for f in &folders {
        if !f.created_by.is_empty() {
            usernames.insert(f.created_by.clone());
        }
    }
    for n in top_level.iter().chain(replies.iter()) {
        if let Some(u) = &n.created_by {
            usernames.insert(u.clone());
        }
    }
    let username_map = resolve_usernames(&http, &uum_url, &uum_admin_token, &usernames).await;

    let candidate_team_ids: HashSet<String> = trees.values().filter_map(|(_, team)| team.clone()).collect();
    let team_org_map = resolve_team_organizations(&http, &uum_url, &uum_admin_token, &candidate_team_ids).await;

    tracing::info!(
        usernames_resolved = username_map.len(),
        usernames_total = usernames.len(),
        teams_with_org = team_org_map.len(),
        teams_candidate = candidate_team_ids.len(),
        "resolution complete"
    );

    // ── Decide every note's team/visibility (A0) ────────────────────────
    enum Decision {
        Migrate { team_id: String, visibility: &'static str },
        Ambiguous { team_id: String, all_candidates: Vec<String> },
        Skip(&'static str),
    }
    fn decide(is_shared: bool, resolved_team_ids: &BTreeSet<String>, had_slugs: bool, had_resolved_trees: bool) -> Decision {
        if !had_slugs {
            return Decision::Skip("no_trees");
        }
        if !had_resolved_trees {
            return Decision::Skip("all_tree_slugs_unresolvable");
        }
        match resolved_team_ids.len() {
            0 => Decision::Skip("no_team_on_any_resolved_tree"),
            1 => Decision::Migrate {
                team_id: resolved_team_ids.iter().next().unwrap().clone(),
                visibility: if is_shared { "team" } else { "private" },
            },
            _ => Decision::Ambiguous {
                team_id: resolved_team_ids.iter().next().unwrap().clone(), // BTreeSet is sorted -- deterministic
                all_candidates: resolved_team_ids.iter().cloned().collect(),
            },
        }
    }

    let mut decisions: HashMap<&str, Decision> = HashMap::new();
    for n in &top_level {
        let had_slugs = !n.trees.is_empty();
        let resolved_trees: Vec<&(String, Option<String>)> = n.trees.iter().filter_map(|slug| trees.get(slug)).collect();
        let had_resolved_trees = !resolved_trees.is_empty();
        let resolved_team_ids: BTreeSet<String> = resolved_trees
            .iter()
            .filter_map(|(_, team)| team.clone())
            .filter(|team_id| team_org_map.contains_key(team_id))
            .collect();
        decisions.insert(&n.id, decide(n.is_shared, &resolved_team_ids, had_slugs, had_resolved_trees));
    }

    let (mut to_migrate, mut ambiguous, mut skipped) = (0usize, 0usize, 0usize);
    for d in decisions.values() {
        match d {
            Decision::Migrate { .. } => to_migrate += 1,
            Decision::Ambiguous { .. } => {
                to_migrate += 1;
                ambiguous += 1;
            }
            Decision::Skip(reason) => {
                skipped += 1;
                tracing::warn!(reason, "note decision: skip");
            }
        }
    }
    tracing::info!(to_migrate, ambiguous, skipped, "A0 decisions complete");

    // Derived (legacy_folder_id, team_id) pairs -- known before any note is
    // created, so no separate folder-repoint phase is needed at all.
    let mut needed_folders: BTreeSet<(String, String)> = BTreeSet::new();
    for n in &top_level {
        let team_id = match decisions.get(n.id.as_str()) {
            Some(Decision::Migrate { team_id, .. }) | Some(Decision::Ambiguous { team_id, .. }) => team_id.clone(),
            _ => continue,
        };
        if let Some(folder_id) = &n.folder_id {
            needed_folders.insert((folder_id.clone(), team_id));
        }
    }
    tracing::info!(folders_needed = needed_folders.len(), "derived folder set");

    if dry_run {
        tracing::info!("dry run complete -- zero writes performed. Re-run without --dry-run once every WARN above has been reviewed.");
        return Ok(());
    }

    // ── Step A: create derived folders ──────────────────────────────────
    let folder_by_id: HashMap<&str, &ClannFolder> = folders.iter().map(|f| (f.id.as_str(), f)).collect();
    let mut folder_map: HashMap<(String, String), Uuid> = HashMap::new();
    let (mut folders_created, mut folders_skipped) = (0u32, 0u32);
    for (legacy_id, team_id) in &needed_folders {
        let state_key = format!("folder:{legacy_id}|{team_id}");
        if let Some(existing) = state.migrated.get(&state_key) {
            folder_map.insert((legacy_id.clone(), team_id.clone()), *existing);
            continue;
        }
        let name = folder_by_id.get(legacy_id.as_str()).map(|f| f.name.as_str()).unwrap_or("Unfiled");
        let Ok(team_uuid) = Uuid::parse_str(team_id) else { continue };
        let resp = http
            .post(format!("{tack_api_url}/note-folders"))
            .bearer_auth(&tack_admin_token)
            .json(&json!({ "team_id": team_uuid, "name": name }))
            .send()
            .await
            .with_context(|| format!("creating folder {legacy_id}"))?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            tracing::error!(legacy_id, name, body, "failed to create folder");
            folders_skipped += 1;
            continue;
        }
        let created: TackFolder = resp.json().await.context("parsing created folder")?;
        folder_map.insert((legacy_id.clone(), team_id.clone()), created.id);
        state.record(&state_path, &state_key, created.id)?;
        folders_created += 1;
        tracing::info!(legacy_id, name, team_id, tack_id = %created.id, "created folder");
    }
    tracing::info!(folders_created, folders_skipped, "Step A complete");

    // ── Step B: create notes + replies ───────────────────────────────────
    let (mut notes_created, mut notes_already_migrated, mut notes_skipped) = (0u32, 0u32, 0u32);
    let (mut replies_created, mut replies_skipped) = (0u32, 0u32);
    let (mut body_mismatches, mut dam_mismatches) = (0u32, 0u32);

    for n in &top_level {
        let (team_id, visibility, is_ambiguous, all_candidates) = match decisions.get(n.id.as_str()) {
            Some(Decision::Migrate { team_id, visibility }) => (team_id.clone(), *visibility, false, vec![]),
            Some(Decision::Ambiguous { team_id, all_candidates }) => (team_id.clone(), "private", true, all_candidates.clone()),
            _ => continue,
        };
        if is_ambiguous {
            tracing::warn!(note_id = %n.id, %team_id, ?all_candidates, "note resolves to multiple teams -- migrated as private, chosen team is the lexicographically-first candidate; review individually");
        }

        if let Some(existing) = state.migrated.get(&n.id).copied() {
            notes_already_migrated += 1;
            tracing::info!(note_id = %n.id, tack_id = %existing, "already migrated, skipping (per state file) -- still checking replies");
            reconcile_replies(&http, &tack_api_url, &tack_admin_token, &username_map, &mut state, &state_path, n, existing, &replies_by_parent, &mut replies_created, &mut replies_skipped).await?;
            continue;
        }

        let body = match &n.body {
            Some(b) if !b.trim().is_empty() => b.clone(),
            _ => {
                tracing::warn!(note_id = %n.id, title = %n.title, "skipping: empty body_markdown (tack-server rejects this) -- decide per-row whether to reuse the title as body, or skip as junk, per the runbook's Step 3");
                notes_skipped += 1;
                continue;
            }
        };
        let title = if n.title.trim().is_empty() { "Untitled".to_string() } else { n.title.clone() };
        let Some(author_username) = &n.created_by else {
            tracing::warn!(note_id = %n.id, "skipping: no created_by at all");
            notes_skipped += 1;
            continue;
        };
        let Some(&author_uuid) = username_map.get(author_username) else {
            tracing::warn!(note_id = %n.id, author_username, "skipping: unresolvable author username");
            notes_skipped += 1;
            continue;
        };
        let Ok(team_uuid) = Uuid::parse_str(&team_id) else { continue };
        let folder_uuid = n.folder_id.as_ref().and_then(|fid| folder_map.get(&(fid.clone(), team_id.clone()))).copied();
        let created_at = n.created_at.as_deref().and_then(|s| s.parse::<chrono::DateTime<chrono::Utc>>().ok());
        let body_before_hash = sha256_hex(&body);
        let dam_before = dam_url_count(&body);

        let resp = http
            .post(format!("{tack_api_url}/notes"))
            .bearer_auth(&tack_admin_token)
            .json(&json!({
                "team_id": team_uuid,
                "visibility": visibility,
                "title": title,
                "body_markdown": body,
                "folder_id": folder_uuid,
                "created_at": created_at,
                "created_by": author_uuid,
            }))
            .send()
            .await
            .with_context(|| format!("creating note {}", n.id))?;
        if !resp.status().is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            tracing::error!(note_id = %n.id, body = body_text, "failed to create note");
            notes_skipped += 1;
            continue;
        }
        let created: TackNote = resp.json().await.context("parsing created note")?;

        if sha256_hex(&created.body_markdown) != body_before_hash {
            body_mismatches += 1;
            tracing::error!(note_id = %n.id, tack_id = %created.id, "BODY HASH MISMATCH");
        }
        if dam_url_count(&created.body_markdown) != dam_before {
            dam_mismatches += 1;
            tracing::error!(note_id = %n.id, tack_id = %created.id, "DAM URL COUNT MISMATCH");
        }

        for tree_slug in &n.trees {
            if let Some((tree_id, _)) = trees.get(tree_slug) {
                let _ = http
                    .post(format!("{tack_api_url}/notes/{}/attachments", created.id))
                    .bearer_auth(&tack_admin_token)
                    .json(&json!({ "owning_service": "clann", "entity_type": "tree", "entity_id": tree_id }))
                    .send()
                    .await;
            }
        }

        if let Some(desc) = &n.description {
            let _ = clann
                .query("CREATE tack_note_meta SET tack_note_id = $tid, description = $desc")
                .bind(("tid", created.id.to_string()))
                .bind(("desc", desc.clone()))
                .await;
        }

        state.record(&state_path, &n.id, created.id)?;
        notes_created += 1;
        tracing::info!(note_id = %n.id, tack_id = %created.id, title, "created note");

        reconcile_replies(&http, &tack_api_url, &tack_admin_token, &username_map, &mut state, &state_path, n, created.id, &replies_by_parent, &mut replies_created, &mut replies_skipped).await?;
    }

    tracing::info!(
        notes_created, notes_already_migrated, notes_skipped, replies_created, replies_skipped,
        body_mismatches, dam_mismatches,
        "backfill complete"
    );
    if body_mismatches > 0 || dam_mismatches > 0 {
        tracing::error!("COMPLETED WITH DISCREPANCIES -- review before treating this run as clean");
    }

    Ok(())
}

/// Creates any of `note`'s replies not yet in `state`, attributed to their
/// own real author (bulk-resolved `username_map`) with their own original
/// timestamp. Empty-body replies are skipped with the same discipline as
/// top-level notes.
#[allow(clippy::too_many_arguments)]
async fn reconcile_replies(
    http: &reqwest::Client,
    tack_api_url: &str,
    tack_admin_token: &str,
    username_map: &HashMap<String, Uuid>,
    state: &mut State,
    state_path: &PathBuf,
    note: &ClannNote,
    tack_note_id: Uuid,
    replies_by_parent: &HashMap<String, Vec<&ClannNote>>,
    replies_created: &mut u32,
    replies_skipped: &mut u32,
) -> Result<()> {
    let Some(thread_replies) = replies_by_parent.get(&note.id) else { return Ok(()) };
    for r in thread_replies {
        if state.migrated.contains_key(&r.id) {
            continue;
        }
        let body = match &r.body {
            Some(b) if !b.trim().is_empty() => b.clone(),
            _ => {
                tracing::warn!(reply_id = %r.id, "skipping reply: empty body_markdown");
                *replies_skipped += 1;
                continue;
            }
        };
        let Some(author_username) = &r.created_by else {
            tracing::warn!(reply_id = %r.id, "skipping reply: no created_by");
            *replies_skipped += 1;
            continue;
        };
        let Some(&author_uuid) = username_map.get(author_username) else {
            tracing::warn!(reply_id = %r.id, author_username, "skipping reply: unresolvable author username");
            *replies_skipped += 1;
            continue;
        };
        let created_at = r.created_at.as_deref().and_then(|s| s.parse::<chrono::DateTime<chrono::Utc>>().ok());
        let resp = http
            .post(format!("{tack_api_url}/notes/{tack_note_id}/replies"))
            .bearer_auth(tack_admin_token)
            .json(&json!({ "body_markdown": body, "created_at": created_at, "created_by": author_uuid }))
            .send()
            .await
            .with_context(|| format!("creating reply {}", r.id))?;
        if !resp.status().is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            tracing::error!(reply_id = %r.id, body = body_text, "failed to create reply");
            *replies_skipped += 1;
            continue;
        }
        let created: TackNote = resp.json().await.context("parsing created reply")?;
        state.record(state_path, &r.id, created.id)?;
        *replies_created += 1;
    }
    Ok(())
}
