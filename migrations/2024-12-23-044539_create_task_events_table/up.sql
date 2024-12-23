-- Your SQL goes here
CREATE TABLE IF NOT EXISTS task_events (
    id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL,
    agent_role TEXT,         -- ex: 'proposer', 'reviewer', 'validator', 'formatter'
    event_type TEXT NOT NULL,-- ex: 'module_request', 'proposal_generated', 'revision_requested'...
    content TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    FOREIGN KEY (task_id) REFERENCES tasks(task_id)
);

CREATE INDEX IF NOT EXISTS idx_task_events_task_id ON task_events(task_id);
