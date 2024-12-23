use diesel::result::Error as DieselError;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Diesel error: {0}")]
    Diesel(#[from] DieselError),
    #[error("Serde error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("Jsonschema error: {0}")]
    Jsonschema(#[from] jsonschema::ValidationError<'static>),
}
