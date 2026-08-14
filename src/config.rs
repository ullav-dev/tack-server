use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub database_url: String,
    pub oauth2_jwks_url: String,
    pub oauth2_issuer: String,
    pub opensearch_url: String,
    /// Canonical public URI of this server's MCP endpoint — used as the
    /// OAuth2 resource-server audience. Defaults to a local dev URL; must be
    /// set to the real public URL in any deployed environment.
    pub tack_mcp_canonical_uri: String,
    /// Where the local embedding model is cached (downloaded once on first
    /// run if absent). Point this at a persistent volume in any deployed
    /// environment so the model survives container restarts/redeploys.
    pub embedding_model_cache_dir: std::path::PathBuf,
    /// ONNX Runtime intra-op thread count for the embedding model. Default
    /// (1) is deliberately conservative, not tuned for throughput -- see the
    /// doc comment on the env var default below for why. Raise this if the
    /// deployment environment has memory headroom to spare and embedding
    /// throughput is actually a bottleneck (it isn't expected to be: this
    /// runs off the write path, via the outbox worker, or once per search
    /// query -- never a hot synchronous path).
    pub embedding_intra_threads: usize,
    /// Tokio runtime worker thread count. Same host-core-count-leakage
    /// problem as `embedding_intra_threads` (see its own doc comment), one
    /// level up the stack: `#[tokio::main]`'s default multi-thread runtime
    /// sizes its worker pool to `std::thread::available_parallelism()`,
    /// which is the *host's* full core count under most container
    /// schedulers, not the pod's actual CPU allotment. Each worker is its
    /// own OS thread (default 2MiB stack) plus its own scheduler queues --
    /// on a many-core node this is real, unbounded baseline memory that has
    /// nothing to do with actual request concurrency. A small fixed default
    /// is still comfortably enough for this service's real load (a handful
    /// of concurrent requests, not thousands).
    pub tokio_worker_threads: usize,
    /// Postgres connection pool ceiling. `deadpool`'s own default (used if
    /// this were left unset) is `num_cpus::get() * 2` -- the exact same
    /// host-core-count leakage as the two fields above (`num_cpus`, like
    /// `available_parallelism()`, reads `sched_getaffinity`). On a busy
    /// many-core node this pool ceiling could be 60-100+, and each actually-
    /// opened connection (this service's own client-side buffers, not just
    /// the Postgres server's backend process) is real memory that scales
    /// with how much of that ceiling the caller's own concurrency actually
    /// reaches -- unlike the two fields above, this one only matters under
    /// real load, not at startup, which is why it wasn't caught by the
    /// startup-OOM investigation that found the other two.
    pub db_pool_max_size: usize,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            host: std::env::var("HOST").unwrap_or_else(|_| "0.0.0.0".into()),
            port: std::env::var("PORT")
                .unwrap_or_else(|_| "8087".into())
                .parse()
                .context("PORT must be a valid number")?,
            database_url: std::env::var("DATABASE_URL").context("DATABASE_URL is required")?,
            oauth2_jwks_url: std::env::var("OAUTH2_JWKS_URL")
                .unwrap_or_else(|_| "http://localhost:8081/oauth2/jwks".into()),
            oauth2_issuer: std::env::var("OAUTH2_ISSUER")
                .unwrap_or_else(|_| "http://localhost:8081".into()),
            opensearch_url: std::env::var("OPENSEARCH_URL")
                .unwrap_or_else(|_| "http://localhost:9200".into()),
            tack_mcp_canonical_uri: std::env::var("TACK_MCP_CANONICAL_URI")
                .unwrap_or_else(|_| "http://localhost:8087/mcp".into()),
            embedding_model_cache_dir: std::env::var("EMBEDDING_MODEL_CACHE_DIR")
                .unwrap_or_else(|_| "./.embedding-models".into())
                .into(),
            // `fastembed`'s own default (used if this were left unset) is
            // `std::thread::available_parallelism()` -- on Linux this reads
            // the process's CPU affinity mask via `sched_getaffinity`, which
            // most container schedulers (Kubernetes' default `none` CPU
            // Manager policy included -- CFS quota/period throttling, not
            // cpuset pinning) leave unrestricted to the *host's* full core
            // count, not the pod's actual `resources` allotment. On a node
            // with many cores this drove ONNX Runtime to spin up one
            // intra-op thread (and its own memory arena) per host core
            // during session creation, OOM-killing the process before
            // startup ever reaches "Listening on" -- confirmed as the root
            // cause of a production deploy failure (every node in the
            // cluster hit it, "even in isolation", because the host core
            // count -- not the pod's memory limit -- was driving the
            // allocation). A small fixed default sidesteps this entirely,
            // independent of whatever the cluster's CPU Manager policy is.
            embedding_intra_threads: std::env::var("EMBEDDING_INTRA_THREADS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1),
            tokio_worker_threads: std::env::var("TOKIO_WORKER_THREADS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(4),
            db_pool_max_size: std::env::var("DB_POOL_MAX_SIZE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(10),
        })
    }

    pub fn bind_addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}
