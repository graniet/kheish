CREATE TABLE tasks (
    id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL,
    name TEXT NULL,
    description TEXT NULL,
    state TEXT NOT NULL,
    context TEXT NULL,
    proposal_history TEXT NULL,
    current_proposal TEXT NULL,
    feedback_history TEXT NULL,
    module_execution_history TEXT NULL,
    conversation TEXT NULL,
    config TEXT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    last_run_at DATETIME NULL,
    interval TEXT NULL
);
