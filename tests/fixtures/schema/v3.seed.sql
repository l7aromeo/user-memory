-- Rows of every kind for a schema version 3 store: explicit, observed and
-- inferred memories; confirmed, superseded, tentative and stale; global and
-- project scope; one-line titles; conversations, a distinct pair and a
-- stored vector.
INSERT INTO memories (id,created_at,updated_at,memory_type,category,context,title,content,reason,scope,project,source,status,evidence,evidence_ids,superseded_by,last_confirmed_at) VALUES
('1a2b3c4d-1111-4aaa-8aaa-000000000001','2026-10-01 10:00:00','2026-10-02 09:00:00','preference','programming','','Prefer Rust for new tools','Prefer Rust','Memory safety','global','','explicit','confirmed','','[]',NULL,'2026-10-02 09:00:00'),
('1a2b3c4d-2222-4aaa-8aaa-000000000002','2026-10-01 10:05:00','2026-10-03 11:00:00','preference','formatting','','Indent with tabs','Indent with tabs','','project','github.com/x/y','explicit','superseded','','[]','1a2b3c4d-3333-4aaa-8aaa-000000000003',NULL),
('1a2b3c4d-3333-4aaa-8aaa-000000000003','2026-10-03 11:00:00','2026-10-03 11:00:00','preference','formatting','In this repository','Indent with four spaces in this repository','Indent with four spaces','Team style guide','project','github.com/x/y','explicit','confirmed','','[]',NULL,NULL),
('1a2b3c4d-4444-4aaa-8aaa-000000000004','2026-10-04 08:00:00','2026-10-04 08:00:00','behavior','communication','','Prefers short answers','Prefers short answers','','global','','observed','confirmed','Asked twice to shorten replies','[]',NULL,NULL),
('1a2b3c4d-5555-4aaa-8aaa-000000000005','2026-10-05 08:00:00','2026-10-05 08:00:00','pattern','communication','','Values direct, concise work','Values direct, concise work','','global','','inferred','tentative','','["1a2b3c4d-1111-4aaa-8aaa-000000000001","1a2b3c4d-4444-4aaa-8aaa-000000000004"]',NULL,NULL),
('1a2b3c4d-6666-4aaa-8aaa-000000000006','2026-10-05 09:00:00','2026-10-06 09:00:00','pattern','formatting','','Cares about consistent formatting','Cares about consistent formatting','','global','','inferred','stale','','["1a2b3c4d-2222-4aaa-8aaa-000000000002","1a2b3c4d-4444-4aaa-8aaa-000000000004"]',NULL,NULL);
INSERT INTO memory_origins (memory_id,origin,first_seen) VALUES
('1a2b3c4d-1111-4aaa-8aaa-000000000001','chat/a','2026-10-01 10:00:00'),
('1a2b3c4d-1111-4aaa-8aaa-000000000001','chat/b','2026-10-02 09:00:00'),
('1a2b3c4d-3333-4aaa-8aaa-000000000003','claude:session/x','2026-10-03 11:00:00'),
('1a2b3c4d-4444-4aaa-8aaa-000000000004','codex:thread/y','2026-10-04 08:00:00');
INSERT INTO memory_distinct (a,b) VALUES
('1a2b3c4d-1111-4aaa-8aaa-000000000001','1a2b3c4d-3333-4aaa-8aaa-000000000003');
INSERT INTO memory_vectors (id,model,dimensions,source,vector) VALUES
('1a2b3c4d-1111-4aaa-8aaa-000000000001','test-model',3,'Prefer Rust',X'0000803F0000000000000000');
