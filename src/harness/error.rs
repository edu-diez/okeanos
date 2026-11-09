use thiserror::Error;

#[derive(Debug, Error)]
pub enum HarnessError {
    #[error("unsupported mode: {0}")]
    UnsupportedMode(String),
    #[error("initialization failed: {0}")]
    Initialization(String),
}
