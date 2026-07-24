//! OpenSearch client: the secondary, rebuildable search index fed by the
//! transactional outbox. Postgres remains the source of truth — this whole
//! index can be dropped and rebuilt from `notes`/`note_bodies` at any time.
//!
//! A plain `reqwest`-based REST client, not an OpenSearch Rust SDK crate —
//! the API surface used here is tiny (index/delete/search a handful of
//! fields) and a hand-rolled client avoids an extra dependency of uncertain
//! maturity for not much benefit.
//!
//! NOTE: the `text.icu` sub-field (multilingual analysis) described in the
//! architecture plan requires the `analysis-icu` OpenSearch plugin, which
//! isn't installed on a vanilla `opensearchproject/opensearch` image. This
//! first pass uses the default `standard` analyzer only — genuinely
//! multilingual search is a documented follow-up, not silently assumed done.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::models::note::{Note, Visibility};

pub const CONTENT_INDEX: &str = "tack-content";

#[derive(Clone)]
pub struct SearchClient {
    base_url: String,
    http: reqwest::Client,
}

impl SearchClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self { base_url: base_url.into(), http: reqwest::Client::new() }
    }

    /// Creates the content index with its mapping, if it doesn't already exist.
    /// Safe to call on every startup (matches the Postgres migrations'
    /// idempotent-on-every-run convention).
    pub async fn ensure_index(&self) -> Result<()> {
        let url = format!("{}/{CONTENT_INDEX}", self.base_url);
        let exists = self.http.head(&url).send().await?.status().is_success();
        if exists {
            return Ok(());
        }

        let mapping = json!({
            "settings": { "number_of_shards": 1, "number_of_replicas": 0 },
            "mappings": {
                "properties": {
                    "content_id":      { "type": "keyword" },
                    "content_type":    { "type": "keyword" },
                    "source":          { "type": "keyword" },
                    "owning_service":  { "type": "keyword" },
                    "organization_id": { "type": "keyword" },
                    "team_id":         { "type": "keyword" },
                    "visibility":      { "type": "keyword" },
                    "created_by":      { "type": "keyword" },
                    "language":        { "type": "keyword" },
                    "text":            { "type": "text" },
                    "chunk_index":     { "type": "integer" },
                    "embedding_version": { "type": "keyword" },
                    "created_at":      { "type": "date" },
                    "updated_at":      { "type": "date" }
                }
            }
        });

        let resp = self.http.put(&url).json(&mapping).send().await?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("failed to create index {CONTENT_INDEX}: {body}");
        }
        Ok(())
    }

    /// Indexes (or re-indexes) a note as a single whole-document — chunking
    /// long notes is deferred until embeddings/semantic search land.
    pub async fn index_note(&self, note: &Note) -> Result<()> {
        let doc_id = format!("note:{}", note.id);
        let url = format!("{}/{CONTENT_INDEX}/_doc/{doc_id}", self.base_url);
        let doc = json!({
            "content_id": note.id,
            "content_type": "note",
            "source": "body",
            "organization_id": note.organization_id,
            "team_id": note.team_id,
            "visibility": note.visibility.as_db_str(),
            "created_by": note.created_by,
            "language": "en",
            "text": note.body_markdown,
            "created_at": note.created_at.to_rfc3339(),
            "updated_at": note.updated_at.to_rfc3339(),
        });
        let resp = self.http.put(&url).json(&doc).send().await.context("indexing request failed")?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("failed to index {doc_id}: {body}");
        }
        Ok(())
    }

    /// Removes a note from the index (soft-delete in Postgres -> hard removal
    /// from search, since deleted content should never surface in results).
    pub async fn delete_note(&self, note_id: Uuid) -> Result<()> {
        let doc_id = format!("note:{note_id}");
        let url = format!("{}/{CONTENT_INDEX}/_doc/{doc_id}", self.base_url);
        let resp = self.http.delete(&url).send().await.context("delete request failed")?;
        // 404 is fine — deleting something already absent is not an error here.
        if !resp.status().is_success() && resp.status().as_u16() != 404 {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("failed to delete {doc_id}: {body}");
        }
        Ok(())
    }

    /// Hybrid-search placeholder: lexical (BM25) only for now — kNN joins in
    /// once embeddings exist (see the architecture plan's implementation
    /// sequencing). ACL is enforced in the query itself, not just filtered
    /// after the fact: admins get an unfiltered search; everyone else only
    /// ever gets back rows they're allowed to see, computed live from their
    /// current team/organization memberships (`caller`).
    pub async fn search(&self, query_text: &str, caller: &SearchCaller) -> Result<Vec<SearchHit>> {
        let acl_filter = caller.acl_filter();
        let query = if let Some(filter) = acl_filter {
            json!({
                "query": {
                    "bool": {
                        "must": [{ "match": { "text": query_text } }],
                        "filter": [filter]
                    }
                }
            })
        } else {
            json!({ "query": { "match": { "text": query_text } } })
        };

        let url = format!("{}/{CONTENT_INDEX}/_search", self.base_url);
        let resp = self.http.post(&url).json(&query).send().await.context("search request failed")?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("search failed: {body}");
        }
        let body: SearchResponse = resp.json().await.context("failed to parse search response")?;
        Ok(body.hits.hits.into_iter().map(|h| h.into_hit()).collect())
    }
}

/// The caller's identity/membership, used to build the search ACL filter —
/// the OpenSearch-side mirror of `handlers::notes::can_view`, since search
/// results must respect exactly the same visibility rules as direct reads.
pub struct SearchCaller {
    pub user_id: Uuid,
    pub is_admin: bool,
    pub team_ids: Vec<Uuid>,
    pub organization_ids: Vec<Uuid>,
}

impl SearchCaller {
    /// `None` means "no filter" (admin — sees everything).
    fn acl_filter(&self) -> Option<Value> {
        if self.is_admin {
            return None;
        }
        let mut should = vec![json!({ "term": { "created_by": self.user_id } })];
        if !self.team_ids.is_empty() {
            should.push(json!({
                "bool": {
                    "must": [
                        { "term": { "visibility": Visibility::Team.as_db_str() } },
                        { "terms": { "team_id": self.team_ids } }
                    ]
                }
            }));
        }
        if !self.organization_ids.is_empty() {
            should.push(json!({
                "bool": {
                    "must": [
                        { "term": { "visibility": Visibility::Organization.as_db_str() } },
                        { "terms": { "organization_id": self.organization_ids } }
                    ]
                }
            }));
        }
        Some(json!({ "bool": { "should": should, "minimum_should_match": 1 } }))
    }
}

#[derive(Debug, Clone, Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct SearchHit {
    pub content_id: Uuid,
    pub content_type: String,
    pub score: f32,
    pub text: String,
}

#[derive(Deserialize)]
struct SearchResponse {
    hits: SearchHits,
}

#[derive(Deserialize)]
struct SearchHits {
    hits: Vec<RawHit>,
}

#[derive(Deserialize)]
struct RawHit {
    #[serde(rename = "_score")]
    score: Option<f32>,
    #[serde(rename = "_source")]
    source: RawSource,
}

#[derive(Deserialize)]
struct RawSource {
    content_id: Uuid,
    content_type: String,
    text: String,
}

impl RawHit {
    fn into_hit(self) -> SearchHit {
        SearchHit {
            content_id: self.source.content_id,
            content_type: self.source.content_type,
            score: self.score.unwrap_or(0.0),
            text: self.source.text,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caller(is_admin: bool, team_ids: Vec<Uuid>, organization_ids: Vec<Uuid>) -> SearchCaller {
        SearchCaller { user_id: Uuid::new_v4(), is_admin, team_ids, organization_ids }
    }

    #[test]
    fn admin_gets_no_filter() {
        assert!(caller(true, vec![], vec![]).acl_filter().is_none());
    }

    #[test]
    fn non_admin_always_gets_a_filter_even_with_no_teams_or_orgs() {
        // Still must be able to find their own private notes.
        let filter = caller(false, vec![], vec![]).acl_filter().unwrap();
        let should = filter["bool"]["should"].as_array().unwrap();
        assert_eq!(should.len(), 1, "only the created_by clause, no team/org clauses");
    }

    #[test]
    fn non_admin_with_teams_and_orgs_gets_all_three_should_clauses() {
        let filter = caller(false, vec![Uuid::new_v4()], vec![Uuid::new_v4()]).acl_filter().unwrap();
        let should = filter["bool"]["should"].as_array().unwrap();
        assert_eq!(should.len(), 3, "created_by + team clause + organization clause");
    }

    #[test]
    fn team_clause_scopes_to_team_visibility_and_the_callers_team_ids() {
        let team_id = Uuid::new_v4();
        let filter = caller(false, vec![team_id], vec![]).acl_filter().unwrap();
        let should = filter["bool"]["should"].as_array().unwrap();
        let team_clause = &should[1];
        assert_eq!(team_clause["bool"]["must"][0]["term"]["visibility"], "team");
        assert_eq!(team_clause["bool"]["must"][1]["terms"]["team_id"][0], team_id.to_string());
    }
}
