//! Session persistence and transcript parsing helpers.

mod fs;
mod path;
mod store;
mod transcript;

pub use fs::{
    append_json_line_sync, append_json_lines_sync, atomic_write, write_json_pretty_atomically,
};
pub use kheish_types::{CompactBoundaryMetadata, CompactionTrigger, PreservedSegment};
pub use path::{
    decode_safe_storage_name, legacy_storage_dir, legacy_storage_name, legacy_storage_path,
    prepare_storage_dir_for_write, prepare_storage_path_for_write, resolve_storage_dir_for_read,
    resolve_storage_path_for_read, safe_storage_dir, safe_storage_name, safe_storage_path,
};
pub use store::{
    FileSessionStore, PermissionAuditRecord, PersistedSessionRecord, SessionMigration,
    SessionRecordEnvelope, SessionRestoreCursor, StoredOutputRecord, StoredSession,
};
pub use transcript::{
    NormalizedTranscriptMessage, ParsedToolCall, ParsedToolResult, TranscriptGraph, TranscriptNode,
};
