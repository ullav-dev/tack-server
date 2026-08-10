//! One-off backfill: copies awe-server's plain Notes/note_folders
//! (`entity_type IN ('task','workflow','job','project')`) into tack-server,
//! via tack-server's real `POST /notes`/`POST /note-folders`/
//! `POST /notes/:id/replies` API — not a direct-to-Postgres write, so this
//! reuses `db::notes::create_note`'s actual invariants (thread_path
//! construction, the notes/note_bodies split, `content_attachments`)
//! instead of re-implementing them by hand in a script. See the AWE-apps
//! Notes migration plan, Phase 2.
//!
//! Deliberately excludes `pull_request`/`commit` (lagan's own entity types,
//! Phase 4's job) and Idea Boards (`folder_type = 'ideas_board'`, Phase 5's
//! job) -- both are left untouched in awe-server's DB by this script.
//!
//! awe-server's `notes` table carries no `team_id` of its own -- a note's
//! team is derived by walking to its owning entity: `workflow`/`project`/
//! `job` all have a direct `team_id` column; `task` has none, so its team is
//! resolved via `tasks.workflow_id -> workflows.team_id`. A note whose
//! entity (or that entity's team) can't be resolved -- an orphaned/deleted
//! entity, or a `team_id` that's itself `NULL` -- is skipped and reported,
//! never silently dropped or guessed at.
//!
//! Uses tack-server's real `POST /notes`/`POST /notes/:id/replies` admin-only
//! `created_at`/`created_by` overrides to preserve the original authorship
//! and timestamps, and `notes_acl::resolve_team_organization_live` (this
//! same migration's Phase 2 prerequisite) so the calling admin doesn't need
//! to be a JWT-claimed member of every team being backfilled.
//!
//! Run with `--dry-run` first: resolves every note's team/organization and
//! prints exactly what would be created, without writing anything.
//!
//! Re-run safety: `--state-file <path>` (default `backfill-awe-notes-state.json`
//! in the working directory) persists `awe id -> tack id` for every folder/
//! note/reply successfully created, flushed to disk after each one -- not
//! just at the end, so a crash or Ctrl-C mid-run loses no completed
//! progress. On startup, anything already in the state file is skipped
//! rather than recreated, so re-running after a partial failure (or by
//! mistake) is safe and resumes instead of duplicating. There's no
//! equivalent server-side dedup (tack-server has no way to know "this note
//! already came from awe note X") -- the state file is the only source of
//! truth for that, so don't delete it between runs of the same migration.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use tokio_postgres::NoTls;
use uuid::Uuid;

/// `awe id -> tack id`, for folders, top-level notes, and replies alike
/// (all three id spaces are UUIDs with no overlap risk in practice, and
/// keeping one flat map is simpler than three).
#[derive(Default, Serialize, Deserialize)]
struct State {
    migrated: HashMap<Uuid, Uuid>,
}

impl State {
    fn load(path: &PathBuf) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let data = std::fs::read_to_string(path).with_context(|| format!("reading state file {}", path.display()))?;
        serde_json::from_str(&data).with_context(|| format!("parsing state file {}", path.display()))
    }

    /// Records one successful creation and flushes immediately -- a
    /// migration run can take a while and nobody should lose completed
    /// progress to a crash near the end.
    fn record(&mut self, path: &PathBuf, awe_id: Uuid, tack_id: Uuid) -> Result<()> {
        self.migrated.insert(awe_id, tack_id);
        let data = serde_json::to_string_pretty(self).context("serializing state")?;
        std::fs::write(path, data).with_context(|| format!("writing state file {}", path.display()))
    }
}

#[derive(Debug, Clone)]
struct AweNote {
    id: Uuid,
    entity_type: String,
    entity_id: Uuid,
    title: String,
    body: String,
    is_shared: bool,
    parent_id: Option<Uuid>,
    folder_id: Option<Uuid>,
    created_by: String,
    created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone)]
struct AweFolder {
    id: Uuid,
    name: String,
    entity_type: Option<String>,
    entity_id: Option<Uuid>,
}

#[derive(Deserialize)]
struct TackNote {
    id: Uuid,
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
        .unwrap_or_else(|| PathBuf::from("backfill-awe-notes-state.json"));
    let awe_database_url = std::env::var("AWE_DATABASE_URL").context("AWE_DATABASE_URL must be set")?;
    let tack_api_url = std::env::var("TACK_API_URL").unwrap_or_else(|_| "http://localhost:8087".into());
    let tack_admin_token = std::env::var("TACK_ADMIN_TOKEN").context("TACK_ADMIN_TOKEN must be set")?;

    let mut state = State::load(&state_path)?;
    tracing::info!(dry_run, %tack_api_url, state_file = %state_path.display(), already_migrated = state.migrated.len(), "starting awe-server -> tack-server notes backfill");

    let (awe, connection) = tokio_postgres::connect(&awe_database_url, NoTls).await.context("connecting to awe-server's database")?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            tracing::error!("awe-server db connection error: {e}");
        }
    });

    let http = reqwest::Client::new();

    // ── Build entity -> team_id resolvers ───────────────────────────────

    let mut workflow_team: HashMap<Uuid, Option<Uuid>> = HashMap::new();
    for row in awe.query("SELECT id, team_id FROM workflows", &[]).await? {
        workflow_team.insert(row.get(0), row.get(1));
    }
    let mut project_team: HashMap<Uuid, Option<Uuid>> = HashMap::new();
    for row in awe.query("SELECT id, team_id FROM projects", &[]).await? {
        project_team.insert(row.get(0), row.get(1));
    }
    let mut job_team: HashMap<Uuid, Option<Uuid>> = HashMap::new();
    for row in awe.query("SELECT id, team_id FROM jobs", &[]).await? {
        job_team.insert(row.get(0), row.get(1));
    }
    let mut task_workflow: HashMap<Uuid, Uuid> = HashMap::new();
    for row in awe.query("SELECT id, workflow_id FROM tasks", &[]).await? {
        task_workflow.insert(row.get(0), row.get(1));
    }

    let resolve_team = |entity_type: &str, entity_id: Uuid| -> Option<Uuid> {
        match entity_type {
            "workflow" => workflow_team.get(&entity_id).copied().flatten(),
            "project" => project_team.get(&entity_id).copied().flatten(),
            "job" => job_team.get(&entity_id).copied().flatten(),
            "task" => task_workflow.get(&entity_id).and_then(|wf| workflow_team.get(wf).copied().flatten()),
            _ => None,
        }
    };

    // ── Folders (general only -- ideas_board is Phase 5's job) ──────────

    let mut awe_folders = Vec::new();
    for row in awe.query(
        "SELECT id, name, entity_type, entity_id FROM note_folders WHERE folder_type = 'general'",
        &[],
    )
    .await?
    {
        awe_folders.push(AweFolder { id: row.get(0), name: row.get(1), entity_type: row.get(2), entity_id: row.get(3) });
    }

    // A folder with its own entity_type/entity_id resolves the same way a
    // note does. A folder with neither (awe-server allows a fully
    // unscoped "general" folder -- no team concept at all there) has no
    // resolvable team of its own; infer one from whichever notes are
    // actually filed in it, if they all agree on a team. No notes agree
    // (or none are filed there) -> skipped and reported, not guessed at.
    let mut folder_team: HashMap<Uuid, Option<Uuid>> = HashMap::new();
    for f in &awe_folders {
        if let (Some(et), Some(eid)) = (&f.entity_type, f.entity_id) {
            folder_team.insert(f.id, resolve_team(et, eid));
        }
    }

    let mut all_notes = Vec::new();
    for row in awe.query(
        "SELECT id, entity_type, entity_id, title, body, is_shared, parent_id, folder_id, created_by, created_at
         FROM notes WHERE entity_type IN ('task', 'workflow', 'job', 'project') ORDER BY created_at",
        &[],
    )
    .await?
    {
        all_notes.push(AweNote {
            id: row.get(0),
            entity_type: row.get(1),
            entity_id: row.get(2),
            title: row.get(3),
            body: row.get(4),
            is_shared: row.get(5),
            parent_id: row.get(6),
            folder_id: row.get(7),
            created_by: row.get(8),
            created_at: row.get(9),
        });
    }

    for f in &awe_folders {
        if folder_team.contains_key(&f.id) {
            continue;
        }
        let mut inferred: Option<Uuid> = None;
        let mut consistent = true;
        for n in all_notes.iter().filter(|n| n.folder_id == Some(f.id)) {
            match resolve_team(&n.entity_type, n.entity_id) {
                Some(team) if inferred.is_none() => inferred = Some(team),
                Some(team) if inferred == Some(team) => {}
                _ => {
                    consistent = false;
                    break;
                }
            }
        }
        folder_team.insert(f.id, if consistent { inferred } else { None });
    }

    let (top_level, replies): (Vec<_>, Vec<_>) = all_notes.iter().partition(|n| n.parent_id.is_none());

    tracing::info!(
        folders = awe_folders.len(),
        top_level = top_level.len(),
        replies = replies.len(),
        "loaded from awe-server"
    );

    // ── Write folders first (top-level notes may reference them) ────────

    let mut folder_id_map: HashMap<Uuid, Uuid> = state.migrated.clone();
    let mut folders_created = 0u32;
    let mut folders_skipped = 0u32;
    for f in &awe_folders {
        if state.migrated.contains_key(&f.id) {
            tracing::info!(folder_id = %f.id, name = %f.name, "already migrated, skipping (per state file)");
            continue;
        }
        let Some(team_id) = folder_team.get(&f.id).copied().flatten() else {
            tracing::warn!(folder_id = %f.id, name = %f.name, "skipping folder: could not resolve a team");
            folders_skipped += 1;
            continue;
        };
        if dry_run {
            tracing::info!(folder_id = %f.id, name = %f.name, %team_id, "[dry-run] would create folder");
            folders_created += 1;
            continue;
        }
        let resp = http
            .post(format!("{tack_api_url}/note-folders"))
            .bearer_auth(&tack_admin_token)
            .json(&json!({ "team_id": team_id, "name": f.name }))
            .send()
            .await
            .with_context(|| format!("creating folder {}", f.id))?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            tracing::error!(folder_id = %f.id, name = %f.name, body, "failed to create folder");
            continue;
        }
        let created: TackFolder = resp.json().await.context("parsing created folder")?;
        folder_id_map.insert(f.id, created.id);
        state.record(&state_path, f.id, created.id)?;
        folders_created += 1;
    }

    // ── Write top-level notes, then their replies ────────────────────────

    let mut notes_created = 0u32;
    let mut notes_skipped = 0u32;
    let mut replies_created = 0u32;
    let mut replies_skipped = 0u32;

    for n in &top_level {
        let mut thread_replies: Vec<&AweNote> = replies.iter().copied().filter(|r| r.parent_id == Some(n.id)).collect();
        thread_replies.sort_by_key(|r| r.created_at);

        // The note itself may already be migrated from a prior run even if
        // some of its replies weren't (a crash between them) -- resume
        // reply-by-reply rather than treating "note already exists" as
        // "thread fully done."
        let existing_tack_note_id = state.migrated.get(&n.id).copied();
        if existing_tack_note_id.is_some() {
            tracing::info!(note_id = %n.id, title = %n.title, "note already migrated, skipping (per state file) -- still checking its replies");
        }

        let created_id = if let Some(id) = existing_tack_note_id {
            id
        } else {
            let Some(team_id) = resolve_team(&n.entity_type, n.entity_id) else {
                tracing::warn!(note_id = %n.id, entity_type = %n.entity_type, entity_id = %n.entity_id, "skipping note: could not resolve a team");
                notes_skipped += 1;
                continue;
            };
            let Ok(created_by) = n.created_by.parse::<Uuid>() else {
                tracing::warn!(note_id = %n.id, created_by = %n.created_by, "skipping note: created_by is not a UUID");
                notes_skipped += 1;
                continue;
            };
            let visibility = if n.is_shared { "team" } else { "private" };
            let tack_folder_id = n.folder_id.and_then(|old| folder_id_map.get(&old)).copied();

            if dry_run {
                tracing::info!(
                    note_id = %n.id, %team_id, visibility, title = %n.title, %created_by,
                    folder_id = ?tack_folder_id, replies = thread_replies.len(), "[dry-run] would create note"
                );
                notes_created += 1;
                replies_created += thread_replies.len() as u32;
                continue;
            }

            let resp = http
                .post(format!("{tack_api_url}/notes"))
                .bearer_auth(&tack_admin_token)
                .json(&json!({
                    "team_id": team_id,
                    "visibility": visibility,
                    "title": n.title,
                    "body_markdown": n.body,
                    "folder_id": tack_folder_id,
                    "attach": { "owning_service": "awe", "entity_type": n.entity_type, "entity_id": n.entity_id.to_string() },
                    "created_at": n.created_at,
                    "created_by": created_by,
                }))
                .send()
                .await
                .with_context(|| format!("creating note {}", n.id))?;
            if !resp.status().is_success() {
                let body = resp.text().await.unwrap_or_default();
                tracing::error!(note_id = %n.id, body, "failed to create note");
                notes_skipped += 1;
                continue;
            }
            let created: TackNote = resp.json().await.context("parsing created note")?;
            notes_created += 1;
            state.record(&state_path, n.id, created.id)?;
            created.id
        };

        for r in thread_replies {
            if state.migrated.contains_key(&r.id) {
                tracing::info!(reply_id = %r.id, "reply already migrated, skipping (per state file)");
                continue;
            }
            let Ok(reply_created_by) = r.created_by.parse::<Uuid>() else {
                tracing::warn!(reply_id = %r.id, created_by = %r.created_by, "skipping reply: created_by is not a UUID");
                replies_skipped += 1;
                continue;
            };
            let resp = http
                .post(format!("{tack_api_url}/notes/{created_id}/replies"))
                .bearer_auth(&tack_admin_token)
                .json(&json!({
                    "body_markdown": r.body,
                    "created_at": r.created_at,
                    "created_by": reply_created_by,
                }))
                .send()
                .await
                .with_context(|| format!("creating reply {}", r.id))?;
            if !resp.status().is_success() {
                let body = resp.text().await.unwrap_or_default();
                tracing::error!(reply_id = %r.id, body, "failed to create reply");
                replies_skipped += 1;
                continue;
            }
            let created_reply: TackNote = resp.json().await.context("parsing created reply")?;
            state.record(&state_path, r.id, created_reply.id)?;
            replies_created += 1;
        }
    }

    tracing::info!(
        dry_run,
        folders_created,
        folders_skipped,
        notes_created,
        notes_skipped,
        replies_created,
        replies_skipped,
        "backfill complete"
    );

    Ok(())
}
