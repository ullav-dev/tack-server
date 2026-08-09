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
//! first pass uses the default `standard` analyzer only for lexical
//! matching — multilingual coverage instead comes from the embedding model
//! (`multilingual-e5-small`), which is genuinely multilingual by training,
//! so hybrid search isn't purely English-only even without the ICU plugin.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use uuid::Uuid;

use crate::embeddings::{chunk_text, Embedder, EMBEDDING_DIMENSION, EMBEDDING_VERSION};
use crate::models::note::{Note, Visibility};
use crate::models::page::Page;

pub const CONTENT_INDEX: &str = "tack-content";

/// Reciprocal Rank Fusion constant — the standard default (60) from the
/// original RRF paper; not sensitive to tuning for a first pass.
const RRF_K: f64 = 60.0;

/// Raw candidates fetched per sub-query (lexical, and separately kNN)
/// before fusion/dedup/pagination. Comfortably covers several pages'
/// worth of *deduped* results (chunks of the same note collapse to one
/// hit), which is the actual depth anyone pages through in a search UI --
/// not an attempt at exhaustive pagination over every matching document,
/// which no search engine does either.
const RAW_FETCH_SIZE: i64 = 200;

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
    ///
    /// Only applies to a *new* index -- an already-existing one (e.g. any
    /// pre-existing dev/prod deployment) keeps whatever mapping it already
    /// has, since this never issues a `PUT _mapping` against an existing
    /// index. That's not a functional gap for the newer fields added here
    /// (`title`/`folder_id`/`space_id`/`parent_id`): the index has no
    /// `"dynamic": false`, so OpenSearch's default dynamic mapping picks
    /// them up automatically the first time a document carrying them is
    /// indexed, just with auto-inferred types (`text` + a `.keyword`
    /// sub-field) rather than this file's explicit ones -- fine here since
    /// nothing filters/aggregates on them, only displays them. `title`
    /// showing up on *already-indexed* content still needs the backfill
    /// (`tack-indexer --backfill`) -- a new mapping field doesn't retroactively
    /// populate old documents.
    pub async fn ensure_index(&self) -> Result<()> {
        let url = format!("{}/{CONTENT_INDEX}", self.base_url);
        let exists = self.http.head(&url).send().await?.status().is_success();
        if exists {
            return Ok(());
        }

        let mapping = json!({
            "settings": {
                "number_of_shards": 1,
                "number_of_replicas": 0,
                "index.knn": true
            },
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
                    "title":           { "type": "text" },
                    "text":            { "type": "text" },
                    "chunk_index":     { "type": "integer" },
                    "folder_id":       { "type": "keyword" },
                    "space_id":        { "type": "keyword" },
                    "parent_id":       { "type": "keyword" },
                    "embedding": {
                        "type": "knn_vector",
                        "dimension": EMBEDDING_DIMENSION,
                        "method": { "name": "hnsw", "space_type": "cosinesimil", "engine": "lucene" }
                    },
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

    /// Indexes (or re-indexes) a note. Long notes are split into overlapping
    /// chunks (`embeddings::chunk_text`), each indexed as its own document
    /// sharing the note's metadata — `delete_note` removes all of a note's
    /// chunk documents together. Without an embedder (e.g. OpenSearch/model
    /// unavailable at startup), chunks are still indexed for lexical search,
    /// just without an `embedding` field.
    pub async fn index_note(&self, note: &Note, embedder: Option<&Embedder>) -> Result<()> {
        let chunks = chunk_text(&note.body_markdown);
        let embeddings: Vec<Option<Vec<f32>>> = if let Some(embedder) = embedder {
            let texts: Vec<String> = chunks.iter().map(|(_, t)| t.clone()).collect();
            match embedder.embed_passages(texts).await {
                Ok(vectors) => vectors.into_iter().map(Some).collect(),
                Err(e) => {
                    tracing::warn!("embedding failed for note {}, indexing lexical-only: {e:#}", note.id);
                    vec![None; chunks.len()]
                }
            }
        } else {
            vec![None; chunks.len()]
        };

        for ((chunk_index, chunk_text), embedding) in chunks.into_iter().zip(embeddings) {
            let doc_id = format!("note:{}:{chunk_index}", note.id);
            let url = format!("{}/{CONTENT_INDEX}/_doc/{doc_id}", self.base_url);
            let mut doc = json!({
                "content_id": note.id,
                "content_type": "note",
                "source": "body",
                "organization_id": note.organization_id,
                "team_id": note.team_id,
                "visibility": note.visibility.as_db_str(),
                "created_by": note.created_by,
                "language": "en",
                // Empty for a reply -- only top-level notes have a title
                // (see `Note::title`'s own doc comment). A reply hit still
                // shows in results via `text`/highlighting; the frontend
                // resolves it to its parent thread's title via `parent_id`.
                "title": note.title,
                "text": chunk_text,
                "chunk_index": chunk_index,
                // `folder_id` is only ever set on a top-level note (server-
                // enforced by a CHECK constraint, see 008_note_folders.sql),
                // so a reply's is always null here too -- matches its own
                // `folder_id` field exactly.
                "folder_id": note.folder_id,
                // Null for a top-level note, the parent's id for a reply --
                // this is what lets a reply hit link to its actual thread
                // instead of opening the reply as if it were standalone.
                "parent_id": note.parent_id,
                "created_at": note.created_at.to_rfc3339(),
                "updated_at": note.updated_at.to_rfc3339(),
            });
            if let Some(vector) = embedding {
                doc["embedding"] = json!(vector);
                doc["embedding_version"] = json!(EMBEDDING_VERSION);
            }
            let resp = self.http.put(&url).json(&doc).send().await.context("indexing request failed")?;
            if !resp.status().is_success() {
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("failed to index {doc_id}: {body}");
            }
        }
        Ok(())
    }

    /// Removes all of a note's chunk documents (soft-delete in Postgres ->
    /// hard removal from search, since deleted content should never surface
    /// in results).
    ///
    /// `?conflicts=proceed` -- found live while running the backfill tool
    /// against real data: `_delete_by_query`'s default (`conflicts=abort`)
    /// aborts the *entire* operation the moment any one matched document's
    /// seqNo has moved since the query's internal search phase (e.g. a
    /// concurrent reindex of the same content elsewhere), surfacing as a 409
    /// even though the delete itself is small and idempotent to just retry.
    /// `proceed` skips conflicting documents instead of aborting -- correct
    /// here because every caller of `delete_note`/`delete_page` already
    /// treats "nothing left to delete" as success, and a genuinely-left-over
    /// chunk from a lost race gets cleaned up by that content's own next
    /// index/delete event (or the next `--backfill` run) regardless.
    pub async fn delete_note(&self, note_id: Uuid) -> Result<()> {
        let url = format!("{}/{CONTENT_INDEX}/_delete_by_query?conflicts=proceed", self.base_url);
        let query = json!({
            "query": { "bool": { "must": [
                { "term": { "content_type": "note" } },
                { "term": { "content_id": note_id } }
            ]}}
        });
        let resp = self.http.post(&url).json(&query).send().await.context("delete request failed")?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("failed to delete note {note_id}: {body}");
        }
        Ok(())
    }

    /// Indexes (or re-indexes) a page's `content_markdown`, chunked the same
    /// way as a note's body. Unlike Notes, Page ACL can't be reduced to a
    /// static field the OpenSearch query itself can filter on (permission is
    /// resolved live from the page/space tree, see `pages_acl.rs`) — the
    /// `acl_filter` below only applies a coarse "belongs to one of the
    /// caller's organizations" pre-filter; the real per-page visibility
    /// check happens as a live post-filter in `handlers::search::search`,
    /// the same live-recheck idiom already used by `handlers::pages::search_pages`.
    pub async fn index_page(&self, page: &Page, embedder: Option<&Embedder>) -> Result<()> {
        let chunks = chunk_text(&page.content_markdown);
        let embeddings: Vec<Option<Vec<f32>>> = if let Some(embedder) = embedder {
            let texts: Vec<String> = chunks.iter().map(|(_, t)| t.clone()).collect();
            match embedder.embed_passages(texts).await {
                Ok(vectors) => vectors.into_iter().map(Some).collect(),
                Err(e) => {
                    tracing::warn!("embedding failed for page {}, indexing lexical-only: {e:#}", page.id);
                    vec![None; chunks.len()]
                }
            }
        } else {
            vec![None; chunks.len()]
        };

        for ((chunk_index, chunk_text), embedding) in chunks.into_iter().zip(embeddings) {
            let doc_id = format!("page:{}:{chunk_index}", page.id);
            let url = format!("{}/{CONTENT_INDEX}/_doc/{doc_id}", self.base_url);
            let mut doc = json!({
                "content_id": page.id,
                "content_type": "page",
                "source": "body",
                "organization_id": page.organization_id,
                "created_by": page.created_by,
                "language": "en",
                "title": page.title,
                "text": chunk_text,
                "chunk_index": chunk_index,
                // The one field the frontend was missing to build a page
                // hit's own `/spaces/:spaceId/pages/:pageId` link at all.
                "space_id": page.space_id,
                "created_at": page.created_at.to_rfc3339(),
                "updated_at": page.updated_at.to_rfc3339(),
            });
            if let Some(vector) = embedding {
                doc["embedding"] = json!(vector);
                doc["embedding_version"] = json!(EMBEDDING_VERSION);
            }
            let resp = self.http.put(&url).json(&doc).send().await.context("indexing request failed")?;
            if !resp.status().is_success() {
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("failed to index {doc_id}: {body}");
            }
        }
        // A page with no content yet (chunk_text("") produces zero chunks)
        // still needs any *previously* indexed chunks removed -- e.g. the
        // page was edited down to empty. Cheap and idempotent either way.
        if page.content_markdown.trim().is_empty() {
            self.delete_page(page.id).await?;
        }
        Ok(())
    }

    /// Removes all of a page's chunk documents (soft-delete in Postgres ->
    /// hard removal from search). `?conflicts=proceed` -- see `delete_note`'s
    /// doc comment for why.
    pub async fn delete_page(&self, page_id: Uuid) -> Result<()> {
        let url = format!("{}/{CONTENT_INDEX}/_delete_by_query?conflicts=proceed", self.base_url);
        let query = json!({
            "query": { "bool": { "must": [
                { "term": { "content_type": "page" } },
                { "term": { "content_id": page_id } }
            ]}}
        });
        let resp = self.http.post(&url).json(&query).send().await.context("delete request failed")?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("failed to delete page {page_id}: {body}");
        }
        Ok(())
    }

    /// Hybrid search: lexical (BM25) + semantic (kNN, when an embedder is
    /// available), combined via Reciprocal Rank Fusion rather than trying to
    /// normalize two incomparable score scales. ACL is enforced *in* both
    /// queries, not filtered after the fact — admins get unfiltered results;
    /// everyone else only ever gets back rows they're allowed to see,
    /// computed live from their current team/organization memberships.
    /// Multiple chunks of the same note are deduped to one result (its
    /// best-ranked chunk represents it).
    /// Returns the fused, deduped hit list -- unpaginated (bounded by
    /// `RAW_FETCH_SIZE` per sub-query, not by any caller-supplied
    /// limit/offset). `handlers::search::search` does the page-ACL
    /// post-filter, then splits this by content type and paginates each
    /// half independently -- see that function's own doc comment for why
    /// pagination happens there and not here.
    pub async fn search(
        &self,
        query_text: &str,
        caller: &SearchCaller,
        embedder: Option<&Embedder>,
    ) -> Result<Vec<SearchHit>> {
        let acl_filter = caller.acl_filter();
        // Highlights the same field being matched -- fragments come back as
        // `hit.highlight.text`, `<em>`-wrapped by default (no custom
        // pre/post tags configured; the frontend swaps them for its own
        // <mark> rendering rather than trusting raw HTML from here).
        let highlight = json!({ "fields": { "text": {} } });

        let lexical_query = match &acl_filter {
            Some(filter) => json!({
                "size": RAW_FETCH_SIZE,
                "query": { "bool": { "must": [{ "match": { "text": query_text } }], "filter": [filter] } },
                "highlight": highlight
            }),
            None => json!({
                "size": RAW_FETCH_SIZE,
                "query": { "match": { "text": query_text } },
                "highlight": highlight
            }),
        };
        let lexical_hits = self.raw_search(lexical_query).await?;

        let knn_hits = if let Some(embedder) = embedder {
            let vector = embedder.embed_query(query_text).await?;
            let mut knn_field = json!({ "vector": vector, "k": RAW_FETCH_SIZE });
            if let Some(filter) = &acl_filter {
                knn_field["filter"] = filter.clone();
            }
            // A pure-semantic match can share zero literal terms with the
            // query, so this highlight is frequently empty -- the frontend
            // falls back to a plain truncated `text` prefix in that case,
            // not a missing snippet.
            let knn_query =
                json!({ "size": RAW_FETCH_SIZE, "query": { "knn": { "embedding": knn_field } }, "highlight": highlight });
            self.raw_search(knn_query).await?
        } else {
            Vec::new()
        };

        Ok(reciprocal_rank_fusion(lexical_hits, knn_hits))
    }

    async fn raw_search(&self, query: Value) -> Result<Vec<RawHit>> {
        let url = format!("{}/{CONTENT_INDEX}/_search", self.base_url);
        let resp = self.http.post(&url).json(&query).send().await.context("search request failed")?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("search failed: {body}");
        }
        let body: SearchResponse = resp.json().await.context("failed to parse search response")?;
        Ok(body.hits.hits)
    }
}

/// Combines two ranked hit lists into one, deduped by `content_id` (a note
/// may appear via more than one chunk, and/or in both the lexical and
/// semantic list). RRF score = sum of `1/(RRF_K + rank)` across whichever
/// list(s) a chunk appears in; a note's final score is its best chunk's RRF
/// score — this avoids needing to normalize BM25 scores against cosine
/// similarity, which don't live on comparable scales.
fn reciprocal_rank_fusion(lexical: Vec<RawHit>, knn: Vec<RawHit>) -> Vec<SearchHit> {
    let mut best_by_content: HashMap<Uuid, (f64, RawHit)> = HashMap::new();

    for (list_idx, list) in [lexical, knn].into_iter().enumerate() {
        for (rank, hit) in list.into_iter().enumerate() {
            let rrf = 1.0 / (RRF_K + (rank + 1) as f64);
            let content_id = hit.source.content_id;
            best_by_content
                .entry(content_id)
                .and_modify(|(score, best)| {
                    *score += rrf;
                    // Prefer whichever chunk scored higher in its own list —
                    // approximated here by keeping the first (best-ranked)
                    // occurrence, since entries are processed in rank order
                    // within each list.
                    let _ = (list_idx, &*best);
                })
                .or_insert((rrf, hit));
        }
    }

    let mut results: Vec<SearchHit> = best_by_content
        .into_values()
        .map(|(score, hit)| SearchHit {
            content_id: hit.source.content_id,
            content_type: hit.source.content_type,
            score: score as f32,
            title: hit.source.title,
            text: hit.source.text,
            highlight: hit.highlight.map(|h| h.text).unwrap_or_default(),
            folder_id: hit.source.folder_id,
            space_id: hit.source.space_id,
            parent_id: hit.source.parent_id,
        })
        .collect();
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    results
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
            // Coarse pre-filter only -- "any page belonging to one of the
            // caller's organizations". The real per-page permission check
            // (ancestor overrides, space membership) happens as a live
            // post-filter in handlers::search::search, since it can't be
            // expressed as a static OpenSearch query the way Notes'
            // visibility enum can.
            should.push(json!({
                "bool": {
                    "must": [
                        { "term": { "content_type": "page" } },
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
    /// Empty for a reply (see `Note::title`) -- the frontend falls back to
    /// showing it as "a reply" and links via `parent_id` instead.
    pub title: String,
    /// The matched chunk's raw text -- kept as a fallback for when
    /// `highlight` is empty (a pure-semantic kNN match can share zero
    /// literal terms with the query, so OpenSearch has nothing to
    /// highlight), truncated by the frontend for display.
    pub text: String,
    /// `<em>`-wrapped fragments from OpenSearch's own highlighter, when the
    /// lexical query actually matched literal terms in this chunk.
    pub highlight: Vec<String>,
    /// Set only for a note hit filed in a folder.
    pub folder_id: Option<Uuid>,
    /// Set only for a page hit -- what the frontend was missing to build
    /// `/spaces/:spaceId/pages/:pageId` at all.
    pub space_id: Option<Uuid>,
    /// Set only for a reply hit -- the parent (thread root) note's id. The
    /// frontend links a reply hit to `/notes/{parent_id}`, not
    /// `/notes/{content_id}` (the reply's own id has no useful standalone
    /// view).
    pub parent_id: Option<Uuid>,
}

/// One content type's slice of a search response -- see `SearchResults`.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct SearchTypeResults {
    pub hits: Vec<SearchHit>,
    /// Total matching hits of this type, ignoring `limit`/`offset` -- lets
    /// the frontend render "Page N of M" per type independently.
    pub total: i64,
}

/// `GET /search`'s response: grouped by content type, each type paginated
/// independently (confirmed design: search stays global across both types
/// regardless of which Navigator tab is active, results grouped and each
/// group gets its own pager).
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct SearchResults {
    pub notes: SearchTypeResults,
    pub pages: SearchTypeResults,
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
    #[serde(rename = "_source")]
    source: RawSource,
    #[serde(default)]
    highlight: Option<RawHighlight>,
}

#[derive(Deserialize)]
struct RawHighlight {
    #[serde(default)]
    text: Vec<String>,
}

#[derive(Deserialize)]
struct RawSource {
    content_id: Uuid,
    content_type: String,
    #[serde(default)]
    title: String,
    text: String,
    #[serde(default)]
    folder_id: Option<Uuid>,
    #[serde(default)]
    space_id: Option<Uuid>,
    #[serde(default)]
    parent_id: Option<Uuid>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caller(is_admin: bool, team_ids: Vec<Uuid>, organization_ids: Vec<Uuid>) -> SearchCaller {
        SearchCaller { user_id: Uuid::new_v4(), is_admin, team_ids, organization_ids }
    }

    fn hit(content_id: Uuid, text: &str) -> RawHit {
        RawHit {
            source: RawSource {
                content_id,
                content_type: "note".into(),
                title: String::new(),
                text: text.into(),
                folder_id: None,
                space_id: None,
                parent_id: None,
            },
            highlight: None,
        }
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
    fn non_admin_with_teams_and_orgs_gets_all_four_should_clauses() {
        let filter = caller(false, vec![Uuid::new_v4()], vec![Uuid::new_v4()]).acl_filter().unwrap();
        let should = filter["bool"]["should"].as_array().unwrap();
        assert_eq!(
            should.len(),
            4,
            "created_by + team clause + organization clause + page-organization-membership clause"
        );
    }

    #[test]
    fn organization_member_gets_a_coarse_page_should_clause() {
        let org_id = Uuid::new_v4();
        let filter = caller(false, vec![], vec![org_id]).acl_filter().unwrap();
        let should = filter["bool"]["should"].as_array().unwrap();
        let page_clause = &should[2];
        assert_eq!(page_clause["bool"]["must"][0]["term"]["content_type"], "page");
        assert_eq!(page_clause["bool"]["must"][1]["terms"]["organization_id"][0], org_id.to_string());
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

    #[test]
    fn rrf_ranks_a_note_appearing_in_both_lists_above_one_appearing_in_only_one() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let lexical = vec![hit(a, "a"), hit(b, "b")];
        let knn = vec![hit(a, "a")];
        let results = reciprocal_rank_fusion(lexical, knn);
        assert_eq!(results[0].content_id, a, "a appears in both lists, should rank first");
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn rrf_dedupes_multiple_chunks_of_the_same_note() {
        let note_id = Uuid::new_v4();
        let lexical = vec![hit(note_id, "chunk 0"), hit(note_id, "chunk 1")];
        let results = reciprocal_rank_fusion(lexical, vec![]);
        assert_eq!(results.len(), 1, "two chunks of the same note must collapse to one result");
    }

    #[test]
    fn rrf_empty_lists_return_empty_results() {
        assert!(reciprocal_rank_fusion(vec![], vec![]).is_empty());
    }
}
