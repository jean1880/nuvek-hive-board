//! Typed API errors. The JSON body never carries database text, so a
//! credential or the connection URL cannot leak through an error.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    Validation(String),
    #[error("unauthorized")]
    Unauthorized,
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("database unavailable")]
    Unavailable,
    #[error("internal error")]
    Internal,
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn validation(msg: impl Into<String>) -> Self {
        Self::Validation(msg.into())
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::NotFound(msg.into())
    }
    pub fn conflict(msg: impl Into<String>) -> Self {
        Self::Conflict(msg.into())
    }

    pub fn status(&self) -> StatusCode {
        match self {
            Self::Validation(_) => StatusCode::BAD_REQUEST,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::Validation(_) => "validation",
            Self::Unauthorized => "unauthorized",
            Self::NotFound(_) => "not_found",
            Self::Conflict(_) => "conflict",
            Self::Unavailable => "unavailable",
            Self::Internal => "internal",
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(serde_json::json!({
            "error": self.to_string(),
            "code": self.code(),
        }));
        (self.status(), body).into_response()
    }
}

/// Name the relation kind of a foreign-key violation from the generated
/// constraint name (`<table>_<column>_fkey`), e.g. `findings_message_id_fkey`
/// becomes "message". Only schema identifiers are named, never a value.
fn fk_message(table: Option<&str>, constraint: Option<&str>) -> String {
    let kind = constraint
        .and_then(|c| c.strip_suffix("_fkey"))
        .map(|c| match table {
            Some(t) => c.strip_prefix(&format!("{t}_")).unwrap_or(c),
            None => c,
        })
        .map(|c| c.strip_suffix("_id").unwrap_or(c))
        .filter(|c| !c.is_empty())
        .unwrap_or("record");
    format!("conflict: the referenced {kind} does not exist or is still referenced")
}

impl From<sqlx::Error> for ApiError {
    fn from(err: sqlx::Error) -> Self {
        match &err {
            sqlx::Error::RowNotFound => Self::NotFound("not found".into()),
            sqlx::Error::PoolTimedOut
            | sqlx::Error::PoolClosed
            | sqlx::Error::Io(_)
            | sqlx::Error::WorkerCrashed => {
                tracing::warn!(kind = "db_unavailable", "database unavailable");
                Self::Unavailable
            }
            sqlx::Error::Database(db) => {
                let code = db.code().map(|c| c.to_string()).unwrap_or_default();
                match code.as_str() {
                    "23505" => Self::Conflict("already exists".into()),
                    "23503" => Self::Conflict(fk_message(db.table(), db.constraint())),
                    // 22021 invalid byte sequence (NUL), 22P05 untranslatable character.
                    "23514" | "23502" | "22001" | "22P02" | "22021" | "22P05" => {
                        Self::Validation("value rejected by the schema".into())
                    }
                    // 57014 statement_timeout, 57P01/57P02/57P03 shutdown, 53xxx resources, 08xxx connection.
                    c if c.starts_with("57") || c.starts_with("53") || c.starts_with("08") => {
                        tracing::warn!(sqlstate = c, "database unavailable");
                        Self::Unavailable
                    }
                    c => {
                        tracing::error!(sqlstate = c, "database error");
                        Self::Internal
                    }
                }
            }
            _ => {
                tracing::error!(kind = "sqlx", "database error");
                Self::Internal
            }
        }
    }
}
