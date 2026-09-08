//! Orchestration shared by the web handlers and the JSON API: anything
//! that needs more than the store (password hashes, mailers, media, the
//! import scratch dir) and would otherwise be duplicated
//! between the two surfaces. Sync like `Services`; callers wrap in
//! spawn_blocking. Handlers stay thin: parse, call one of these, render.

pub mod account;
pub mod enroll;
pub mod export_flow;
pub mod import_flow;
pub mod login;
pub mod reset;

/// How a flow fails, in the two voices a handler must keep apart: a
/// sentence for the person (their file, their input) or a detail for
/// the log (our disk, our database, our bug). Handlers answer `User`
/// with 400 and the sentence, `Internal` with 500 and nothing but
/// "internal error"; the type makes mixing them a compile error rather
/// than a slip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    User(String),
    Internal(String),
}

impl Failure {
    pub fn user(message: impl Into<String>) -> Self {
        Failure::User(message.into())
    }

    pub fn internal(detail: impl std::fmt::Display) -> Self {
        Failure::Internal(detail.to_string())
    }
}

/// What a person may see: the user sentence, or a plain "internal
/// error". The detail of an `Internal` is reachable only by matching.
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::User(msg) => f.write_str(msg),
            Failure::Internal(_) => f.write_str("internal error"),
        }
    }
}

impl From<std::io::Error> for Failure {
    fn from(e: std::io::Error) -> Self {
        Failure::internal(e)
    }
}

impl From<serde_json::Error> for Failure {
    fn from(e: serde_json::Error) -> Self {
        Failure::internal(e)
    }
}

impl From<flash_store::StoreError> for Failure {
    fn from(e: flash_store::StoreError) -> Self {
        use flash_store::StoreError;
        match e {
            StoreError::Invalid(msg) => Failure::User(msg),
            StoreError::NotFound(what) => Failure::User(format!("not found: {what}")),
            StoreError::CapExceeded { .. } => Failure::User(e.to_string()),
            StoreError::Db(_) => Failure::internal(e),
        }
    }
}

impl From<crate::service::ServiceError> for Failure {
    fn from(e: crate::service::ServiceError) -> Self {
        use crate::service::ServiceError;
        match e {
            ServiceError::Invalid(msg) => Failure::User(msg),
            ServiceError::OverCap { message, .. } => Failure::User(message),
            ServiceError::Store(store) => store.into(),
            ServiceError::Scheduler(_) | ServiceError::Internal(_) => Failure::internal(e),
        }
    }
}

impl From<crate::media_store::MediaStoreError> for Failure {
    fn from(e: crate::media_store::MediaStoreError) -> Self {
        Failure::internal(e)
    }
}
