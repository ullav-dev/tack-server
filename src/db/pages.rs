use deadpool_postgres::Pool;
use serde_json::json;
use tokio_postgres::Row;
use uuid::Uuid;

use crate::error::AppError;
use crate::models::page::{Page, PagePermission, PageRevision, PermissionLevel, PrincipalType};

/// UUID encoded as a 32-char lowercase hex string with no hyphens — the only
/// UUID form that's a valid ltree label. Same trick as notes::ltree_label.
fn ltree_label(id: Uuid) -> String {
    id.simple().to_string()
}

fn row_to_page(row: &Row) -> Page {
    Page {
        id: row.get("id"),
        organization_id: row.get("organization_id"),
        space_id: row.get("space_id"),
        parent_id: row.get("parent_id"),
        path: row.get("path"),
        title: row.get("title"),
        is_template: row.get("is_template"),
        content_markdown: row.get("content_markdown"),
        created_by: row.get("created_by"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        child_count: row.get("child_count"),
    }
}

const PAGE_SELECT: &str = "
    SELECT p.id, p.organization_id, p.space_id, p.parent_id, p.path::text AS path,
           p.title, p.is_template, p.created_by, p.created_at, p.updated_at,
           d.content_markdown,
           (SELECT COUNT(*) FROM pages c
            WHERE c.parent_id = p.id AND c.organization_id = p.organization_id AND c.deleted_at IS NULL
           ) AS child_count
    FROM pages p
    JOIN page_docs d ON d.page_id = p.id AND d.organization_id = p.organization_id
";

pub struct NewPage {
    pub organization_id: Uuid,
    pub space_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub title: String,
    pub content_markdown: String,
    pub created_by: Uuid,
}

/// Creates a page (root, if `parent_id` is `None`, otherwise a child of an
/// existing page already verified to be in the same space) plus its content
/// row and an outbox event, all in one transaction.
pub async fn create_page(pool: &Pool, new: NewPage) -> Result<Page, AppError> {
    let mut client = pool.get().await?;
    let tx = client.transaction().await?;

    let id = Uuid::new_v4();
    let path = match new.parent_id {
        None => ltree_label(id),
        Some(parent_id) => {
            let parent_row = tx
                .query_one(
                    "SELECT path::text FROM pages WHERE id = $1 AND organization_id = $2",
                    &[&parent_id, &new.organization_id],
                )
                .await?;
            let parent_path: String = parent_row.get(0);
            format!("{parent_path}.{}", ltree_label(id))
        }
    };

    tx.execute(
        "INSERT INTO pages (id, organization_id, space_id, parent_id, path, title, created_by)
         VALUES ($1, $2, $3, $4, $5::ltree, $6, $7)",
        &[&id, &new.organization_id, &new.space_id, &new.parent_id, &path, &new.title, &new.created_by],
    )
    .await?;

    tx.execute(
        "INSERT INTO page_docs (page_id, organization_id, content_markdown) VALUES ($1, $2, $3)",
        &[&id, &new.organization_id, &new.content_markdown],
    )
    .await?;

    enqueue_outbox_event(&tx, new.organization_id, id, "created").await?;

    tx.commit().await?;

    get_page(pool, id, new.organization_id).await?.ok_or_else(|| {
        AppError::Internal(anyhow::anyhow!("page {id} vanished immediately after insert"))
    })
}

pub async fn get_page(pool: &Pool, id: Uuid, organization_id: Uuid) -> Result<Option<Page>, AppError> {
    let client = pool.get().await?;
    let sql = format!("{PAGE_SELECT} WHERE p.id = $1 AND p.organization_id = $2 AND p.deleted_at IS NULL");
    let row = client.query_opt(&sql, &[&id, &organization_id]).await?;
    Ok(row.as_ref().map(row_to_page))
}

/// Admin-only fallback, mirroring `notes::get_note_admin_any_org`.
pub async fn get_page_admin_any_org(pool: &Pool, id: Uuid) -> Result<Option<Page>, AppError> {
    let client = pool.get().await?;
    let sql = format!("{PAGE_SELECT} WHERE p.id = $1 AND p.deleted_at IS NULL");
    let row = client.query_opt(&sql, &[&id]).await?;
    Ok(row.as_ref().map(row_to_page))
}

/// Direct children of `parent_id` in a space, or root pages if `parent_id`
/// is `None`. Permission filtering happens in the handler (each candidate
/// page's effective permission must be resolved individually — see
/// `pages_acl::resolve_effective_permission`), not here.
pub async fn list_children(
    pool: &Pool,
    organization_id: Uuid,
    space_id: Uuid,
    parent_id: Option<Uuid>,
) -> Result<Vec<Page>, AppError> {
    let client = pool.get().await?;
    let sql = match parent_id {
        Some(_) => format!(
            "{PAGE_SELECT} WHERE p.organization_id = $1 AND p.space_id = $2 AND p.parent_id = $3 AND p.deleted_at IS NULL
             ORDER BY p.title ASC"
        ),
        None => format!(
            "{PAGE_SELECT} WHERE p.organization_id = $1 AND p.space_id = $2 AND p.parent_id IS NULL AND p.deleted_at IS NULL
             ORDER BY p.title ASC"
        ),
    };
    let rows = match parent_id {
        Some(pid) => client.query(&sql, &[&organization_id, &space_id, &pid]).await?,
        None => client.query(&sql, &[&organization_id, &space_id]).await?,
    };
    Ok(rows.iter().map(row_to_page).collect())
}

pub async fn update_page(
    pool: &Pool,
    page: &Page,
    title: Option<&str>,
    content_markdown: Option<&str>,
) -> Result<Page, AppError> {
    let mut client = pool.get().await?;
    let tx = client.transaction().await?;

    if let Some(t) = title {
        tx.execute(
            "UPDATE pages SET title = $1, updated_at = NOW() WHERE id = $2 AND organization_id = $3",
            &[&t, &page.id, &page.organization_id],
        )
        .await?;
    }

    if let Some(body) = content_markdown {
        tx.execute(
            "UPDATE page_docs SET content_markdown = $1, updated_at = NOW() WHERE page_id = $2 AND organization_id = $3",
            &[&body, &page.id, &page.organization_id],
        )
        .await?;
        if title.is_none() {
            tx.execute(
                "UPDATE pages SET updated_at = NOW() WHERE id = $1 AND organization_id = $2",
                &[&page.id, &page.organization_id],
            )
            .await?;
        }
    }

    if title.is_some() || content_markdown.is_some() {
        enqueue_outbox_event(&tx, page.organization_id, page.id, "updated").await?;
    }

    tx.commit().await?;

    get_page(pool, page.id, page.organization_id).await?.ok_or_else(|| {
        AppError::Internal(anyhow::anyhow!("page {} vanished immediately after update", page.id))
    })
}

/// Soft-deletes a single page. Does not cascade the soft-delete to
/// descendants (that's a deliberate, documented simplification for this
/// first pass — hard deletion of the page or its space still cascades via
/// the FK, so data integrity isn't at risk, just the "delete this whole
/// subtree" UX, which is deferred).
pub async fn soft_delete_page(pool: &Pool, page: &Page) -> Result<(), AppError> {
    let mut client = pool.get().await?;
    let tx = client.transaction().await?;
    tx.execute(
        "UPDATE pages SET deleted_at = NOW() WHERE id = $1 AND organization_id = $2",
        &[&page.id, &page.organization_id],
    )
    .await?;
    enqueue_outbox_event(&tx, page.organization_id, page.id, "deleted").await?;
    tx.commit().await?;
    Ok(())
}

/// Snapshots a page's *current* `content_markdown` as a new named version —
/// a deliberate action by an authorized user (caller must already be
/// authorized; enforced by the handler), not something that happens
/// implicitly on every save. Mirrors `db::notes::create_revision` exactly,
/// except there's no "first revision at creation time" baseline the way
/// Notes gets one automatically -- a page has zero revisions until someone
/// explicitly saves one.
pub async fn create_page_revision(pool: &Pool, page: &Page, edited_by: Uuid) -> Result<PageRevision, AppError> {
    let mut client = pool.get().await?;
    let tx = client.transaction().await?;

    let content_row = tx
        .query_one(
            "SELECT content_markdown FROM page_docs WHERE page_id = $1 AND organization_id = $2",
            &[&page.id, &page.organization_id],
        )
        .await?;
    let content_markdown: String = content_row.get(0);

    let version_row = tx
        .query_one(
            "SELECT COALESCE(MAX(version), 0) + 1 FROM page_revisions
             WHERE page_id = $1 AND organization_id = $2",
            &[&page.id, &page.organization_id],
        )
        .await?;
    let next_version: i32 = version_row.get(0);

    let row = tx
        .query_one(
            "INSERT INTO page_revisions (organization_id, page_id, version, content_markdown, edited_by)
             VALUES ($1, $2, $3, $4, $5)
             RETURNING id, page_id, version, content_markdown, edited_by, edited_at",
            &[&page.organization_id, &page.id, &next_version, &content_markdown, &edited_by],
        )
        .await?;

    tx.commit().await?;

    Ok(PageRevision {
        id: row.get("id"),
        page_id: row.get("page_id"),
        version: row.get("version"),
        content_markdown: row.get("content_markdown"),
        edited_by: row.get("edited_by"),
        edited_at: row.get("edited_at"),
    })
}

/// Deletes a single saved page version. Caller must already be authorized
/// (creator or admin) — enforced by the handler. Unlike
/// `db::notes::delete_revision`, there's no "last remaining version" guard
/// needed for the same reason there's no reply-reassignment step: Pages
/// have no automatic baseline revision at creation time, so a page can
/// validly have zero revisions (before anyone ever saves one) -- deleting
/// down to zero is not a special case to guard against.
pub async fn delete_page_revision(pool: &Pool, page: &Page, revision_id: Uuid) -> Result<(), AppError> {
    let client = pool.get().await?;
    let deleted = client
        .execute(
            "DELETE FROM page_revisions WHERE id = $1 AND page_id = $2 AND organization_id = $3",
            &[&revision_id, &page.id, &page.organization_id],
        )
        .await?;
    if deleted == 0 {
        return Err(AppError::NotFound("Version not found.".into()));
    }
    Ok(())
}

pub async fn list_page_revisions(pool: &Pool, page_id: Uuid, organization_id: Uuid) -> Result<Vec<PageRevision>, AppError> {
    let client = pool.get().await?;
    let rows = client
        .query(
            "SELECT id, page_id, version, content_markdown, edited_by, edited_at
             FROM page_revisions WHERE page_id = $1 AND organization_id = $2
             ORDER BY version DESC",
            &[&page_id, &organization_id],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|r| PageRevision {
            id: r.get("id"),
            page_id: r.get("page_id"),
            version: r.get("version"),
            content_markdown: r.get("content_markdown"),
            edited_by: r.get("edited_by"),
            edited_at: r.get("edited_at"),
        })
        .collect())
}

async fn enqueue_outbox_event(
    tx: &deadpool_postgres::Transaction<'_>,
    organization_id: Uuid,
    content_id: Uuid,
    event_type: &str,
) -> Result<(), AppError> {
    tx.execute(
        "INSERT INTO outbox_events (organization_id, content_type, content_id, event_type, payload)
         VALUES ($1, 'page', $2, $3, $4)",
        &[&organization_id, &content_id, &event_type, &json!({})],
    )
    .await?;
    Ok(())
}

/// Rows of the nearest ancestor-or-self of `page` (by path) that has *any*
/// explicit permission rows, or an empty vec if no ancestor in the chain has
/// any — meaning "fall back to space membership" (see `pages_acl.rs`).
/// Never merges rows from more than one page: the walk stops at the first
/// (most specific) page with any row and returns exactly that page's rows.
pub async fn nearest_permission_rows(
    pool: &Pool,
    organization_id: Uuid,
    page_path: &str,
) -> Result<Vec<PagePermission>, AppError> {
    let client = pool.get().await?;
    let rows = client
        .query(
            "WITH nearest AS (
                SELECT anc.id
                FROM pages anc
                WHERE anc.organization_id = $1
                  AND anc.path @> $2::ltree
                  AND EXISTS (
                    SELECT 1 FROM page_permissions pp
                    WHERE pp.page_id = anc.id AND pp.organization_id = anc.organization_id
                  )
                ORDER BY nlevel(anc.path) DESC
                LIMIT 1
             )
             SELECT pp.id, pp.page_id, pp.principal_type, pp.principal_id, pp.level, pp.created_at
             FROM nearest
             JOIN page_permissions pp ON pp.page_id = nearest.id AND pp.organization_id = $1",
            &[&organization_id, &page_path],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|r| PagePermission {
            id: r.get("id"),
            page_id: r.get("page_id"),
            principal_type: PrincipalType::from_db_str(r.get("principal_type")),
            principal_id: r.get("principal_id"),
            level: PermissionLevel::from_db_str(r.get("level")),
            created_at: r.get("created_at"),
        })
        .collect())
}

/// A page's own explicit permission rows (not inherited from an ancestor) —
/// used by the "manage this page's permissions" endpoints, as distinct from
/// `nearest_permission_rows`, which is the read-time resolution query.
pub async fn list_own_permissions(
    pool: &Pool,
    page_id: Uuid,
    organization_id: Uuid,
) -> Result<Vec<PagePermission>, AppError> {
    let client = pool.get().await?;
    let rows = client
        .query(
            "SELECT id, page_id, principal_type, principal_id, level, created_at
             FROM page_permissions WHERE page_id = $1 AND organization_id = $2
             ORDER BY created_at ASC",
            &[&page_id, &organization_id],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|r| PagePermission {
            id: r.get("id"),
            page_id: r.get("page_id"),
            principal_type: PrincipalType::from_db_str(r.get("principal_type")),
            principal_id: r.get("principal_id"),
            level: PermissionLevel::from_db_str(r.get("level")),
            created_at: r.get("created_at"),
        })
        .collect())
}

pub async fn add_permission(
    pool: &Pool,
    organization_id: Uuid,
    page_id: Uuid,
    principal_type: PrincipalType,
    principal_id: Option<Uuid>,
    level: PermissionLevel,
) -> Result<PagePermission, AppError> {
    let client = pool.get().await?;
    let row = client
        .query_one(
            "INSERT INTO page_permissions (organization_id, page_id, principal_type, principal_id, level)
             VALUES ($1, $2, $3, $4, $5)
             RETURNING id, page_id, principal_type, principal_id, level, created_at",
            &[&organization_id, &page_id, &principal_type.as_db_str(), &principal_id, &level.as_db_str()],
        )
        .await?;
    Ok(PagePermission {
        id: row.get("id"),
        page_id: row.get("page_id"),
        principal_type: PrincipalType::from_db_str(row.get("principal_type")),
        principal_id: row.get("principal_id"),
        level: PermissionLevel::from_db_str(row.get("level")),
        created_at: row.get("created_at"),
    })
}

pub async fn delete_permission(
    pool: &Pool,
    permission_id: Uuid,
    organization_id: Uuid,
) -> Result<(), AppError> {
    let client = pool.get().await?;
    client
        .execute(
            "DELETE FROM page_permissions WHERE id = $1 AND organization_id = $2",
            &[&permission_id, &organization_id],
        )
        .await?;
    Ok(())
}
