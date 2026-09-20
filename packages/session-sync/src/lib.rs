#![deny(unsafe_code)]
//! Consent-gated browser cookie extraction. Secrets never cross desktop IPC.
mod crypto;
mod local_storage;
mod paths;
mod profile;
mod reader;
mod service;
mod user_agent;

pub use local_storage::{StorageItem, build_hydration_script, read_local_storage};
pub use paths::{cookie_db_candidates, local_base, resolve_profile_dir, user_data_dir};
pub use profile::{BrowserSource, SyncRequest, ValidatedRequest, validate_portal};
pub use reader::{Cookie, CookieSameSite, ancestor_roots, read_profile, sso_secondaries};
pub use service::{FallbackReason, PreparedSync, prepare};
pub use user_agent::{build_user_agent, source_user_agent};

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("Consent is required before reading browser cookies")]
    ConsentRequired,
    #[error("Enter an HTTPS portal URL without credentials")]
    InvalidPortal,
    #[error("Choose Default or a numbered browser profile")]
    InvalidProfile,
    #[error("Cookie database is unavailable")]
    Database(#[from] sqlx::Error),
    #[error("Cookie format is unsupported")]
    UnsupportedFormat,
    #[error("Cookie decryption failed")]
    Decryption,
    #[error("No current cookies were found for this portal")]
    MissingCookies,
    #[error("Cookie reader failed")]
    WorkerFailed,
    #[error("Cookie profile could not be located: {}", path.display())]
    ProfileUnavailable { path: std::path::PathBuf },
    #[error("Cookie database could not be staged for reading")]
    Unstageable,
}
