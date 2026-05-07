-- argunix v1 schema. See design/questions-answers.md Q73.
-- Timestamps are stored as RFC 3339 TEXT (UTC) to keep raw SQL inspection
-- readable; sqlx maps them to chrono::DateTime<Utc>.

CREATE TABLE repos (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    forge TEXT NOT NULL,
    slug TEXT NOT NULL,
    UNIQUE (forge, slug)
);

CREATE TABLE evaluations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id INTEGER NOT NULL REFERENCES repos(id),
    trigger TEXT NOT NULL,
    git_ref TEXT NOT NULL,
    sha TEXT NOT NULL,
    started_at TEXT,
    finished_at TEXT,
    status TEXT NOT NULL
);

CREATE INDEX idx_evaluations_repo ON evaluations(repo_id);
CREATE INDEX idx_evaluations_status ON evaluations(status);

CREATE TABLE jobs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    eval_id INTEGER NOT NULL REFERENCES evaluations(id),
    attr_path TEXT NOT NULL,
    drv_path TEXT,
    system TEXT NOT NULL,
    started_at TEXT,
    finished_at TEXT,
    status TEXT NOT NULL,
    log_path TEXT,
    output_path TEXT
);

CREATE INDEX idx_jobs_eval ON jobs(eval_id);
CREATE INDEX idx_jobs_status ON jobs(status);

CREATE TABLE queue (
    job_id INTEGER PRIMARY KEY REFERENCES jobs(id),
    priority INTEGER NOT NULL DEFAULT 0,
    enqueued_at TEXT NOT NULL,
    dispatched_at TEXT
);

CREATE TABLE forge_status (
    eval_id INTEGER NOT NULL REFERENCES evaluations(id),
    kind TEXT NOT NULL,
    handle TEXT,
    last_posted_at TEXT NOT NULL,
    PRIMARY KEY (eval_id, kind)
);
