-- Schema version 6 of a fresh user-memory store: sqlite_master in
-- creation order, without automatic indexes or FTS5 shadow tables.
-- Written by tests/schema.rs; never edit the snapshot of a released version.
CREATE TABLE memories (
    id TEXT PRIMARY KEY NOT NULL CHECK(length(id)=36 AND substr(id,9,1)='-' AND substr(id,14,1)='-' AND substr(id,19,1)='-' AND substr(id,24,1)='-' AND substr(id,15,1)='4' AND substr(id,20,1) IN ('8','9','a','b') AND length(replace(id,'-',''))=32 AND replace(id,'-','') NOT GLOB '*[^0-9a-f]*'),
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    memory_type TEXT NOT NULL,
    category TEXT NOT NULL,
    context TEXT NOT NULL DEFAULT '',
    title TEXT NOT NULL CHECK(length(title) BETWEEN 1 AND 120 AND instr(title,char(10))=0 AND instr(title,char(13))=0),
    content TEXT NOT NULL,
    reason TEXT NOT NULL DEFAULT '',
    scope TEXT NOT NULL DEFAULT 'global',
    project TEXT NOT NULL DEFAULT '',
    source TEXT NOT NULL DEFAULT 'explicit',
    status TEXT NOT NULL DEFAULT 'confirmed',
    evidence TEXT NOT NULL DEFAULT '',
    evidence_ids TEXT NOT NULL DEFAULT '[]',
    superseded_by TEXT,
    last_confirmed_at TEXT,
    CHECK(source='inferred' OR evidence_ids='[]'),
    CHECK(status IN ('confirmed','tentative','stale','superseded')),
    CHECK((status='superseded')=(superseded_by IS NOT NULL))
)
;
CREATE TABLE memory_origins (
    memory_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    origin TEXT NOT NULL CHECK(length(origin) BETWEEN 1 AND 240),
    first_seen TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY(memory_id, origin)
)
;
CREATE TABLE memory_distinct (
    a TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    b TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    PRIMARY KEY(a, b),
    CHECK(a < b)
)
;
CREATE TRIGGER memory_support_deleted AFTER DELETE ON memories BEGIN
    UPDATE memories SET status='stale',updated_at=CURRENT_TIMESTAMP
    WHERE status!='superseded' AND EXISTS (SELECT 1 FROM json_each(evidence_ids) WHERE value=OLD.id);
END
;
CREATE TRIGGER memory_support_changed AFTER UPDATE OF memory_type,category,context,content,source,status,evidence ON memories
WHEN (OLD.memory_type,OLD.category,OLD.context,OLD.content)
    IS NOT (NEW.memory_type,NEW.category,NEW.context,NEW.content)
    OR (OLD.source IS NOT NEW.source AND NOT (OLD.source='observed' AND NEW.source='explicit'))
    OR (OLD.status IS NOT NEW.status AND NOT (OLD.status='tentative' AND NEW.status='confirmed'))
    OR (OLD.evidence IS NOT NEW.evidence AND NOT (OLD.source='observed' AND NEW.source='explicit' AND NEW.evidence='')) BEGIN
    UPDATE memories SET status='stale',updated_at=CURRENT_TIMESTAMP
    WHERE status!='superseded' AND EXISTS (SELECT 1 FROM json_each(evidence_ids) WHERE value=OLD.id);
END
;
CREATE TRIGGER memory_support_moved AFTER UPDATE OF scope,project ON memories
WHEN (OLD.scope,OLD.project) IS NOT (NEW.scope,NEW.project) AND NOT (NEW.scope='global' AND NEW.project='') BEGIN
    UPDATE memories SET status='stale',updated_at=CURRENT_TIMESTAMP
    WHERE status!='superseded' AND EXISTS (SELECT 1 FROM json_each(evidence_ids) WHERE value=OLD.id)
    AND (scope!='project' OR project IS NOT NEW.project);
END
;
CREATE TRIGGER memory_origin_removed AFTER DELETE ON memory_origins BEGIN
    UPDATE memories SET status='stale',updated_at=CURRENT_TIMESTAMP
    WHERE status!='superseded' AND EXISTS (SELECT 1 FROM json_each(evidence_ids) WHERE value=OLD.memory_id)
    AND (SELECT COUNT(DISTINCT o.origin) FROM json_each(memories.evidence_ids) e JOIN memory_origins o ON o.memory_id=e.value)
        + (SELECT COUNT(*) FROM json_each(memories.evidence_ids) e WHERE NOT EXISTS (SELECT 1 FROM memory_origins o WHERE o.memory_id=e.value)) < 2;
END
;
CREATE TABLE conversation_projects (
    origin TEXT NOT NULL CHECK(length(origin) BETWEEN 1 AND 240),
    project TEXT NOT NULL CHECK(length(project) BETWEEN 1 AND 240),
    linked_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY(origin, project)
)
;
CREATE TABLE embedding_cutoffs (
    model TEXT NOT NULL CHECK(length(model) BETWEEN 1 AND 200),
    length INTEGER NOT NULL CHECK(length > 0),
    duplicate REAL,
    related REAL,
    calibrated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY(model, length),
    CHECK((duplicate IS NULL) = (related IS NULL)),
    CHECK(duplicate IS NULL OR (related < duplicate AND duplicate <= 1.0 AND related >= -1.0))
)
;
CREATE TABLE memory_vectors (
    id TEXT PRIMARY KEY NOT NULL,
    model TEXT NOT NULL,
    dimensions INTEGER NOT NULL,
    source TEXT NOT NULL,
    vector BLOB NOT NULL
)
;
CREATE TABLE search_keys (
    key INTEGER PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    memory_type TEXT NOT NULL,
    category TEXT NOT NULL,
    scope TEXT NOT NULL,
    project TEXT NOT NULL,
    source TEXT NOT NULL,
    status TEXT NOT NULL,
    updated_at TEXT NOT NULL
)
;
CREATE VIRTUAL TABLE search_words USING fts5(category,title,type,body,content='',contentless_delete=1,detail=column,tokenize='ascii')
;
CREATE TABLE search_pending (key INTEGER PRIMARY KEY NOT NULL)
;
CREATE TABLE search_state (
    id INTEGER PRIMARY KEY CHECK(id=1),
    stemmed_by TEXT NOT NULL
)
;
CREATE TABLE search_changes (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL
)
;
CREATE TRIGGER search_inserted AFTER INSERT ON memories BEGIN
    DELETE FROM search_words WHERE rowid IN (SELECT key FROM search_keys WHERE id=NEW.id);
    DELETE FROM search_pending WHERE key IN (SELECT key FROM search_keys WHERE id=NEW.id);
    DELETE FROM search_keys WHERE id=NEW.id;
    INSERT INTO search_keys(id,memory_type,category,scope,project,source,status,updated_at) VALUES (NEW.id,NEW.memory_type,NEW.category,NEW.scope,NEW.project,NEW.source,NEW.status,NEW.updated_at);
    INSERT INTO search_pending(key) SELECT key FROM search_keys WHERE id=NEW.id;
    INSERT INTO search_changes(id) VALUES (NEW.id);
END
;
CREATE TRIGGER search_changed AFTER UPDATE ON memories BEGIN
    UPDATE search_keys SET id=NEW.id,memory_type=NEW.memory_type,category=NEW.category,scope=NEW.scope,project=NEW.project,source=NEW.source,status=NEW.status,updated_at=NEW.updated_at WHERE id=OLD.id;
    INSERT OR IGNORE INTO search_pending(key) SELECT key FROM search_keys WHERE id=NEW.id AND (NEW.memory_type IS NOT OLD.memory_type OR NEW.category IS NOT OLD.category OR NEW.title IS NOT OLD.title OR NEW.content IS NOT OLD.content OR NEW.context IS NOT OLD.context OR NEW.reason IS NOT OLD.reason OR NEW.evidence IS NOT OLD.evidence);
    INSERT INTO search_changes(id) VALUES (NEW.id);
END
;
CREATE TRIGGER search_deleted AFTER DELETE ON memories BEGIN
    DELETE FROM search_words WHERE rowid IN (SELECT key FROM search_keys WHERE id=OLD.id);
    DELETE FROM search_pending WHERE key IN (SELECT key FROM search_keys WHERE id=OLD.id);
    DELETE FROM search_keys WHERE id=OLD.id;
    DELETE FROM memory_vectors WHERE id=OLD.id;
    INSERT INTO search_changes(id) VALUES (OLD.id);
END
;
CREATE TRIGGER search_vector_inserted AFTER INSERT ON memory_vectors BEGIN
    INSERT INTO search_changes(id) VALUES (NEW.id);
END
;
CREATE TRIGGER search_vector_changed AFTER UPDATE ON memory_vectors BEGIN
    INSERT INTO search_changes(id) VALUES (NEW.id);
END
;
CREATE TRIGGER search_vector_deleted AFTER DELETE ON memory_vectors BEGIN
    INSERT INTO search_changes(id) VALUES (OLD.id);
END
;
