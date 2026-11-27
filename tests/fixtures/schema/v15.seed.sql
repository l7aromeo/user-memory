-- Rows of every kind for a schema version 15 store: explicit, observed and
-- inferred memories; confirmed, superseded, tentative and stale; global and
-- project scope; one-line titles; conversations, a distinct pair, a stored
-- vector, and computed embedding cutoffs, one pair separated and one not.
-- The keyword index is derived: the triggers key and queue each memory, and
-- the first write or rebuild stems them. The memories' inserts add a record
-- event each; the events below add one of every other kind: merged
-- confirmations, a change with the values it replaced, a stale pattern,
-- the tombstones of a forgotten memory and an imported event. Use rows:
-- one memory used in two sessions of a project, read in one and shown in
-- the other, and a global memory held in one (#118's columns), each with
-- its write's seq, the clock past them, and the global memory's resets.
-- A memory of each condition kind (a date, a tag with its project, a
-- text); the text one ended on an agent's evidence, which writes its
-- `ended` event, and a `reopened` event of an end since removed.
-- The store's changes: its row comes first, so the triggers count each
-- memory and link as it is added; the forgotten memory's row says when it
-- was removed, and a removal older than the pruned mark is gone.
-- Kept vector pairs: two vectors of one model, kept as a pair with the
-- floor they were compared down to; saving a vector notes it due, and the
-- first one was compared since.
INSERT INTO store_changes (id,store_id,last_change,pruned_through) VALUES (1,'0f1e2d3c4b5a69788796a5b4c3d2e1f0',40,3);
INSERT INTO memories (id,created_at,updated_at,memory_type,category,context,title,content,reason,scope,project,source,status,evidence,evidence_ids,superseded_by,last_confirmed_at) VALUES
('1a2b3c4d-1111-4aaa-8aaa-000000000001','2026-10-01 10:00:00','2026-10-02 09:00:00','preference','programming','','Prefer Rust for new tools','Prefer Rust','Memory safety','global','','explicit','confirmed','','[]',NULL,'2026-10-02 09:00:00'),
('1a2b3c4d-2222-4aaa-8aaa-000000000002','2026-10-01 10:05:00','2026-10-03 11:00:00','preference','formatting','','Indent with tabs','Indent with tabs','','project','github.com/x/y','explicit','superseded','','[]','1a2b3c4d-3333-4aaa-8aaa-000000000003',NULL),
('1a2b3c4d-3333-4aaa-8aaa-000000000003','2026-10-03 11:00:00','2026-10-03 11:00:00','preference','formatting','In this repository','Indent with four spaces in this repository','Indent with four spaces','Team style guide','project','github.com/x/y','explicit','confirmed','','[]',NULL,NULL),
('1a2b3c4d-4444-4aaa-8aaa-000000000004','2026-10-04 08:00:00','2026-10-04 08:00:00','behavior','communication','','Prefers short answers','Prefers short answers','','global','','observed','confirmed','Asked twice to shorten replies','[]',NULL,NULL),
('1a2b3c4d-5555-4aaa-8aaa-000000000005','2026-10-05 08:00:00','2026-10-05 08:00:00','pattern','communication','','Values direct, concise work','Values direct, concise work','','global','','inferred','tentative','','["1a2b3c4d-1111-4aaa-8aaa-000000000001","1a2b3c4d-4444-4aaa-8aaa-000000000004"]',NULL,NULL),
('1a2b3c4d-6666-4aaa-8aaa-000000000006','2026-10-05 09:00:00','2026-10-06 09:00:00','pattern','formatting','','Cares about consistent formatting','Cares about consistent formatting','','global','','inferred','stale','','["1a2b3c4d-2222-4aaa-8aaa-000000000002","1a2b3c4d-4444-4aaa-8aaa-000000000004"]',NULL,NULL);
INSERT INTO memories (id,created_at,updated_at,memory_type,category,context,title,content,reason,scope,project,source,status,evidence,evidence_ids,superseded_by,last_confirmed_at,until_kind,until_value,until_project) VALUES
('1a2b3c4d-8888-4aaa-8aaa-000000000008','2026-10-11 08:00:00','2026-10-11 08:00:00','preference','workflow','','Skip the slow tests for today','Skip the slow tests','','global','','explicit','confirmed','','[]',NULL,NULL,'date','2026-10-11T22:00:00Z',''),
('1a2b3c4d-9999-4aaa-8aaa-000000000009','2026-10-12 08:00:00','2026-10-12 08:00:00','preference','workflow','','Push this release without asking','Push without asking','','project','github.com/x/y','explicit','confirmed','','[]',NULL,NULL,'tag','v1.2.0','github.com/x/y'),
('1a2b3c4d-1010-4aaa-8aaa-000000000010','2026-10-13 08:00:00','2026-10-13 08:00:00','fact','tooling','','Pin the parser at 2.3','Pin the parser at 2.3','','global','','observed','confirmed','Empty rows break 2.4','[]',NULL,NULL,'text','the parser''s fix for empty rows ships','');
INSERT INTO memory_ended (memory_id,ended_at,how,evidence,origin) VALUES
('1a2b3c4d-1010-4aaa-8aaa-000000000010','2026-10-14 08:00:00','evidence','ran cargo update -p parser: 2.4.1 installed, the empty-rows test passes without the pin','claude:session/x');
INSERT INTO memory_origins (memory_id,origin,first_seen) VALUES
('1a2b3c4d-1111-4aaa-8aaa-000000000001','chat/a','2026-10-01 10:00:00'),
('1a2b3c4d-1111-4aaa-8aaa-000000000001','chat/b','2026-10-02 09:00:00'),
('1a2b3c4d-3333-4aaa-8aaa-000000000003','claude:session/x','2026-10-03 11:00:00'),
('1a2b3c4d-4444-4aaa-8aaa-000000000004','codex:thread/y','2026-10-04 08:00:00');
INSERT INTO memory_distinct (a,b) VALUES
('1a2b3c4d-1111-4aaa-8aaa-000000000001','1a2b3c4d-3333-4aaa-8aaa-000000000003');
INSERT INTO memory_vectors (id,model,dimensions,source,vector) VALUES
('1a2b3c4d-1111-4aaa-8aaa-000000000001','test-model',3,'Prefer Rust',X'0000803F0000000000000000'),
('1a2b3c4d-3333-4aaa-8aaa-000000000003','test-model',3,'Indent with four spaces',X'0000803F0000000000000000');
INSERT INTO memory_pairs (a,b,model,dimensions) VALUES
('1a2b3c4d-1111-4aaa-8aaa-000000000001','1a2b3c4d-3333-4aaa-8aaa-000000000003','test-model',3);
INSERT INTO memory_pairs_state (model,dimensions,length,floor) VALUES ('test-model',3,3,0.5);
UPDATE memory_pairs_saved SET compared=1 WHERE id='1a2b3c4d-1111-4aaa-8aaa-000000000001';
-- Projects linked by two conversations, one of them linking two.
INSERT INTO conversation_projects (origin,project,linked_at) VALUES
('claude:session/x','github.com/acme/notebooks','2026-10-17 09:00:00'),
('claude:session/x','github.com/acme/compiler','2026-10-17 09:05:00'),
('codex:thread/y','github.com/acme/compiler','2026-10-17 10:00:00');
INSERT INTO embedding_cutoffs (model,length,duplicate,related,calibrated_at) VALUES
('test-model',3,0.8,0.6,'2026-10-18 09:00:00'),
('other-model',768,NULL,NULL,'2026-10-18 09:05:00');
INSERT INTO memory_events (memory_id,at,last_at,count,op,origin,client,before,imported) VALUES
('1a2b3c4d-1111-4aaa-8aaa-000000000001','2026-10-02 09:00:00','2026-10-09 09:00:00',3,'confirm','chat/b','claude-code',NULL,NULL),
('1a2b3c4d-3333-4aaa-8aaa-000000000003','2026-10-03 11:00:00','2026-10-03 11:00:00',1,'update','claude:session/x','codex','{"content":"Indent with 2 spaces","reason":"Old guide"}',NULL),
('1a2b3c4d-2222-4aaa-8aaa-000000000002','2026-10-03 11:00:00','2026-10-03 11:00:00',1,'supersede','','cli','{"status":"confirmed"}',NULL),
('1a2b3c4d-6666-4aaa-8aaa-000000000006','2026-10-06 09:00:00','2026-10-06 09:00:00',1,'stale','','','{"status":"tentative"}',NULL),
('1a2b3c4d-7777-4aaa-8aaa-000000000007','2026-10-07 09:00:00','2026-10-07 09:00:00',1,'record','','',NULL,NULL),
('1a2b3c4d-7777-4aaa-8aaa-000000000007','2026-10-08 09:00:00','2026-10-08 09:00:00',1,'forget','','cli',NULL,NULL),
('1a2b3c4d-4444-4aaa-8aaa-000000000004','2026-09-30 08:00:00','2026-09-30 08:00:00',1,'update','','host:codex','{"evidence":"Asked once"}','2026-10-10 08:00:00'),
('1a2b3c4d-9999-4aaa-8aaa-000000000009','2026-10-12 09:00:00','2026-10-12 09:00:00',1,'reopened','chat/a','cli','{"until":{"kind":"tag","value":"v1.1.0","project":"github.com/x/y"},"ended_at":"2026-10-12 08:30:00","how":"tag","evidence":"tag v1.1.0 at 1a2b3c4 in a checkout of github.com/x/y","origin":""}',NULL);
INSERT INTO memory_changes (memory_id,change,removed_at) VALUES ('1a2b3c4d-7777-4aaa-8aaa-000000000007',7,'2026-10-08 09:00:00');
INSERT INTO memory_use (memory,project,origin,at,read,recorded,shown,held,overridden,passage,seq) VALUES
('1a2b3c4d-3333-4aaa-8aaa-000000000003','github.com/x/y','claude:session/x','2026-10-20 09:00:00',1,0,0,NULL,NULL,NULL,1),
('1a2b3c4d-3333-4aaa-8aaa-000000000003','github.com/x/y','codex:thread/y','2026-10-21 09:00:00',0,0,1,NULL,NULL,2,3),
('1a2b3c4d-1111-4aaa-8aaa-000000000001','github.com/x/y','codex:thread/y','2026-10-21 09:00:00',0,1,1,1,0,1,2);
INSERT INTO use_clock (id,seq) VALUES (1,3);
INSERT INTO hold_reset (memory,seq,passages) VALUES ('1a2b3c4d-1111-4aaa-8aaa-000000000001',1,2);
