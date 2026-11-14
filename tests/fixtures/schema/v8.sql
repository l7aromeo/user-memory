-- Schema version 8 of a fresh user-memory store: sqlite_master in
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
CREATE TABLE memory_events (
    seq INTEGER PRIMARY KEY,
    memory_id TEXT NOT NULL,
    at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    count INTEGER NOT NULL DEFAULT 1 CHECK(count >= 1),
    op TEXT NOT NULL CHECK(op IN ('record','confirm','update','supersede','merge','forget','import','revive','distinct','stale','move','origin_removed')),
    origin TEXT NOT NULL DEFAULT '',
    client TEXT NOT NULL DEFAULT '',
    before TEXT CHECK(before IS NULL OR json_valid(before)),
    imported TEXT
)
;
CREATE INDEX memory_events_by_memory ON memory_events(memory_id, seq)
;
CREATE TABLE memory_writer (
    id INTEGER PRIMARY KEY CHECK(id=1),
    client TEXT NOT NULL DEFAULT '',
    op TEXT,
    subject TEXT,
    origin TEXT NOT NULL DEFAULT ''
)
;
CREATE TRIGGER memory_event_recorded AFTER INSERT ON memories BEGIN
    INSERT INTO memory_events(memory_id,op,origin,client)
    VALUES (NEW.id,
        CASE (SELECT op FROM memory_writer WHERE id=1) WHEN 'merge' THEN 'merge' WHEN 'import' THEN 'import' ELSE 'record' END,
        coalesce((SELECT origin FROM memory_writer WHERE id=1),''),
        coalesce((SELECT client FROM memory_writer WHERE id=1),''));
END
;
CREATE TRIGGER memory_event_changed AFTER UPDATE ON memories BEGIN
    UPDATE memory_events SET last_at=CURRENT_TIMESTAMP,count=count+1,
        origin=coalesce(nullif((SELECT origin FROM memory_writer WHERE id=1),''),origin),
        client=coalesce((SELECT client FROM memory_writer WHERE id=1),'')
    WHERE seq=(SELECT max(seq) FROM memory_events WHERE memory_id=NEW.id) AND op='confirm' AND imported IS NULL
    AND (OLD.memory_type,OLD.category,OLD.context,OLD.title,OLD.content,OLD.reason,OLD.scope,OLD.project,OLD.source,OLD.status,OLD.evidence,OLD.evidence_ids,OLD.superseded_by)
        IS (NEW.memory_type,NEW.category,NEW.context,NEW.title,NEW.content,NEW.reason,NEW.scope,NEW.project,NEW.source,NEW.status,NEW.evidence,NEW.evidence_ids,NEW.superseded_by)
    AND (OLD.last_confirmed_at IS NOT NEW.last_confirmed_at OR EXISTS (SELECT 1 FROM memory_writer WHERE id=1 AND op IN ('update','confirm') AND subject=NEW.id));
    INSERT INTO memory_events(memory_id,op,origin,client,before)
    SELECT NEW.id,
        CASE
            WHEN (OLD.memory_type,OLD.category,OLD.context,OLD.title,OLD.content,OLD.reason,OLD.scope,OLD.project,OLD.source,OLD.status,OLD.evidence,OLD.evidence_ids,OLD.superseded_by)
                IS (NEW.memory_type,NEW.category,NEW.context,NEW.title,NEW.content,NEW.reason,NEW.scope,NEW.project,NEW.source,NEW.status,NEW.evidence,NEW.evidence_ids,NEW.superseded_by) THEN 'confirm'
            WHEN NEW.status='stale' AND (OLD.memory_type,OLD.category,OLD.context,OLD.title,OLD.content,OLD.reason,OLD.scope,OLD.project,OLD.source,OLD.evidence,OLD.evidence_ids,OLD.superseded_by)
                IS (NEW.memory_type,NEW.category,NEW.context,NEW.title,NEW.content,NEW.reason,NEW.scope,NEW.project,NEW.source,NEW.evidence,NEW.evidence_ids,NEW.superseded_by) THEN 'stale'
            WHEN (SELECT op FROM memory_writer WHERE id=1)='import' THEN 'import'
            WHEN NEW.status='superseded' AND OLD.status IS NOT 'superseded' THEN 'supersede'
            WHEN OLD.status='superseded' AND NEW.status IS NOT 'superseded' THEN 'revive'
            WHEN (OLD.memory_type,OLD.category,OLD.context,OLD.title,OLD.content,OLD.reason,OLD.source,OLD.status,OLD.evidence,OLD.evidence_ids,OLD.superseded_by)
                IS (NEW.memory_type,NEW.category,NEW.context,NEW.title,NEW.content,NEW.reason,NEW.source,NEW.status,NEW.evidence,NEW.evidence_ids,NEW.superseded_by)
                AND NOT EXISTS (SELECT 1 FROM memory_writer WHERE id=1 AND op='update' AND subject=NEW.id) THEN 'move'
            ELSE 'update'
        END,
        coalesce((SELECT origin FROM memory_writer WHERE id=1),''),
        coalesce((SELECT client FROM memory_writer WHERE id=1),''),
        CASE WHEN (OLD.memory_type,OLD.category,OLD.context,OLD.title,OLD.content,OLD.reason,OLD.scope,OLD.project,OLD.source,OLD.status,OLD.evidence,OLD.evidence_ids,OLD.superseded_by)
            IS (NEW.memory_type,NEW.category,NEW.context,NEW.title,NEW.content,NEW.reason,NEW.scope,NEW.project,NEW.source,NEW.status,NEW.evidence,NEW.evidence_ids,NEW.superseded_by) THEN NULL
        ELSE json_remove(json_object(
            'type',OLD.memory_type,'category',OLD.category,'context',OLD.context,'title',OLD.title,
            'content',OLD.content,'reason',OLD.reason,'scope',OLD.scope,'project',OLD.project,
            'source',OLD.source,'status',OLD.status,'evidence',OLD.evidence,
            'evidence_ids',CASE WHEN json_valid(OLD.evidence_ids) THEN json(OLD.evidence_ids) ELSE OLD.evidence_ids END,
            'superseded_by',OLD.superseded_by),
            CASE WHEN OLD.memory_type IS NEW.memory_type THEN '$.type' ELSE '$._' END,
            CASE WHEN OLD.category IS NEW.category THEN '$.category' ELSE '$._' END,
            CASE WHEN OLD.context IS NEW.context THEN '$.context' ELSE '$._' END,
            CASE WHEN OLD.title IS NEW.title THEN '$.title' ELSE '$._' END,
            CASE WHEN OLD.content IS NEW.content THEN '$.content' ELSE '$._' END,
            CASE WHEN OLD.reason IS NEW.reason THEN '$.reason' ELSE '$._' END,
            CASE WHEN OLD.scope IS NEW.scope THEN '$.scope' ELSE '$._' END,
            CASE WHEN OLD.project IS NEW.project THEN '$.project' ELSE '$._' END,
            CASE WHEN OLD.source IS NEW.source THEN '$.source' ELSE '$._' END,
            CASE WHEN OLD.status IS NEW.status THEN '$.status' ELSE '$._' END,
            CASE WHEN OLD.evidence IS NEW.evidence THEN '$.evidence' ELSE '$._' END,
            CASE WHEN OLD.evidence_ids IS NEW.evidence_ids THEN '$.evidence_ids' ELSE '$._' END,
            CASE WHEN OLD.superseded_by IS NEW.superseded_by THEN '$.superseded_by' ELSE '$._' END)
        END
    WHERE (OLD.memory_type,OLD.category,OLD.context,OLD.title,OLD.content,OLD.reason,OLD.scope,OLD.project,OLD.source,OLD.status,OLD.evidence,OLD.evidence_ids,OLD.superseded_by)
        IS NOT (NEW.memory_type,NEW.category,NEW.context,NEW.title,NEW.content,NEW.reason,NEW.scope,NEW.project,NEW.source,NEW.status,NEW.evidence,NEW.evidence_ids,NEW.superseded_by)
    OR ((OLD.last_confirmed_at IS NOT NEW.last_confirmed_at OR EXISTS (SELECT 1 FROM memory_writer WHERE id=1 AND op IN ('update','confirm') AND subject=NEW.id))
        AND NOT EXISTS (SELECT 1 FROM memory_events WHERE seq=(SELECT max(seq) FROM memory_events WHERE memory_id=NEW.id) AND op='confirm' AND imported IS NULL));
END
;
CREATE TRIGGER memory_event_forgotten AFTER DELETE ON memories BEGIN
    UPDATE memory_events SET origin='',client='',before=NULL,imported=NULL WHERE memory_id=OLD.id;
    INSERT INTO memory_events(memory_id,op,client) VALUES (OLD.id,'forget',coalesce((SELECT client FROM memory_writer WHERE id=1),''));
END
;
CREATE TRIGGER memory_event_origin_removed AFTER DELETE ON memory_origins
WHEN EXISTS (SELECT 1 FROM memories WHERE id=OLD.memory_id) BEGIN
    INSERT INTO memory_events(memory_id,op,client) VALUES (OLD.memory_id,'origin_removed',coalesce((SELECT client FROM memory_writer WHERE id=1),''));
END
;
CREATE TRIGGER memory_event_distinct AFTER INSERT ON memory_distinct BEGIN
    INSERT INTO memory_events(memory_id,op,origin,client)
    SELECT value,'distinct',coalesce((SELECT origin FROM memory_writer WHERE id=1),''),coalesce((SELECT client FROM memory_writer WHERE id=1),'')
    FROM (SELECT NEW.a AS value UNION ALL SELECT NEW.b);
END
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
CREATE VIRTUAL TABLE search_words USING fts5(category,title,type,body,detail=column,tokenize='ascii')
;
