use diesel::result::Error as DieselError;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Diesel error: {0}")]
    DieselError(#[from] DieselError),
    #[error("Serde error: {0}")]
    SerdeError(#[from] serde_json::Error),
}
