//! Endpoint and identity configuration for hive-mcp. Nothing here names a local path: the
//! client stores no Hive data. Every default is `localhost`; set the variables to reach a
//! remote deployment.

/// Fallback host when a per-service variable is unset.
pub const DEFAULT_HOST: &str = "localhost";

const SERVER_URL_VAR: &str = "HIVE_SERVER_URL";
const TOKEN_VAR: &str = "HIVE_API_TOKEN";
const AGENT_TYPE_VAR: &str = "HIVE_AGENT_TYPE";
const MODEL_VAR: &str = "HIVE_MODEL";
const HARNESS_VAR: &str = "HIVE_HARNESS";

fn url(var: &str, default: String) -> String {
    nuvek_core::env::env_or(var, default)
        .trim_end_matches('/')
        .to_string()
}

/// hive-board's base URL (`HIVE_SERVER_URL`), default `http://localhost:8904`.
pub fn server_url() -> String {
    // Hive::new trims a trailing '/'. hive-gardener uses the same variable and default.
    nuvek_core::env::env_or(SERVER_URL_VAR, format!("http://{DEFAULT_HOST}:8904"))
}

/// Bearer token for writes. Never logged. `None` is legal when the server runs `HIVE_AUTH=off`.
pub fn api_token() -> Option<String> {
    nuvek_core::env::env_var(TOKEN_VAR)
}

/// The caller's identity, sent as the `agent` object on writes. Each field defaults to
/// `unknown`; the server then falls back to the alias row for the author.
pub fn agent_identity() -> (String, String, String) {
    (
        nuvek_core::env::env_or(AGENT_TYPE_VAR, "unknown"),
        nuvek_core::env::env_or(MODEL_VAR, "unknown"),
        nuvek_core::env::env_or(HARNESS_VAR, "unknown"),
    )
}

/// The topology graph's tenant label (`HIVE_TOPOLOGY_LABEL`), validated against
/// `^[A-Za-z][A-Za-z0-9_]{0,63}$` because Cypher labels cannot be parameters and this one is
/// interpolated into the brief's subject query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopologyLabel(String);

impl TopologyLabel {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let mut cs = raw.chars();
        let ok = raw.len() <= 64
            && cs.next().is_some_and(|c| c.is_ascii_alphabetic())
            && cs.all(|c| c.is_ascii_alphanumeric() || c == '_');
        if ok {
            Ok(Self(raw.to_string()))
        } else {
            Err(format!(
                "HIVE_TOPOLOGY_LABEL {raw:?} is not a valid label (^[A-Za-z][A-Za-z0-9_]{{0,63}}$)"
            ))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// `HIVE_TOPOLOGY_LABEL`. No default: unset (or blank) is `Ok(None)` and the brief skips the
/// graph source. An invalid value is `Err`, a startup error.
pub fn topology_label() -> Result<Option<TopologyLabel>, String> {
    nuvek_core::env::env_var("HIVE_TOPOLOGY_LABEL")
        .map(|v| TopologyLabel::parse(v.trim()))
        .transpose()
}

pub fn neo4j_uri() -> String {
    url("NEO4J_HOST", format!("bolt://{DEFAULT_HOST}:7687"))
}

pub fn neo4j_user() -> String {
    nuvek_core::env::env_or("NEO4J_USER", "neo4j")
}

pub fn neo4j_config() -> Option<nuvek_neo4j::Neo4jConfig> {
    nuvek_core::env::env_var("NEO4J_PASSWORD").map(|password| nuvek_neo4j::Neo4jConfig {
        uri: neo4j_uri(),
        user: neo4j_user(),
        password,
        timeout: Some(std::time::Duration::from_secs(4)),
    })
}

pub fn chroma_config() -> nuvek_chroma::ChromaConfig {
    nuvek_chroma::ChromaConfig {
        chroma_base: url("CHROMA_URL", format!("http://{DEFAULT_HOST}:8000")),
        ollama_base: url("OLLAMA_URL_BASE", format!("http://{DEFAULT_HOST}:11434")),
        embed_model: nuvek_core::env::env_or("NUVEK_EMBED_MODEL", nuvek_chroma::EMBED_MODEL),
        ..Default::default()
    }
}
