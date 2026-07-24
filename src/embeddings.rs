//! Self-hosted local embedding model — no external API calls, no per-call
//! cost, per the explicit "self-hosted, not an external provider" decision
//! for semantic search. Uses `fastembed` (ONNX Runtime under the hood via
//! `ort`) to run inference in-process.
//!
//! Model: multilingual-e5-small (384 dimensions) — chosen specifically for
//! multilingual support, since that's a stated requirement for search from
//! the start of this project, not an English-only model like the more
//! commonly-reached-for all-MiniLM-L6-v2.
//!
//! E5 models are trained with a `"query: "` / `"passage: "` prefix
//! convention — queries and documents are prefixed differently, which
//! measurably affects retrieval quality. Get this right rather than embed
//! raw text for both, since it's free to do correctly from the start and
//! silently wrong (but still "working") if skipped.

use anyhow::{Context, Result};
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// multilingual-e5-small's output dimension. Part of the OpenSearch mapping
/// (`knn_vector` dimension) — changing the model means reindexing everything,
/// same one-way-door caveat as any embedding-model choice.
pub const EMBEDDING_DIMENSION: usize = 384;

/// Stored alongside every embedding so a future model upgrade is a trackable
/// backfill job, not a silent mix of incompatible vectors in one index.
pub const EMBEDDING_VERSION: &str = "multilingual-e5-small-v1";

/// Word-count chunking for long notes, with overlap so a chunk boundary
/// doesn't split the one sentence that actually answers a query.
const CHUNK_SIZE_WORDS: usize = 400;
const CHUNK_OVERLAP_WORDS: usize = 50;

#[derive(Clone)]
pub struct Embedder {
    // `ort::Session` inference is not verified `Sync` by this crate's public
    // API, so a plain Mutex around the whole model is the safe default —
    // embedding happens off the write path via the outbox worker (or once
    // per search query), never a hot synchronous path, so serializing calls
    // here is an acceptable trade-off, not a bottleneck.
    inner: Arc<Mutex<TextEmbedding>>,
}

impl Embedder {
    /// Loads the model, downloading it to `cache_dir` on first run if not
    /// already cached. This is the one place this service reaches out to an
    /// external host (Hugging Face) — a one-time model fetch, not a
    /// per-embedding API call; bake the model into the Docker image (or
    /// pre-warm the cache volume) in any environment without outbound
    /// internet access.
    pub fn new(cache_dir: PathBuf) -> Result<Self> {
        let model = TextEmbedding::try_new(
            InitOptions::new(EmbeddingModel::MultilingualE5Small)
                .with_cache_dir(cache_dir)
                .with_show_download_progress(false),
        )
        .context("failed to load embedding model")?;
        Ok(Self { inner: Arc::new(Mutex::new(model)) })
    }

    /// Embeds search-query text (uses E5's `"query: "` prefix).
    pub async fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        let prefixed = format!("query: {text}");
        let mut result = self.embed_batch(vec![prefixed]).await?;
        Ok(result.remove(0))
    }

    /// Embeds document/content text (uses E5's `"passage: "` prefix).
    pub async fn embed_passages(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        let prefixed: Vec<String> = texts.into_iter().map(|t| format!("passage: {t}")).collect();
        self.embed_batch(prefixed).await
    }

    async fn embed_batch(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let model = inner.lock().expect("embedding model mutex poisoned");
            model.embed(texts, None)
        })
        .await
        .context("embedding task panicked")?
        .context("embedding inference failed")
    }
}

/// Splits `text` into overlapping word-count chunks. A short note (the
/// common case) is a single chunk. Returns `(chunk_index, chunk_text)` pairs.
pub fn chunk_text(text: &str) -> Vec<(i32, String)> {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() {
        return vec![(0, String::new())];
    }
    if words.len() <= CHUNK_SIZE_WORDS {
        return vec![(0, text.to_string())];
    }

    let mut chunks = Vec::new();
    let mut start = 0usize;
    let mut idx = 0i32;
    let step = CHUNK_SIZE_WORDS - CHUNK_OVERLAP_WORDS;
    while start < words.len() {
        let end = (start + CHUNK_SIZE_WORDS).min(words.len());
        chunks.push((idx, words[start..end].join(" ")));
        idx += 1;
        if end == words.len() {
            break;
        }
        start += step;
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_a_single_chunk() {
        let chunks = chunk_text("just a short note body");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].0, 0);
    }

    #[test]
    fn empty_text_is_a_single_empty_chunk() {
        let chunks = chunk_text("");
        assert_eq!(chunks, vec![(0, String::new())]);
    }

    #[test]
    fn long_text_splits_into_overlapping_chunks() {
        let words: Vec<String> = (0..1000).map(|i| format!("word{i}")).collect();
        let text = words.join(" ");
        let chunks = chunk_text(&text);
        assert!(chunks.len() > 1, "expected more than one chunk for 1000 words");
        // Indices are sequential starting at 0.
        for (i, (idx, _)) in chunks.iter().enumerate() {
            assert_eq!(*idx, i as i32);
        }
        // Every word appears in at least one chunk (nothing silently dropped).
        let covered: std::collections::HashSet<&str> =
            chunks.iter().flat_map(|(_, c)| c.split_whitespace()).collect();
        assert_eq!(covered.len(), 1000);
    }

    #[test]
    fn chunks_overlap() {
        let words: Vec<String> = (0..900).map(|i| format!("w{i}")).collect();
        let text = words.join(" ");
        let chunks = chunk_text(&text);
        assert!(chunks.len() >= 2);
        let first_words: Vec<&str> = chunks[0].1.split_whitespace().collect();
        let second_words: Vec<&str> = chunks[1].1.split_whitespace().collect();
        let overlap = first_words
            .iter()
            .rev()
            .take(CHUNK_OVERLAP_WORDS)
            .filter(|w| second_words.contains(w))
            .count();
        assert!(overlap > 0, "expected some overlap between consecutive chunks");
    }
}
