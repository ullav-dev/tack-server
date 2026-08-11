//! One-off backfill: copies awe-server's Idea Boards (`note_folders` rows
//! with `folder_type = 'ideas_board'`, plus `idea_board_stickies`/
//! `board_shapes`/`note_links`) into tack-server, via tack-server's real
//! `POST /idea-boards`/`.../stickies`/`.../shapes`/`.../links` API -- same
//! "go through the real API, not a direct-to-Postgres write" discipline as
//! `backfill-awe-notes`, and the sibling script to it: together the two
//! scripts partition awe-server's `notes` table (`backfill-awe-notes`
//! explicitly excludes any note filed in an `ideas_board` folder -- see its
//! own header comment). See the AWE-apps Notes migration plan, Phase 5.
//!
//! Team resolution, `created_by` UUID parsing, and the general shape of
//! this script all mirror `backfill-awe-notes` -- see that file for the
//! reasoning; not re-derived here.
//!
//! Links are the one genuinely new wrinkle: awe-server's `note_links` has
//! no `board_id` column (tack-server denormalizes one onto every link, see
//! `011_idea_boards.sql`), so it's resolved here from whichever endpoint
//! the link has (a sticky's `board_id`, or a shape's `board_id`). A link
//! whose two endpoints resolve to *different* boards is corrupt data in
//! awe-server (shouldn't be possible via its own API, but nothing enforces
//! it at the DB level there) -- skipped and reported, never guessed at.
//!
//! Run with `--dry-run` first. `--state-file <path>` (default
//! `backfill-awe-idea-boards-state.json`) persists `awe id -> tack id` for
//! every board/sticky-note/shape/link successfully created, the same
//! resume-safe shape as `backfill-awe-notes`'s own state file (a separate
//! file -- the two scripts' id spaces are independent).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use tokio_postgres::NoTls;
use uuid::Uuid;

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

    fn record(&mut self, path: &PathBuf, awe_id: Uuid, tack_id: Uuid) -> Result<()> {
        self.migrated.insert(awe_id, tack_id);
        let data = serde_json::to_string_pretty(self).context("serializing state")?;
        std::fs::write(path, data).with_context(|| format!("writing state file {}", path.display()))
    }
}

struct AweBoard {
    id: Uuid,
    name: String,
    entity_type: Option<String>,
    entity_id: Option<Uuid>,
}

struct AweSticky {
    board_id: Uuid,
    note_id: Uuid,
    x: f64,
    y: f64,
    color: String,
    width: f64,
    height: f64,
    workflow_id: Option<Uuid>,
    // From the joined `notes` row.
    title: String,
    body: String,
    created_by: String,
    created_at: chrono::DateTime<chrono::Utc>,
}

struct AweShape {
    id: Uuid,
    board_id: Uuid,
    shape_type: String,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    fill_color: String,
    stroke_color: String,
    stroke_width: f64,
    label: Option<String>,
    label_color: String,
    label_size: i32,
    image_url: Option<String>,
    created_by: String,
    created_at: chrono::DateTime<chrono::Utc>,
}

struct AweLink {
    id: Uuid,
    from_note_id: Option<Uuid>,
    to_note_id: Option<Uuid>,
    from_shape_id: Option<Uuid>,
    to_shape_id: Option<Uuid>,
    from_port: Option<String>,
    to_port: Option<String>,
    label: Option<String>,
    created_by: String,
    created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Deserialize)]
struct TackCreated {
    id: Uuid,
}

/// `POST /idea-boards/{id}/stickies` addresses a sticky by its `note_id`
/// (see `Sticky`'s doc comment in tack-server), so its "created" response
/// is deserialized the same way as a board/shape/link.
#[derive(Deserialize)]
struct TackSticky {
    note_id: Uuid,
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
        .unwrap_or_else(|| PathBuf::from("backfill-awe-idea-boards-state.json"));
    let awe_database_url = std::env::var("AWE_DATABASE_URL").context("AWE_DATABASE_URL must be set")?;
    let tack_api_url = std::env::var("TACK_API_URL").unwrap_or_else(|_| "http://localhost:8087".into());
    let tack_admin_token = std::env::var("TACK_ADMIN_TOKEN").context("TACK_ADMIN_TOKEN must be set")?;

    let mut state = State::load(&state_path)?;
    tracing::info!(dry_run, %tack_api_url, state_file = %state_path.display(), already_migrated = state.migrated.len(), "starting awe-server -> tack-server idea boards backfill");

    let (awe, connection) = tokio_postgres::connect(&awe_database_url, NoTls).await.context("connecting to awe-server's database")?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            tracing::error!("awe-server db connection error: {e}");
        }
    });

    let http = reqwest::Client::new();

    // ── Team resolution (same shape as backfill-awe-notes) ──────────────

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

    // ── Boards ────────────────────────────────────────────────────────

    let mut awe_boards = Vec::new();
    for row in awe
        .query("SELECT id, name, entity_type, entity_id FROM note_folders WHERE folder_type = 'ideas_board'", &[])
        .await?
    {
        awe_boards.push(AweBoard { id: row.get(0), name: row.get(1), entity_type: row.get(2), entity_id: row.get(3) });
    }

    let mut awe_stickies = Vec::new();
    for row in awe
        .query(
            "SELECT s.board_id, s.note_id, s.x, s.y, s.color, s.width, s.height, s.workflow_id,
                    n.title, n.body, n.created_by, n.created_at
             FROM idea_board_stickies s
             JOIN notes n ON n.id = s.note_id
             ORDER BY n.created_at",
            &[],
        )
        .await?
    {
        awe_stickies.push(AweSticky {
            board_id: row.get(0),
            note_id: row.get(1),
            x: row.get(2),
            y: row.get(3),
            color: row.get(4),
            width: row.get(5),
            height: row.get(6),
            workflow_id: row.get(7),
            title: row.get(8),
            body: row.get(9),
            created_by: row.get(10),
            created_at: row.get(11),
        });
    }

    let mut awe_shapes = Vec::new();
    for row in awe
        .query(
            "SELECT id, board_id, shape_type, x, y, width, height, fill_color, stroke_color, stroke_width,
                    label, label_color, label_size, image_url, created_by, created_at
             FROM board_shapes ORDER BY created_at",
            &[],
        )
        .await?
    {
        awe_shapes.push(AweShape {
            id: row.get(0),
            board_id: row.get(1),
            shape_type: row.get(2),
            x: row.get(3),
            y: row.get(4),
            width: row.get(5),
            height: row.get(6),
            fill_color: row.get(7),
            stroke_color: row.get(8),
            stroke_width: row.get(9),
            label: row.get(10),
            label_color: row.get(11),
            label_size: row.get(12),
            image_url: row.get(13),
            created_by: row.get(14),
            created_at: row.get(15),
        });
    }

    let mut awe_links = Vec::new();
    for row in awe
        .query(
            "SELECT id, from_note_id, to_note_id, from_shape_id, to_shape_id, from_port, to_port, label, created_by, created_at
             FROM note_links ORDER BY created_at",
            &[],
        )
        .await?
    {
        awe_links.push(AweLink {
            id: row.get(0),
            from_note_id: row.get(1),
            to_note_id: row.get(2),
            from_shape_id: row.get(3),
            to_shape_id: row.get(4),
            from_port: row.get(5),
            to_port: row.get(6),
            label: row.get(7),
            created_by: row.get(8),
            created_at: row.get(9),
        });
    }

    tracing::info!(
        boards = awe_boards.len(),
        stickies = awe_stickies.len(),
        shapes = awe_shapes.len(),
        links = awe_links.len(),
        "loaded from awe-server"
    );

    // ── Boards: write first (everything else references one) ────────────

    let mut board_id_map: HashMap<Uuid, Uuid> = state.migrated.clone();
    let (mut boards_created, mut boards_skipped) = (0u32, 0u32);
    for b in &awe_boards {
        if state.migrated.contains_key(&b.id) {
            tracing::info!(board_id = %b.id, name = %b.name, "board already migrated, skipping (per state file)");
            continue;
        }
        let team_id = match (&b.entity_type, b.entity_id) {
            (Some(et), Some(eid)) => resolve_team(et, eid),
            _ => None,
        };
        let Some(team_id) = team_id else {
            tracing::warn!(board_id = %b.id, name = %b.name, "skipping board: could not resolve a team (no entity scope, or entity/team unresolvable)");
            boards_skipped += 1;
            continue;
        };
        if dry_run {
            tracing::info!(board_id = %b.id, name = %b.name, %team_id, "[dry-run] would create board");
            board_id_map.insert(b.id, b.id); // see note_id_map's dry-run comment below
            boards_created += 1;
            continue;
        }
        let mut payload = json!({ "team_id": team_id, "name": b.name });
        if let (Some(et), Some(eid)) = (&b.entity_type, b.entity_id) {
            payload["attach"] = json!({ "owning_service": "awe", "entity_type": et, "entity_id": eid.to_string() });
        }
        let resp = http
            .post(format!("{tack_api_url}/idea-boards"))
            .bearer_auth(&tack_admin_token)
            .json(&payload)
            .send()
            .await
            .with_context(|| format!("creating board {}", b.id))?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            tracing::error!(board_id = %b.id, name = %b.name, body, "failed to create board");
            boards_skipped += 1;
            continue;
        }
        let created: TackCreated = resp.json().await.context("parsing created board")?;
        board_id_map.insert(b.id, created.id);
        state.record(&state_path, b.id, created.id)?;
        boards_created += 1;
    }

    // ── Stickies ─────────────────────────────────────────────────────

    let mut note_id_map: HashMap<Uuid, Uuid> = HashMap::new();
    let (mut stickies_created, mut stickies_skipped) = (0u32, 0u32);
    for s in &awe_stickies {
        let Some(&tack_board_id) = board_id_map.get(&s.board_id) else {
            tracing::warn!(note_id = %s.note_id, board_id = %s.board_id, "skipping sticky: its board was not migrated");
            stickies_skipped += 1;
            continue;
        };
        if let Some(&existing) = state.migrated.get(&s.note_id) {
            note_id_map.insert(s.note_id, existing);
            tracing::info!(note_id = %s.note_id, "sticky already migrated, skipping (per state file)");
            continue;
        }
        let Ok(created_by) = s.created_by.parse::<Uuid>() else {
            tracing::warn!(note_id = %s.note_id, created_by = %s.created_by, "skipping sticky: created_by is not a UUID");
            stickies_skipped += 1;
            continue;
        };
        if dry_run {
            tracing::info!(note_id = %s.note_id, %tack_board_id, title = %s.title, "[dry-run] would create sticky");
            // Fakes an identity mapping (awe id stands in for the not-yet-
            // created tack id) purely so the link dry-run pass below can
            // trace through and report accurately instead of every link
            // showing as "endpoint not migrated."
            note_id_map.insert(s.note_id, s.note_id);
            stickies_created += 1;
            continue;
        }
        let resp = http
            .post(format!("{tack_api_url}/idea-boards/{tack_board_id}/stickies"))
            .bearer_auth(&tack_admin_token)
            .json(&json!({
                "title": s.title,
                "body_markdown": s.body,
                "x": s.x, "y": s.y, "color": s.color, "width": s.width, "height": s.height,
                "linked_entity_type": s.workflow_id.map(|_| "workflow"),
                "linked_entity_id": s.workflow_id.map(|id| id.to_string()),
                "created_at": s.created_at,
                "created_by": created_by,
            }))
            .send()
            .await
            .with_context(|| format!("creating sticky {}", s.note_id))?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            tracing::error!(note_id = %s.note_id, body, "failed to create sticky");
            stickies_skipped += 1;
            continue;
        }
        let created: TackSticky = resp.json().await.context("parsing created sticky")?;
        note_id_map.insert(s.note_id, created.note_id);
        state.record(&state_path, s.note_id, created.note_id)?;
        stickies_created += 1;
    }

    // ── Shapes ───────────────────────────────────────────────────────

    let mut shape_id_map: HashMap<Uuid, Uuid> = HashMap::new();
    let (mut shapes_created, mut shapes_skipped) = (0u32, 0u32);
    for sh in &awe_shapes {
        let Some(&tack_board_id) = board_id_map.get(&sh.board_id) else {
            tracing::warn!(shape_id = %sh.id, board_id = %sh.board_id, "skipping shape: its board was not migrated");
            shapes_skipped += 1;
            continue;
        };
        if let Some(&existing) = state.migrated.get(&sh.id) {
            shape_id_map.insert(sh.id, existing);
            tracing::info!(shape_id = %sh.id, "shape already migrated, skipping (per state file)");
            continue;
        }
        let Ok(created_by) = sh.created_by.parse::<Uuid>() else {
            tracing::warn!(shape_id = %sh.id, created_by = %sh.created_by, "skipping shape: created_by is not a UUID");
            shapes_skipped += 1;
            continue;
        };
        if dry_run {
            tracing::info!(shape_id = %sh.id, %tack_board_id, shape_type = %sh.shape_type, "[dry-run] would create shape");
            shape_id_map.insert(sh.id, sh.id); // see note_id_map's dry-run comment above
            shapes_created += 1;
            continue;
        }
        let resp = http
            .post(format!("{tack_api_url}/idea-boards/{tack_board_id}/shapes"))
            .bearer_auth(&tack_admin_token)
            .json(&json!({
                "shape_type": sh.shape_type, "x": sh.x, "y": sh.y, "width": sh.width, "height": sh.height,
                "fill_color": sh.fill_color, "stroke_color": sh.stroke_color, "stroke_width": sh.stroke_width,
                "label": sh.label, "label_color": sh.label_color, "label_size": sh.label_size, "image_url": sh.image_url,
                "created_at": sh.created_at,
                "created_by": created_by,
            }))
            .send()
            .await
            .with_context(|| format!("creating shape {}", sh.id))?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            tracing::error!(shape_id = %sh.id, body, "failed to create shape");
            shapes_skipped += 1;
            continue;
        }
        let created: TackCreated = resp.json().await.context("parsing created shape")?;
        shape_id_map.insert(sh.id, created.id);
        state.record(&state_path, sh.id, created.id)?;
        shapes_created += 1;
    }

    // ── Links ────────────────────────────────────────────────────────

    let (mut links_created, mut links_skipped) = (0u32, 0u32);
    for l in &awe_links {
        if state.migrated.contains_key(&l.id) {
            tracing::info!(link_id = %l.id, "link already migrated, skipping (per state file)");
            continue;
        }

        // Resolve each endpoint to its tack id, and separately to the awe
        // board it belongs to (via the sticky/shape maps loaded above) --
        // needed to catch a from/to pair spanning two different boards
        // before ever calling the API (tack-server's own FK/CHECK
        // constraints would only catch a *malformed* link, not one that's
        // well-formed but crosses boards, since `board_id` there comes from
        // the URL, not derived from the endpoints).
        let from_note_tack = l.from_note_id.and_then(|id| note_id_map.get(&id).copied());
        let to_note_tack = l.to_note_id.and_then(|id| note_id_map.get(&id).copied());
        let from_shape_tack = l.from_shape_id.and_then(|id| shape_id_map.get(&id).copied());
        let to_shape_tack = l.to_shape_id.and_then(|id| shape_id_map.get(&id).copied());

        let from_board = l
            .from_note_id
            .and_then(|id| awe_stickies.iter().find(|s| s.note_id == id).map(|s| s.board_id))
            .or_else(|| l.from_shape_id.and_then(|id| awe_shapes.iter().find(|s| s.id == id).map(|s| s.board_id)));
        let to_board = l
            .to_note_id
            .and_then(|id| awe_stickies.iter().find(|s| s.note_id == id).map(|s| s.board_id))
            .or_else(|| l.to_shape_id.and_then(|id| awe_shapes.iter().find(|s| s.id == id).map(|s| s.board_id)));

        let (Some(from_board), Some(to_board)) = (from_board, to_board) else {
            tracing::warn!(link_id = %l.id, "skipping link: could not resolve one or both endpoints' board");
            links_skipped += 1;
            continue;
        };
        if from_board != to_board {
            tracing::warn!(link_id = %l.id, %from_board, %to_board, "skipping link: endpoints belong to different boards (corrupt in awe-server)");
            links_skipped += 1;
            continue;
        }
        let Some(&tack_board_id) = board_id_map.get(&from_board) else {
            tracing::warn!(link_id = %l.id, board_id = %from_board, "skipping link: its board was not migrated");
            links_skipped += 1;
            continue;
        };

        let from_ok = from_note_tack.is_some() || from_shape_tack.is_some();
        let to_ok = to_note_tack.is_some() || to_shape_tack.is_some();
        if !from_ok || !to_ok {
            tracing::warn!(link_id = %l.id, "skipping link: one or both endpoints were not migrated (their sticky/shape was skipped above)");
            links_skipped += 1;
            continue;
        }

        let Ok(created_by) = l.created_by.parse::<Uuid>() else {
            tracing::warn!(link_id = %l.id, created_by = %l.created_by, "skipping link: created_by is not a UUID");
            links_skipped += 1;
            continue;
        };

        if dry_run {
            tracing::info!(link_id = %l.id, %tack_board_id, "[dry-run] would create link");
            links_created += 1;
            continue;
        }

        let resp = http
            .post(format!("{tack_api_url}/idea-boards/{tack_board_id}/links"))
            .bearer_auth(&tack_admin_token)
            .json(&json!({
                "from_note_id": from_note_tack, "from_shape_id": from_shape_tack,
                "to_note_id": to_note_tack, "to_shape_id": to_shape_tack,
                "from_port": l.from_port, "to_port": l.to_port, "label": l.label,
                "created_at": l.created_at,
                "created_by": created_by,
            }))
            .send()
            .await
            .with_context(|| format!("creating link {}", l.id))?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            tracing::error!(link_id = %l.id, body, "failed to create link");
            links_skipped += 1;
            continue;
        }
        let created: TackCreated = resp.json().await.context("parsing created link")?;
        state.record(&state_path, l.id, created.id)?;
        links_created += 1;
    }

    tracing::info!(
        dry_run,
        boards_created, boards_skipped,
        stickies_created, stickies_skipped,
        shapes_created, shapes_skipped,
        links_created, links_skipped,
        "backfill complete"
    );

    Ok(())
}
