use std::sync::Arc;

use zlogic_core::SharedStore;
use zlogic_engine::service::{SessionService, WorkspaceService};
use zlogic_engine::{Sessions, Workspaces};
use zlogic_objects::{MemoryObjectStore, ObjectStore};
use zlogic_protocol::message::{ContentPart, TextPart};
use zlogic_protocol::query::{
    SessionListReq, SessionOpenReq, SessionRenameReq, SessionSearchReq, TranscriptKind,
    TranscriptReq, WorkspaceSelector,
};
use zlogic_protocol::{EntryId, SessionId, TurnId};
use zlogic_store::{Db, EntryKind, NewEntry};

struct Rig {
    sessions: Sessions,
    store: SharedStore,
    work: tempfile::TempDir,
}

impl Rig {
    fn new() -> Self {
        let work = tempfile::tempdir().unwrap();
        let store = SharedStore::new(Db::open_in_memory().unwrap());
        let workspaces = Arc::new(Workspaces::new(store.clone()));
        store
            .with(|db| db.workspaces().resolve(work.path()))
            .unwrap();
        let sessions = Sessions::new(
            store.clone(),
            Arc::new(MemoryObjectStore::new()),
            workspaces as Arc<dyn WorkspaceService>,
            work.path().join("attachments"),
        );
        Self {
            sessions,
            store,
            work,
        }
    }

    fn sel(&self) -> WorkspaceSelector {
        WorkspaceSelector::Path {
            root: self.work.path().to_string_lossy().into_owned(),
        }
    }

    async fn open_new(&self) -> SessionId {
        self.sessions
            .open(SessionOpenReq {
                workspace: self.sel(),
                session_id: None,
            })
            .await
            .unwrap()
            .session
            .session_id
    }

    fn append_round_text(&self, session: SessionId, turn_seq: i64, text: &str) {
        let part = ContentPart::Text(TextPart {
            text: text.into(),
            raw: None,
            truncated: false,
        });
        self.store.with(|db| {
            let turn = db
                .entries()
                .list(session)
                .unwrap()
                .into_iter()
                .find(|e| e.turn_seq == turn_seq)
                .map(|e| e.turn_id)
                .expect("the turn exists");
            db.entries()
                .append(NewEntry::new(
                    session,
                    turn,
                    turn_seq,
                    EntryKind::AssistantText,
                    serde_json::to_value(part).unwrap(),
                ))
                .unwrap();
        });
    }

    fn say(&self, session: SessionId, turn_seq: i64, user: &str, reply: &str) {
        let turn = TurnId::new();
        let text = |t: &str| {
            serde_json::to_value(ContentPart::Text(TextPart {
                text: t.into(),
                raw: None,
                truncated: false,
            }))
            .unwrap()
        };
        self.store.with(|db| {
            db.entries()
                .append(NewEntry::new(
                    session,
                    turn,
                    turn_seq,
                    EntryKind::User,
                    text(user),
                ))
                .unwrap();
            db.entries()
                .append(NewEntry::new(
                    session,
                    turn,
                    turn_seq,
                    EntryKind::AssistantText,
                    text(reply),
                ))
                .unwrap();
            db.entries()
                .append_turn_end(
                    NewEntry::new(
                        session,
                        turn,
                        turn_seq,
                        EntryKind::Event,
                        serde_json::json!({ "type": "turn_end", "status": "completed" }),
                    ),
                    &zlogic_objects::MemoryObjectStore::default(),
                )
                .unwrap();
        });
    }

    fn entry_at(&self, session: SessionId, turn_seq: i64, turn_id: TurnId, at: &str) {
        self.store.with(|db| {
            db.conn()
                .execute(
                    "INSERT INTO session_entry
                        (entry_id, session_id, seq, turn_seq, turn_id, kind, data, created_at)
                     VALUES (?1, ?2,
                             (SELECT COALESCE(MAX(seq), 0) + 1 FROM session_entry
                               WHERE session_id = ?2),
                             ?3, ?4, 'user', 'null', ?5)",
                    rusqlite::params![
                        EntryId::new().to_string(),
                        session.to_string(),
                        turn_seq,
                        turn_id.to_string(),
                        at,
                    ],
                )
                .unwrap();
            // Raw insert bypasses [`EntryStore::append`]'s incremental maintenance; rebuild the
            // derived `session` columns so the list reads the live-turn-excluding key the test pins.
            db.entries().recompute_stats(session).unwrap();
        });
    }

    async fn list_ids(&self) -> Vec<SessionId> {
        self.sessions
            .list(SessionListReq {
                workspace: self.sel(),
                include_sub_agents: false,
                include_archived: false,
                limit: None,
                offset: None,
            })
            .await
            .unwrap()
            .items
            .iter()
            .map(|s| s.session_id)
            .collect()
    }
}

fn parse_ts(at: &str) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(at)
        .unwrap()
        .with_timezone(&chrono::Utc)
}

// ── open ──

#[tokio::test]
async fn opening_without_an_id_creates_a_session_and_returns_one_snapshot() {
    let rig = Rig::new();
    let opened = rig
        .sessions
        .open(SessionOpenReq {
            workspace: rig.sel(),
            session_id: None,
        })
        .await
        .unwrap();

    assert!(opened.session.is_root());
    assert_eq!(opened.session.turn_count, 0);
    assert_eq!(
        opened.last_turn_id, None,
        "a new session has no history, and no last_turn"
    );
    assert_eq!(opened.turn, None, "a new session has no live turn");
    assert!(opened.pending_submissions.is_empty());
    assert_eq!(opened.exec_cwd, opened.workspace.root);
    assert_eq!(opened.session.workspace_id, opened.workspace.workspace_id);
}

#[tokio::test]
async fn opening_an_existing_session_reports_its_last_turn() {
    let rig = Rig::new();
    let id = rig.open_new().await;
    rig.say(id, 1, "first question", "first answer");
    rig.say(id, 2, "second question", "second answer");

    let opened = rig
        .sessions
        .open(SessionOpenReq {
            workspace: rig.sel(),
            session_id: Some(id),
        })
        .await
        .unwrap();

    assert_eq!(opened.session.session_id, id);
    assert_eq!(opened.session.turn_count, 2);
    let last_turn_id = rig
        .store
        .with(|db| db.entries().list(id))
        .unwrap()
        .last()
        .map(|row| row.turn_id);
    assert_eq!(opened.last_turn_id, last_turn_id);
}

#[tokio::test]
async fn opening_a_session_through_the_wrong_workspace_is_refused() {
    let rig = Rig::new();
    let id = rig.open_new().await;
    let other = tempfile::tempdir().unwrap();
    rig.store
        .with(|db| db.workspaces().resolve(other.path()))
        .unwrap();

    let err = rig
        .sessions
        .open(SessionOpenReq {
            workspace: WorkspaceSelector::Path {
                root: other.path().to_string_lossy().into_owned(),
            },
            session_id: Some(id),
        })
        .await
        .unwrap_err();
    assert_eq!(
        err.category,
        zlogic_protocol::ErrorCategory::InvalidArgument,
        "{err:?}"
    );
}

#[tokio::test]
async fn opening_an_unknown_session_is_not_found() {
    let rig = Rig::new();
    let err = rig
        .sessions
        .open(SessionOpenReq {
            workspace: rig.sel(),
            session_id: Some(SessionId::new()),
        })
        .await
        .unwrap_err();
    assert_eq!(
        err.category,
        zlogic_protocol::ErrorCategory::NotFound,
        "{err:?}"
    );
}

#[tokio::test]
async fn without_a_router_the_model_selection_says_nothing_is_configured() {
    let rig = Rig::new();
    let opened = rig
        .sessions
        .open(SessionOpenReq {
            workspace: rig.sel(),
            session_id: None,
        })
        .await
        .unwrap();

    assert!(!opened.model.models_configured);
    assert!(!opened.model.has_usable_credential);
    assert_eq!(
        opened.model.source,
        zlogic_protocol::query::ModelSelectionSource::None
    );
}

#[tokio::test]
async fn set_effort_validates_round_trips_and_survives_reopen() {
    let rig = Rig::new();
    let session = rig.open_new().await;

    let err = rig
        .sessions
        .set_effort(session, "turbo".into())
        .await
        .unwrap_err();
    assert_eq!(err.code, "session_effort_unknown");

    let summary = rig
        .sessions
        .set_effort(session, "high".into())
        .await
        .unwrap();
    assert_eq!(summary.effort, Some(zlogic_protocol::Effort::High));

    let summary = rig
        .sessions
        .set_effort(session, "low".into())
        .await
        .unwrap();
    assert_eq!(summary.effort, Some(zlogic_protocol::Effort::Low));

    let reopened = rig
        .sessions
        .open(SessionOpenReq {
            workspace: rig.sel(),
            session_id: Some(session),
        })
        .await
        .unwrap();
    assert_eq!(reopened.session.effort, Some(zlogic_protocol::Effort::Low));
}

// ── list ──

#[tokio::test]
async fn the_list_is_ordered_by_the_last_message_not_by_updated_at() {
    let rig = Rig::new();
    let old = rig.open_new().await;
    rig.say(old, 1, "long ago", "answer");
    let fresh = rig.open_new().await;
    rig.say(fresh, 1, "just now", "answer");

    rig.sessions
        .open(SessionOpenReq {
            workspace: rig.sel(),
            session_id: Some(old),
        })
        .await
        .unwrap();

    let page = rig
        .sessions
        .list(SessionListReq {
            workspace: rig.sel(),
            include_sub_agents: false,
            include_archived: false,
            limit: None,
            offset: None,
        })
        .await
        .unwrap();

    let ids: Vec<_> = page.items.iter().map(|s| s.session_id).collect();
    assert_eq!(
        ids.first(),
        Some(&fresh),
        "the one with a new message is the one that belongs at the front: {ids:?}"
    );
}

#[tokio::test]
async fn a_live_session_keeps_its_position_until_its_turn_completes() {
    let rig = Rig::new();

    let old = rig.open_new().await; // idle, T1 finished
    rig.entry_at(old, 1, TurnId::new(), "2026-06-01T08:00:00Z");
    let live_a = rig.open_new().await; // live, previous round T2
    rig.entry_at(live_a, 1, TurnId::new(), "2026-06-01T09:00:00Z");
    let live_b = rig.open_new().await; // live, previous round T3 (newest in the live group)
    rig.entry_at(live_b, 1, TurnId::new(), "2026-06-01T11:00:00Z");
    let idle_fresh = rig.open_new().await; // idle, T4 (the most recent idle session)
    rig.entry_at(idle_fresh, 1, TurnId::new(), "2026-06-01T12:00:00Z");

    let live_a_turn = TurnId::new();
    let live_b_turn = TurnId::new();
    rig.store.with(|db| {
        db.locks()
            .acquire(live_a, live_a_turn, Some("desktop"))
            .unwrap();
        db.locks()
            .acquire(live_b, live_b_turn, Some("desktop"))
            .unwrap();
    });
    let live_a_msg_at = "2026-06-01T13:00:00Z";
    let live_b_msg_at = "2026-06-01T14:00:00Z";
    rig.entry_at(live_a, 2, live_a_turn, live_a_msg_at);
    rig.entry_at(live_b, 2, live_b_turn, live_b_msg_at);

    assert_eq!(
        rig.list_ids().await,
        vec![live_b, live_a, idle_fresh, old],
        "live first; inside the live group, newest round completion first; the idle group by last message, newest first"
    );

    let list = rig
        .sessions
        .list(SessionListReq {
            workspace: rig.sel(),
            include_sub_agents: false,
            include_archived: false,
            limit: None,
            offset: None,
        })
        .await
        .unwrap();
    let by_id = |id: SessionId| list.items.iter().find(|s| s.session_id == id).unwrap();
    assert_eq!(
        by_id(live_b)
            .last_message_at
            .map(|t| t.with_timezone(&chrono::Utc)),
        Some(parse_ts("2026-06-01T11:00:00Z")),
        "a live session's key is when its previous round completed"
    );
    assert_eq!(
        by_id(live_a)
            .last_message_at
            .map(|t| t.with_timezone(&chrono::Utc)),
        Some(parse_ts("2026-06-01T09:00:00Z")),
    );
    assert_eq!(
        by_id(idle_fresh)
            .last_message_at
            .map(|t| t.with_timezone(&chrono::Utc)),
        Some(parse_ts("2026-06-01T12:00:00Z")),
    );

    let holder = rig
        .store
        .with(|db| db.locks().get(live_b).unwrap().unwrap().holder_id);
    rig.store
        .with(|db| db.locks().release(live_b, holder).unwrap());
    assert_eq!(
        rig.list_ids().await,
        vec![live_a, live_b, idle_fresh, old],
        "live_a (still live) goes to the top; live_b joins the idle group at the front, by its round-completion time T6"
    );

    let holder = rig
        .store
        .with(|db| db.locks().get(live_a).unwrap().unwrap().holder_id);
    rig.store
        .with(|db| db.locks().release(live_a, holder).unwrap());
    assert_eq!(
        rig.list_ids().await,
        vec![live_b, live_a, idle_fresh, old],
        "once all are idle, by last-message time, newest first: live_b(T6) > live_a(T5) > idle_fresh(T4)"
    );
    let list = rig
        .sessions
        .list(SessionListReq {
            workspace: rig.sel(),
            include_sub_agents: false,
            include_archived: false,
            limit: None,
            offset: None,
        })
        .await
        .unwrap();
    let by_id = |id: SessionId| list.items.iter().find(|s| s.session_id == id).unwrap();
    assert_eq!(
        by_id(live_a)
            .last_message_at
            .map(|t| t.with_timezone(&chrono::Utc)),
        Some(parse_ts("2026-06-01T13:00:00Z")),
        "after the release, live_a's key moves back to the message time of its round, T5"
    );
    assert_eq!(
        by_id(live_b)
            .last_message_at
            .map(|t| t.with_timezone(&chrono::Utc)),
        Some(parse_ts("2026-06-01T14:00:00Z")),
        "live_b's key moves back to T6"
    );
}

#[tokio::test]
async fn sub_agent_transcripts_are_hidden_unless_asked_for() {
    let rig = Rig::new();
    let parent = rig.open_new().await;
    rig.store.with(|db| {
        db.sessions()
            .create(zlogic_store::NewSession::child(parent, "researcher"))
            .unwrap()
    });

    let req = |include: bool| SessionListReq {
        workspace: rig.sel(),
        include_sub_agents: include,
        include_archived: false,
        limit: None,
        offset: None,
    };
    assert_eq!(rig.sessions.list(req(false)).await.unwrap().total, 1);
    assert_eq!(rig.sessions.list(req(true)).await.unwrap().total, 2);
}

#[tokio::test]
async fn paging_reports_the_filtered_total_not_the_page_size() {
    let rig = Rig::new();
    for i in 0..5 {
        let id = rig.open_new().await;
        rig.say(id, 1, &format!("number {i}"), "answer");
    }

    let page = rig
        .sessions
        .list(SessionListReq {
            workspace: rig.sel(),
            include_sub_agents: false,
            include_archived: false,
            limit: Some(2),
            offset: Some(1),
        })
        .await
        .unwrap();

    assert_eq!(page.items.len(), 2);
    assert_eq!(page.total, 5);
}

#[tokio::test]
async fn archived_sessions_are_hidden_unless_asked_for() {
    let rig = Rig::new();
    let id = rig.open_new().await;
    rig.store.with(|db| db.sessions().archive(id).unwrap());

    let req = |include: bool| SessionListReq {
        workspace: rig.sel(),
        include_sub_agents: false,
        include_archived: include,
        limit: None,
        offset: None,
    };
    assert_eq!(rig.sessions.list(req(false)).await.unwrap().total, 0);
    assert_eq!(rig.sessions.list(req(true)).await.unwrap().total, 1);
}

#[tokio::test]
async fn every_row_gets_its_own_counts() {
    let rig = Rig::new();
    let a = rig.open_new().await;
    rig.say(a, 1, "one", "reply");
    rig.say(a, 2, "two", "reply");
    let b = rig.open_new().await;
    rig.say(b, 1, "only one round", "reply");

    let page = rig
        .sessions
        .list(SessionListReq {
            workspace: rig.sel(),
            include_sub_agents: false,
            include_archived: false,
            limit: None,
            offset: None,
        })
        .await
        .unwrap();

    let counts: std::collections::HashMap<_, _> = page
        .items
        .iter()
        .map(|s| (s.session_id, s.turn_count))
        .collect();
    assert_eq!(counts[&a], 2);
    assert_eq!(counts[&b], 1);
}

// ── rename ──

#[tokio::test]
async fn renaming_sets_a_protected_user_title() {
    let rig = Rig::new();
    let id = rig.open_new().await;

    let s = rig
        .sessions
        .rename(SessionRenameReq {
            session_id: id,
            title: "  refactor the snapshot subsystem  ".into(),
        })
        .await
        .unwrap();
    assert_eq!(
        s.title.as_deref(),
        Some("refactor the snapshot subsystem"),
        "the whitespace at both ends should be stripped"
    );
    assert_eq!(
        s.title_source,
        Some(zlogic_protocol::query::TitleSource::User)
    );

    let replaced = rig
        .store
        .with(|db| {
            db.sessions().set_title(
                id,
                "the name the model made up",
                zlogic_store::TitleSource::Model,
            )
        })
        .unwrap();
    assert!(!replaced, "the User source is protected");
}

#[tokio::test]
async fn renaming_to_empty_hands_the_title_back_to_the_automatic_path() {
    let rig = Rig::new();
    let id = rig.open_new().await;
    rig.sessions
        .rename(SessionRenameReq {
            session_id: id,
            title: "the one I made up".into(),
        })
        .await
        .unwrap();

    let cleared = rig
        .sessions
        .rename(SessionRenameReq {
            session_id: id,
            title: "   ".into(),
        })
        .await
        .unwrap();
    assert_eq!(cleared.title, None);
    assert_eq!(
        cleared.title_source, None,
        "the source has to go back to empty along with it"
    );

    let replaced = rig
        .store
        .with(|db| {
            db.sessions()
                .set_title(id, "automatic title", zlogic_store::TitleSource::Draft)
        })
        .unwrap();
    assert!(replaced);
}

// ── delete ──

#[tokio::test]
async fn deleting_removes_the_session_and_its_entries() {
    let rig = Rig::new();
    let id = rig.open_new().await;
    rig.say(id, 1, "said something", "answer");
    let attachments = rig.work.path().join("attachments").join(id.to_string());
    std::fs::create_dir_all(&attachments).unwrap();
    std::fs::write(attachments.join("data.csv"), b"a,b\n1,2\n").unwrap();

    rig.sessions.delete(id).await.unwrap();

    assert!(
        rig.store
            .with(|db| db.sessions().find(id))
            .unwrap()
            .is_none()
    );
    assert!(
        rig.store
            .with(|db| db.entries().list(id))
            .unwrap()
            .is_empty()
    );
    assert!(!attachments.exists());
}

#[tokio::test]
async fn deleting_an_unknown_session_is_not_found() {
    let rig = Rig::new();
    let err = rig.sessions.delete(SessionId::new()).await.unwrap_err();
    assert_eq!(
        err.category,
        zlogic_protocol::ErrorCategory::NotFound,
        "{err:?}"
    );
}

#[tokio::test]
async fn deleting_a_session_keeps_its_usage_records() {
    let rig = Rig::new();
    let id = rig.open_new().await;
    rig.store.with(|db| {
        db.usage()
            .record(zlogic_store::NewUsage::new(
                id,
                zlogic_store::Purpose::Main,
                zlogic_protocol::usage::TokenUsage {
                    input: 1_000,
                    output: 200,
                    ..Default::default()
                },
            ))
            .unwrap()
    });

    rig.sessions.delete(id).await.unwrap();

    let kept = rig
        .store
        .with(|db| db.usage().total_for_session(id))
        .unwrap();
    assert_eq!(
        kept.input, 1_000,
        "usage has to survive deleting the session"
    );
    assert_eq!(kept.output, 200);
}

// ── transcript ──

#[tokio::test]
async fn the_transcript_can_be_fetched_incrementally() {
    let rig = Rig::new();
    let id = rig.open_new().await;
    rig.say(id, 1, "one", "reply one");
    rig.say(id, 2, "two", "reply two");
    rig.say(id, 3, "three", "reply three");

    let page = rig
        .sessions
        .transcript(TranscriptReq {
            session_id: id,
            after_turn_seq: Some(1),
            offset: None,
            limit: None,
        })
        .await
        .unwrap();

    assert_eq!(page.items.len(), 6, "turns 2 and 3");
    assert_eq!(page.total, 2, "total is the number of rounds");
    assert!(page.items.iter().all(|e| e.turn_seq > 1));
}

#[tokio::test]
async fn a_limited_transcript_keeps_the_most_recent_turns() {
    let rig = Rig::new();
    let id = rig.open_new().await;
    for i in 1..=5 {
        rig.say(id, i, &format!("question {i}"), &format!("answer {i}"));
    }

    let page = rig
        .sessions
        .transcript(TranscriptReq {
            session_id: id,
            after_turn_seq: None,
            offset: None,
            limit: Some(2),
        })
        .await
        .unwrap();

    assert_eq!(
        page.items.len(),
        6,
        "the last 2 rounds × 3 rows each = 6 entries"
    );
    assert_eq!(
        page.total, 5,
        "total is the number of rounds after filtering, not the number of rows on this page"
    );
    let mut turn_seqs: Vec<u32> = page.items.iter().map(|e| e.turn_seq).collect();
    turn_seqs.sort_unstable();
    turn_seqs.dedup();
    assert_eq!(turn_seqs, vec![4, 5], "the cut is by round, not by entry");
    assert_eq!(
        page.items.last().unwrap().turn_seq,
        5,
        "the last entry has to be the newest one"
    );
}

#[tokio::test]
async fn the_transcript_pages_back_from_the_tail() {
    let rig = Rig::new();
    let id = rig.open_new().await;
    for i in 1..=5 {
        rig.say(id, i, &format!("question {i}"), &format!("answer {i}"));
    }

    let page_of = |offset: u32| {
        rig.sessions.transcript(TranscriptReq {
            session_id: id,
            after_turn_seq: None,
            offset: Some(offset),
            limit: Some(4),
        })
    };

    let first = page_of(0).await.unwrap();
    assert_eq!(first.items.len(), 12, "4 rounds × 3 rows");
    assert_eq!(first.total, 5, "total is the number of rounds");
    let mut first_turns: Vec<u32> = first.items.iter().map(|e| e.turn_seq).collect();
    first_turns.sort_unstable();
    first_turns.dedup();
    assert_eq!(
        first_turns,
        vec![2, 3, 4, 5],
        "one page is exactly 4 whole rounds"
    );

    let second = page_of(4).await.unwrap();
    assert_eq!(second.items.len(), 3);
    assert_eq!(second.total, 5);
    assert!(second.items.iter().all(|e| e.turn_seq == 1));

    let past = page_of(8).await.unwrap();
    assert!(past.items.is_empty());
    assert_eq!(past.total, 5);
}

#[tokio::test]
async fn asking_for_an_unknown_sessions_transcript_is_not_found() {
    let rig = Rig::new();
    let err = rig
        .sessions
        .transcript(TranscriptReq {
            session_id: SessionId::new(),
            after_turn_seq: None,
            offset: None,
            limit: None,
        })
        .await
        .unwrap_err();
    assert_eq!(
        err.category,
        zlogic_protocol::ErrorCategory::NotFound,
        "{err:?}"
    );
}

// ── search ──

#[tokio::test]
async fn search_finds_the_turn_and_reports_a_snippet() {
    let rig = Rig::new();
    let id = rig.open_new().await;
    rig.say(id, 1, "let's talk about something else first", "ok");
    rig.say(
        id,
        2,
        "help me refactor the snapshot subsystem",
        "let me take a look",
    );

    let hits = rig
        .sessions
        .search(SessionSearchReq {
            workspace: Some(rig.sel()),
            session_id: None,
            query: "refactor".into(),
            limit: None,
        })
        .await
        .unwrap();

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].turn_seq, 2);
    assert_eq!(hits[0].kind, TranscriptKind::User);
    assert!(hits[0].snippet.contains("refactor"), "{}", hits[0].snippet);
    assert_eq!(hits[0].session.session_id, id);
}

#[tokio::test]
async fn search_can_scope_to_one_session() {
    let rig = Rig::new();
    let first = rig.open_new().await;
    let second = rig.open_new().await;
    rig.say(first, 1, "both sessions have this keyword", "the first one");
    rig.say(
        second,
        1,
        "both sessions have this keyword",
        "the second one",
    );

    let hits = rig
        .sessions
        .search(SessionSearchReq {
            workspace: Some(rig.sel()),
            session_id: Some(second),
            query: "keyword".into(),
            limit: None,
        })
        .await
        .unwrap();

    assert!(!hits.is_empty());
    assert!(hits.iter().all(|hit| hit.session.session_id == second));
}

#[tokio::test]
async fn search_is_case_insensitive_and_covers_replies() {
    let rig = Rig::new();
    let id = rig.open_new().await;
    rig.say(id, 1, "question", "LibGit2 will do");

    let hits = rig
        .sessions
        .search(SessionSearchReq {
            workspace: None,
            session_id: None,
            query: "libgit2".into(),
            limit: None,
        })
        .await
        .unwrap();

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].kind, TranscriptKind::Text);
}

#[tokio::test]
async fn search_skips_intermediate_round_text_and_still_finds_the_question() {
    let rig = Rig::new();
    let id = rig.open_new().await;
    rig.say(id, 1, "do something for me", "let me take a look"); // the final answer
    rig.append_round_text(
        id,
        1,
        "narration mentioning a sailboat ⛵ in the middle round",
    );

    let mid = rig
        .sessions
        .search(SessionSearchReq {
            workspace: None,
            session_id: None,
            query: "sailboat".into(),
            limit: None,
        })
        .await
        .unwrap();
    assert!(
        mid.is_empty(),
        "middle-round text must not show up in search results: {mid:?}"
    );

    let asked = rig
        .sessions
        .search(SessionSearchReq {
            workspace: None,
            session_id: None,
            query: "do something for me".into(),
            limit: None,
        })
        .await
        .unwrap();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].kind, TranscriptKind::User);
}

#[tokio::test]
async fn an_empty_query_matches_nothing() {
    let rig = Rig::new();
    let id = rig.open_new().await;
    rig.say(id, 1, "has content", "has content");

    for q in ["", "   "] {
        let hits = rig
            .sessions
            .search(SessionSearchReq {
                workspace: None,
                session_id: None,
                query: q.into(),
                limit: None,
            })
            .await
            .unwrap();
        assert!(hits.is_empty(), "the query {q:?} should come back empty");
    }
}

#[tokio::test]
async fn search_honours_its_limit() {
    let rig = Rig::new();
    let id = rig.open_new().await;
    for i in 1..=10 {
        rig.say(id, i, "a repeated word", "answer");
    }

    let hits = rig
        .sessions
        .search(SessionSearchReq {
            workspace: None,
            session_id: None,
            query: "repeated".into(),
            limit: Some(3),
        })
        .await
        .unwrap();
    assert_eq!(hits.len(), 3);
}

#[tokio::test]
async fn a_snippet_never_splits_a_multibyte_character() {
    let rig = Rig::new();
    let id = rig.open_new().await;
    let long = format!("{}keyword{}", "é".repeat(300), "é".repeat(300));
    rig.say(id, 1, &long, "answer");

    let hits = rig
        .sessions
        .search(SessionSearchReq {
            workspace: None,
            session_id: None,
            query: "keyword".into(),
            limit: None,
        })
        .await
        .unwrap();

    let snippet = &hits[0].snippet;
    assert!(snippet.contains("keyword"));
    assert!(
        snippet.chars().count() < long.chars().count(),
        "it should be truncated"
    );
    let round: String = serde_json::from_str(&serde_json::to_string(snippet).unwrap()).unwrap();
    assert_eq!(&round, snippet);
}

#[tokio::test]
async fn tool_arguments_and_output_are_not_searchable() {
    let rig = Rig::new();
    let id = rig.open_new().await;
    let turn = TurnId::new();
    rig.store.with(|db| {
        db.entries()
            .append(NewEntry::new(
                id,
                turn,
                1,
                EntryKind::ToolCall,
                serde_json::to_value(ContentPart::ToolCall(
                    zlogic_protocol::message::ToolCallPart {
                        calls: vec![zlogic_protocol::message::ToolCall {
                            id: "c1".into(),
                            name: "grep".into(),
                            args: r#"{"q":"a distinctive word"}"#.into(),
                            raw: None,
                        }],
                    },
                ))
                .unwrap(),
            ))
            .unwrap();
    });

    let hits = rig
        .sessions
        .search(SessionSearchReq {
            workspace: None,
            session_id: None,
            query: "a distinctive word".into(),
            limit: None,
        })
        .await
        .unwrap();
    assert!(hits.is_empty(), "tool arguments must not be searchable");
}

#[tokio::test]
async fn turn_items_carry_the_cards_rendered_in_that_turn() {
    let rig = Rig::new();
    let id = rig.open_new().await;
    rig.say(id, 1, "draw two pictures", "done drawing");
    rig.say(id, 2, "draw one more", "ok");

    let objects = MemoryObjectStore::new();
    let first = objects.put(b"widget one").unwrap();
    let second = objects.put(b"widget two").unwrap();
    let third = objects.put(b"widget three").unwrap();
    let card = |object_id: &zlogic_objects::ObjectId, title: &str| {
        zlogic_objects::ObjectRef::classified(
            object_id.clone(),
            zlogic_objects::ObjectRole::Output,
            "widget",
            Some(title.into()),
            Some(serde_json::json!({ "height": 360, "libraries": ["chart"] }).to_string()),
        )
    };
    /* Taking the turn id has to happen **outside** `with`: `SharedStore::with` takes the same
     * lock, so a nested call would lock itself out (which is exactly what hung the first version
     * of this test). */
    let turn_of = |seq: i64| {
        rig.store
            .with(|db| db.entries().list(id).unwrap())
            .into_iter()
            .find(|e| e.turn_seq == seq && e.kind == EntryKind::AssistantText)
            .map(|e| e.turn_id)
            .expect("the turn exists")
    };
    let (first_turn, second_turn) = (turn_of(1), turn_of(2));
    rig.store.with(|db| {
        db.entries()
            .append(
                NewEntry::new(
                    id,
                    first_turn,
                    1,
                    EntryKind::ToolResult,
                    serde_json::json!({ "summary": "charts" }),
                )
                .references(card(&first, "Revenue"))
                .references(card(&second, "Churn")),
            )
            .unwrap();
        db.entries()
            .append(
                NewEntry::new(
                    id,
                    second_turn,
                    2,
                    EntryKind::ToolResult,
                    serde_json::json!({ "summary": "chart" }),
                )
                .references(card(&third, "Margin")),
            )
            .unwrap();
    });

    let page = rig
        .sessions
        .turns(zlogic_protocol::query::TurnsReq {
            session_id: id,
            after_turn_seq: None,
            offset: None,
            limit: None,
        })
        .await
        .unwrap();

    let turn = |seq: u32| {
        page.items
            .iter()
            .find(|t| t.turn_seq == seq)
            .unwrap()
            .clone()
    };

    let one = turn(1);
    assert_eq!(
        one.widgets
            .iter()
            .map(|w| (w.object_id.as_str(), w.title.as_str()))
            .collect::<Vec<_>>(),
        [
            (first.to_string().as_str(), "Revenue"),
            (second.to_string().as_str(), "Churn"),
        ]
    );
    assert_eq!(one.widgets[0].height, 360);
    assert_eq!(one.widgets[0].libraries, vec!["chart".to_string()]);

    let two = turn(2);
    assert_eq!(two.widgets.len(), 1);
    assert_eq!(two.widgets[0].title, "Margin");

    assert!(one.answer.is_some() && !one.widgets.is_empty());
}

#[tokio::test]
async fn turn_items_without_cards_have_an_empty_widget_list() {
    let rig = Rig::new();
    let id = rig.open_new().await;
    rig.say(id, 1, "just chatting", "ok");

    let page = rig
        .sessions
        .turns(zlogic_protocol::query::TurnsReq {
            session_id: id,
            after_turn_seq: None,
            offset: None,
            limit: None,
        })
        .await
        .unwrap();

    assert_eq!(page.items.len(), 1);
    assert!(page.items[0].widgets.is_empty());
}

#[tokio::test]
async fn a_compaction_only_turn_carries_its_summary_on_the_row() {
    let rig = Rig::new();
    let id = rig.open_new().await;
    rig.say(id, 1, "let's chat a bit", "chat's over");

    let turn = TurnId::new();
    rig.store.with(|db| {
        db.entries()
            .append(NewEntry::new(
                id,
                turn,
                2,
                EntryKind::Compaction,
                serde_json::json!({
                    "from_turn": 1,
                    "to_turn": 1,
                    "content": "The user wanted to chat a bit, and the answer was done.",
                    "reason": "threshold",
                    "summary_tokens": 21,
                }),
            ))
            .unwrap();
        db.entries()
            .append_turn_end(
                NewEntry::new(
                    id,
                    turn,
                    2,
                    EntryKind::Event,
                    serde_json::json!({ "type": "turn_end", "status": "completed" }),
                ),
                &MemoryObjectStore::default(),
            )
            .unwrap();
    });

    let page = rig
        .sessions
        .turns(zlogic_protocol::query::TurnsReq {
            session_id: id,
            after_turn_seq: None,
            offset: None,
            limit: None,
        })
        .await
        .unwrap();
    let turn_of = |seq: u32| page.items.iter().find(|t| t.turn_seq == seq).unwrap();

    let two = turn_of(2);
    assert!(
        two.answer.is_none(),
        "a compaction round has no answer to begin with"
    );
    assert!(
        two.detail,
        "compaction is a process step, so the row needs somewhere to expand it"
    );
    let compaction = two
        .compaction
        .as_ref()
        .expect("the row carries this compaction's summary and range");
    assert_eq!(compaction.replaces, (1, 1));
    assert_eq!(
        compaction.summary,
        "The user wanted to chat a bit, and the answer was done."
    );
    assert_eq!(compaction.summary_tokens, Some(21));

    let one = turn_of(1);
    assert!(one.compaction.is_none());
    assert!(one.answer.is_some());
}
