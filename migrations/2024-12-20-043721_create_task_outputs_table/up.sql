-- Your SQL goes here
CREATE TABLE task_outputs (
    id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL,
    output TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    FOREIGN KEY (task_id) REFERENCES tasks(task_id)
);
