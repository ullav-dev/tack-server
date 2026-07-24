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
        })
    }

    pub fn bind_addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}
