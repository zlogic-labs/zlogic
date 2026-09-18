use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use zlogic_core::SharedStore;
use zlogic_objects::ObjectStore;
use zlogic_protocol::query::{
    ApiError, ApiResult, EditProtection, EntriesReq, EntryRole, ModelSelection,
    ModelSelectionSource, Page, PendingOrigin, PendingSubmission, SessionListReq, SessionOpenReq,
    SessionOpened, SessionRenameReq, SessionSearchHit, SessionSearchReq, SessionSummary,
    TranscriptBody, TranscriptEntry, TranscriptKind, TranscriptPart, TranscriptReq, TurnAnswer,
    TurnAnswerKind, TurnCompaction, TurnItem, TurnWidget, TurnsReq, WorkspaceSummary,
};
use zlogic_protocol::usage::Purpose;
use zlogic_protocol::{Effort, SessionId, TurnId, query::TitleSource};
use zlogic_store::{EntryKind, EntryRecord, NewSession, SessionRecord};

use crate::service::{SessionService, WorkspaceService};
use crate::{EngineError, ModelRouter, Result, TurnRegistry};

const SNIPPET_CHARS: usize = 160;

const DEFAULT_TURN_PAGE: u32 = 20;

const DEFAULT_ENTRY_LIMIT: u32 = 500;

pub struct Sessions {
    store: SharedStore,
    objects: Arc<dyn ObjectStore>,
    workspaces: Arc<dyn WorkspaceService>,
    router: Option<Arc<ModelRouter>>,
    registry: Option<Arc<TurnRegistry>>,
    grants: Option<Arc<crate::Grants>>,
    /// Materialized uploaded files are session-owned and leave with the session.
    attachment_dir: PathBuf,
}

impl Sessions {
    pub fn new(
        store: SharedStore,
        objects: Arc<dyn ObjectStore>,
        workspaces: Arc<dyn WorkspaceService>,
        attachment_dir: PathBuf,
    ) -> Self {
        Self {
            store,
            objects,
            workspaces,
            router: None,
            registry: None,
            grants: None,
            attachment_dir,
        }
    }

    pub fn with_router(mut self, router: Arc<ModelRouter>) -> Self {
        self.router = Some(router);
        self
    }

    pub fn with_grants(mut self, grants: Arc<crate::Grants>) -> Self {
        self.grants = Some(grants);
        self
    }

    pub fn with_registry(mut self, registry: Arc<TurnRegistry>) -> Self {
        self.registry = Some(registry);
        self
    }

    fn record(&self, session_id: SessionId) -> Result<SessionRecord> {
        self.store
            .with(|db| db.sessions().find(session_id))?
            .ok_or_else(|| EngineError::NotFound(format!("session {session_id}")))
    }

    fn summary(
        &self,
        rec: &SessionRecord,
        live_turn_id: Option<TurnId>,
        awaiting_input: bool,
    ) -> Result<SessionSummary> {
        Ok(SessionSummary {
            session_id: rec.session_id,
            workspace_id: rec.workspace_id,
            agent_paths: rec.agent_paths.0.clone(),
            root_session_id: rec.root_session_id,
            title: rec.title.clone(),
            title_source: rec.title_source.map(title_source),
            model_ref: rec.model_ref.clone(),
            effort: rec.effort.as_deref().and_then(Effort::from_wire),
            created_at: rec.created_at,
            updated_at: rec.updated_at,
            last_message_at: rec.last_message_at,
            turn_count: rec.turn_count,
            live_turn_id,
            awaiting_input,
            archived_at: rec.archived_at,
        })
    }

    fn summary_of(&self, rec: &SessionRecord) -> Result<SessionSummary> {
        let (live_turn_id, awaiting_input) = self.store.with(|db| {
            let live_turn_id = db.locks().live_turn(rec.session_id)?;
            let awaiting_input = !db
                .entries()
                .awaiting_interactions_in(&[rec.session_id])?
                .is_empty();
            Ok::<_, zlogic_store::StoreError>((live_turn_id, awaiting_input))
        })?;
        self.summary(rec, live_turn_id, awaiting_input)
    }

    fn transcript_of(
        &self,
        session_id: SessionId,
        after_turn_seq: Option<u32>,
        offset: Option<u32>,
        limit: Option<u32>,
    ) -> Result<Page<TranscriptEntry>> {
        let rows = self.store.with(|db| db.entries().list(session_id))?;

        let mut turn_seqs: Vec<i64> = rows.iter().map(|r| r.turn_seq).collect();
        turn_seqs.dedup();
        if let Some(after) = after_turn_seq {
            turn_seqs.retain(|seq| *seq > after as i64);
        }

        let total = turn_seqs.len() as u64;
        let skip = offset.unwrap_or(0) as usize;
        if skip > 0 {
            let start = turn_seqs.len().saturating_sub(skip);
            if start < turn_seqs.len() {
                turn_seqs.drain(start..);
            }
        }
        if let Some(limit) = limit {
            let keep = limit as usize;
            if turn_seqs.len() > keep {
                turn_seqs.drain(..turn_seqs.len() - keep);
            }
        }

        let keep: HashSet<i64> = turn_seqs.into_iter().collect();
        let rows: Vec<EntryRecord> = rows
            .into_iter()
            .filter(|r| keep.contains(&r.turn_seq))
            .collect();

        let objects = self.objects.clone();
        let payload_objects = self
            .store
            .with_named("transcript.payloads", |db| {
                db.entries().payload_objects(&rows)
            })
            .unwrap_or_default();
        let load = move |rec: &zlogic_store::EntryRecord| match payload_objects.get(&rec.entry_id) {
            None => Some(rec.data.clone()),
            Some(id) => objects
                .get(id)
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok()),
        };
        let items = crate::transcript::project(&rows, &load);
        Ok(Page { items, total })
    }

    fn turn_items_of(
        &self,
        session_id: SessionId,
        after_turn_seq: Option<u32>,
        offset: Option<u32>,
        limit: Option<u32>,
    ) -> Result<Page<TurnItem>> {
        let (seqs, total) = self.store.with(|db| {
            db.entries().turn_page(
                session_id,
                after_turn_seq.map(i64::from),
                offset.unwrap_or(0),
                limit.unwrap_or(DEFAULT_TURN_PAGE),
            )
        })?;
        if seqs.is_empty() {
            return Ok(Page {
                items: Vec::new(),
                total,
            });
        }

        let assemble: &[EntryKind] = &[
            EntryKind::User,
            EntryKind::Steering,
            EntryKind::TaskUpdate,
            EntryKind::AssistantText,
            EntryKind::Thinking,
            EntryKind::Event,
            EntryKind::Compaction,
        ];
        let rows = self.store.with(|db| {
            db.entries()
                .rows_of_turns(session_id, &seqs, Some(assemble), true)
        })?;
        let anchors = self
            .store
            .with(|db| db.entries().turn_anchors(session_id, &seqs))?
            .into_iter()
            .map(|(seq, turn_id, at)| (seq, (turn_id, at)))
            .collect::<BTreeMap<_, _>>();
        let widget_assets = self
            .store
            .with(|db| db.entries().assets_of_turns(session_id, &seqs, &["widget"]))?;

        let detail_kinds: &[EntryKind] = &[
            EntryKind::Thinking,
            EntryKind::ToolCall,
            EntryKind::ToolResult,
            EntryKind::Steering,
            EntryKind::TaskUpdate,
            EntryKind::InteractionRequest,
            EntryKind::InteractionResponse,
            EntryKind::Compaction,
        ];
        let detailed = self.store.with(|db| {
            db.entries()
                .turns_having_kinds(session_id, &seqs, detail_kinds)
        })?;

        let mut widgets_by_turn: BTreeMap<i64, Vec<TurnWidget>> = BTreeMap::new();
        for asset in widget_assets {
            let meta: Option<WidgetMeta> = asset
                .meta
                .as_deref()
                .and_then(|text| serde_json::from_str(text).ok());
            widgets_by_turn
                .entry(asset.turn_seq)
                .or_default()
                .push(TurnWidget {
                    object_id: asset.object_id,
                    title: asset.label.unwrap_or_default(),
                    height: meta.as_ref().map(|m| m.height).unwrap_or(0),
                    libraries: meta.map(|m| m.libraries).unwrap_or_default(),
                });
        }

        let projected = self.project_rows(&rows);
        let mut by_turn: BTreeMap<i64, Vec<TranscriptEntry>> = BTreeMap::new();
        for entry in projected {
            by_turn
                .entry(i64::from(entry.turn_seq))
                .or_default()
                .push(entry);
        }

        let mut items: Vec<TurnItem> = Vec::with_capacity(seqs.len());
        for seq in &seqs {
            /* A round must be listed even if it has no content rows at all (only tool rows) — its
             * `turn_id`/time come from the anchor, not from "some row of that round". So we no
             * longer require `by_turn` to have anything in it here. */
            let empty: Vec<TranscriptEntry> = Vec::new();
            let entries = by_turn.get(seq).unwrap_or(&empty);
            let Some((turn_id, at)) = anchors.get(seq) else {
                continue; // that round has not a single entry (impossible, but do not panic)
            };
            let mut user: Vec<TranscriptPart> = Vec::new();
            let mut answer: Option<TurnAnswer> = None;
            let mut status: Option<zlogic_protocol::stream::TurnStatus> = None;
            let mut reason: Option<String> = None;
            /* Compaction done during this round: keep the last one on the row — the same reasoning
             * as taking the last body for `answer`: the row has to describe "what this round
             * finally produced", and a manual `/compact` round produces nothing else. When a round
             * is compacted twice (threshold + overflow retry) both are in the detail. */
            let mut compaction: Option<TurnCompaction> = None;
            /* A widget is an answer, not a process step: it shows on the collapsed row together
             * with the final reply. Cap it at 8 — a round can in theory render any number of them,
             * and the list payload should not be inflated by them (the UI already sorts newest
             * first, so cut from the **tail** and keep the most recent ones). */
            const MAX_TURN_WIDGETS: usize = 8;
            let mut widgets = widgets_by_turn.remove(seq).unwrap_or_default();
            if widgets.len() > MAX_TURN_WIDGETS {
                widgets.drain(..widgets.len() - MAX_TURN_WIDGETS);
            }
            for entry in entries {
                match &entry.body {
                    TranscriptBody::User { parts } => user.extend(parts.iter().cloned()),
                    TranscriptBody::Text { text, truncated } if entry.is_final => {
                        answer = Some(TurnAnswer {
                            kind: TurnAnswerKind::Text,
                            text: text.clone(),
                            truncated: *truncated,
                        });
                    }
                    TranscriptBody::Reasoning { text, truncated } if entry.is_final => {
                        answer = Some(TurnAnswer {
                            kind: TurnAnswerKind::Reasoning,
                            text: text.clone(),
                            truncated: *truncated,
                        });
                    }
                    TranscriptBody::TurnEnd {
                        status: ended,
                        reason: end_reason,
                        ..
                    } => {
                        status = Some(ended.clone());
                        reason = end_reason.clone();
                    }
                    TranscriptBody::Compaction {
                        replaces,
                        summary,
                        summary_tokens,
                        ..
                    } => {
                        compaction = Some(TurnCompaction {
                            replaces: *replaces,
                            summary: summary.clone(),
                            summary_tokens: *summary_tokens,
                        });
                    }
                    _ => {}
                }
            }
            let detail = detailed.contains(seq);
            if user.is_empty() && answer.is_none() && status.is_none() && !detail {
                continue;
            }
            items.push(TurnItem {
                turn_seq: *seq as u32,
                turn_id: *turn_id,
                at: *at,
                user,
                answer,
                status,
                reason,
                detail,
                compaction,
                widgets,
            });
        }
        let missing: Vec<i64> = items
            .iter()
            .filter(|item| item.answer.is_none())
            .map(|item| i64::from(item.turn_seq))
            .collect();
        if !missing.is_empty() {
            let rows = self
                .store
                .with(|db| db.entries().last_text_rows_per_turn(session_id, &missing))?;
            let mut fallback: BTreeMap<i64, TurnAnswer> = BTreeMap::new();
            for entry in self.project_rows(&rows) {
                let answer = match &entry.body {
                    TranscriptBody::Text { text, truncated } => Some(TurnAnswer {
                        kind: TurnAnswerKind::Text,
                        text: text.clone(),
                        truncated: *truncated,
                    }),
                    TranscriptBody::Reasoning { text, truncated } => Some(TurnAnswer {
                        kind: TurnAnswerKind::Reasoning,
                        text: text.clone(),
                        truncated: *truncated,
                    }),
                    _ => None,
                };
                if let Some(answer) = answer {
                    fallback.insert(i64::from(entry.turn_seq), answer);
                }
            }
            for item in &mut items {
                if item.answer.is_none() {
                    item.answer = fallback.remove(&i64::from(item.turn_seq));
                }
            }
        }

        items.reverse();
        Ok(Page { items, total })
    }

    fn entries_of(
        &self,
        session_id: SessionId,
        turn_seq: Option<u32>,
        turn_id: Option<TurnId>,
        role: Option<EntryRole>,
        offset: Option<u32>,
        limit: Option<u32>,
    ) -> Result<Page<TranscriptEntry>> {
        const USER: &[EntryKind] = &[EntryKind::User];
        const ASSISTANT: &[EntryKind] = &[EntryKind::AssistantText];
        const EVENT: &[EntryKind] = &[EntryKind::Event];
        const TOOL: &[EntryKind] = &[EntryKind::ToolCall, EntryKind::ToolResult];
        let (kinds, only_final) = match role {
            None => (None, false),
            Some(EntryRole::User) => (Some(USER), false),
            Some(EntryRole::Assistant) => (Some(ASSISTANT), false),
            Some(EntryRole::Final) => (None, true),
            Some(EntryRole::Event) => (Some(EVENT), false),
            Some(EntryRole::Tool) => (Some(TOOL), false),
        };
        let (mut rows, total) = self.store.with(|db| {
            db.entries().entries_filtered(
                session_id,
                turn_seq.map(i64::from),
                turn_id,
                kinds,
                only_final,
                offset.unwrap_or(0),
                limit.unwrap_or(DEFAULT_ENTRY_LIMIT),
            )
        })?;
        rows.reverse();
        let items = self.project_rows(&rows);
        Ok(Page { items, total })
    }

    fn project_rows(&self, rows: &[EntryRecord]) -> Vec<TranscriptEntry> {
        let objects = self.objects.clone();
        let payload_objects = self
            .store
            .with_named("transcript.payloads", |db| {
                db.entries().payload_objects(rows)
            })
            .unwrap_or_default();
        let load = move |rec: &zlogic_store::EntryRecord| match payload_objects.get(&rec.entry_id) {
            None => Some(rec.data.clone()),
            Some(id) => objects
                .get(id)
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok()),
        };
        crate::transcript::project(rows, &load)
    }

    fn model_selection(&self, rec: &SessionRecord) -> ModelSelection {
        let Some(router) = &self.router else {
            return ModelSelection {
                model_ref: rec.model_ref.clone(),
                source: ModelSelectionSource::None,
                models_configured: false,
                has_usable_credential: false,
            };
        };
        let config = router.config();
        let models_configured = !config.all_model_refs().is_empty();

        match router.resolve(&Purpose::Main, rec.model_ref.as_deref()) {
            Ok(routed) => {
                let resolved = routed.model_ref();
                let source = if rec.model_ref.as_deref() == Some(resolved.as_str()) {
                    ModelSelectionSource::Session
                } else if config.default_model.as_deref() == Some(resolved.as_str()) {
                    ModelSelectionSource::Default
                } else {
                    ModelSelectionSource::FirstUsable
                };
                ModelSelection {
                    model_ref: Some(resolved),
                    source,
                    models_configured,
                    has_usable_credential: true,
                }
            }
            Err(_) => ModelSelection {
                model_ref: rec.model_ref.clone(),
                source: ModelSelectionSource::None,
                models_configured,
                has_usable_credential: false,
            },
        }
    }
}

fn pending_submissions_of(records: Vec<zlogic_store::MailboxRecord>) -> Vec<PendingSubmission> {
    records
        .into_iter()
        .map(|m| {
            let parts: Vec<zlogic_protocol::MessagePart> =
                serde_json::from_value(m.parts).unwrap_or_default();
            let origin = if parts
                .iter()
                .any(|p| matches!(p, zlogic_protocol::MessagePart::TaskUpdate { .. }))
            {
                PendingOrigin::Task
            } else {
                PendingOrigin::User
            };
            PendingSubmission {
                submission_id: m.submission_id.to_string(),
                parts: parts.into_iter().map(part_of_input).collect(),
                origin,
                model_ref: m.model_ref,
                delivery: delivery(m.delivery),
                queued_at: m.created_at,
            }
        })
        .collect()
}

fn title_source(s: zlogic_store::TitleSource) -> TitleSource {
    match s {
        zlogic_store::TitleSource::Draft => TitleSource::Draft,
        zlogic_store::TitleSource::Model => TitleSource::Model,
        zlogic_store::TitleSource::User => TitleSource::User,
    }
}

fn delivery(d: zlogic_store::Delivery) -> zlogic_protocol::input::Delivery {
    match d {
        zlogic_store::Delivery::Steer => zlogic_protocol::input::Delivery::Steer,
        zlogic_store::Delivery::Queue => zlogic_protocol::input::Delivery::Queue,
    }
}

fn part_of_input(p: zlogic_protocol::input::MessagePart) -> TranscriptPart {
    match p {
        zlogic_protocol::input::MessagePart::Text { text } => TranscriptPart::Text { text },
        zlogic_protocol::input::MessagePart::Skill { name, args }
        | zlogic_protocol::input::MessagePart::SkillInvocation { name, args } => {
            TranscriptPart::Text {
                text: format!(
                    "/{name}{}",
                    args.as_deref()
                        .map(|value| format!(" {value}"))
                        .unwrap_or_default()
                ),
            }
        }
        zlogic_protocol::input::MessagePart::SkillLoad { name, .. } => TranscriptPart::Text {
            text: format!("/{name}"),
        },
        zlogic_protocol::input::MessagePart::SkillUnload { name } => TranscriptPart::Text {
            text: format!("/unload {name}"),
        },
        zlogic_protocol::input::MessagePart::File { path } => TranscriptPart::File {
            display_name: std::path::Path::new(&path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned()),
            path,
            mime: None,
            bytes: None,
            preview: None,
            degraded: false,
        },
        zlogic_protocol::input::MessagePart::Attachment {
            object_id,
            name,
            mime_type,
            bytes,
        } => TranscriptPart::Attachment {
            object_id,
            display_name: name,
            mime: mime_type,
            bytes,
            preview: None,
            degraded: false,
        },
        // PendingSubmission predates machine-generated mailbox rows and only has display parts.
        // Keep the origin explicit in the text until the pending projection gets its own enum.
        zlogic_protocol::input::MessagePart::TaskUpdate { update } => TranscriptPart::Text {
            text: format!(
                "[background task {} {}]{}",
                update.task_id,
                update.state,
                update
                    .summary
                    .as_deref()
                    .map(|summary| format!(" {summary}"))
                    .unwrap_or_default()
            ),
        },
    }
}

fn log_slow_api(api: &str, started: std::time::Instant, steps: &[(&'static str, u64)]) {
    let total_ms = started.elapsed().as_millis() as u64;
    if total_ms < 300 {
        return;
    }
    tracing::info!(
        target: "zlogic::engine",
        api,
        total_ms,
        steps = ?steps,
        "engine api took suspiciously long"
    );
}

#[async_trait]
impl SessionService for Sessions {
    async fn list(&self, req: SessionListReq) -> ApiResult<Page<SessionSummary>> {
        let started = std::time::Instant::now();
        let mut steps: Vec<(&'static str, u64)> = Vec::new();
        let mut last = started;

        let mut mark = |name: &'static str| {
            let now = std::time::Instant::now();
            steps.push((name, now.duration_since(last).as_millis() as u64));
            last = now;
        };

        let workspace = self.workspaces.get(req.workspace).await?;
        mark("workspaces.get");

        let ws = workspace.workspace_id;

        let query = zlogic_store::SessionQuery::of(ws)
            .include_sub_agents(req.include_sub_agents)
            .include_archived(req.include_archived);

        let (rows, live_turns, awaiting) = self
            .store
            .with_named("session_list.reads", |db| {
                let rows = db.sessions().query(&query)?;
                let live_turns = db.locks().live_turns_in(ws)?;
                let ids: Vec<SessionId> = rows.iter().map(|r| r.session_id).collect();
                let awaiting = db.entries().awaiting_interactions_in(&ids)?;
                Ok::<_, zlogic_store::StoreError>((rows, live_turns, awaiting))
            })
            .map_err(EngineError::from)?;

        mark("sessions.query + live_turns + awaiting");

        // -----------------------------------------------------------------------------
        // summaries
        // -----------------------------------------------------------------------------
        let summaries_started = std::time::Instant::now();

        let mut items: Vec<SessionSummary> = rows
            .iter()
            .map(|r| {
                self.summary(
                    r,
                    live_turns.get(&r.session_id).copied(),
                    awaiting.contains(&r.session_id),
                )
            })
            .collect::<Result<_>>()
            .map_err(ApiError::from)?;

        let summaries_ms = summaries_started.elapsed().as_millis() as u64;
        steps.push(("summaries", summaries_ms));

        if summaries_ms > 100 {
            tracing::warn!(
                target: "zlogic::engine",
                sessions = rows.len(),
                elapsed_ms = summaries_ms,
                "session summary construction took suspiciously long"
            );
        }

        // -----------------------------------------------------------------------------
        // -----------------------------------------------------------------------------
        let sort_started = std::time::Instant::now();

        items.sort_by(|a, b| {
            let key = |s: &SessionSummary| s.last_message_at.unwrap_or(s.created_at);

            (b.live_turn_id.is_some(), key(b)).cmp(&(a.live_turn_id.is_some(), key(a)))
        });

        let sort_ms = sort_started.elapsed().as_millis() as u64;
        steps.push(("sort", sort_ms));

        if sort_ms > 100 {
            tracing::warn!(
                target: "zlogic::engine",
                sessions = items.len(),
                elapsed_ms = sort_ms,
                "session summary sorting took suspiciously long"
            );
        }

        // -----------------------------------------------------------------------------
        // -----------------------------------------------------------------------------
        let pagination_started = std::time::Instant::now();

        let total = items.len() as u64;
        let offset = req.offset.unwrap_or(0) as usize;

        let items = match req.limit {
            Some(limit) => items
                .into_iter()
                .skip(offset)
                .take(limit as usize)
                .collect(),

            None => items.into_iter().skip(offset).collect(),
        };

        let pagination_ms = pagination_started.elapsed().as_millis() as u64;
        steps.push(("pagination", pagination_ms));

        log_slow_api("session_list", started, &steps);

        Ok(Page { items, total })
    }

    async fn open(&self, req: SessionOpenReq) -> ApiResult<SessionOpened> {
        let started = std::time::Instant::now();
        let mut steps: Vec<(&'static str, u64)> = Vec::new();
        let mut last = started;
        let mut mark = |name: &'static str| {
            let now = std::time::Instant::now();
            steps.push((name, now.duration_since(last).as_millis() as u64));
            last = now;
        };

        let workspace: WorkspaceSummary = self.workspaces.get(req.workspace).await?;
        mark("workspaces.get");

        let rec = match req.session_id {
            Some(id) => {
                let rec = self.record(id).map_err(ApiError::from)?;
                if rec.workspace_id != workspace.workspace_id {
                    return Err(ApiError::invalid_code(
                        "session_workspace_mismatch",
                        format!(
                            "session {id} does not belong to workspace {}",
                            workspace.workspace_id
                        ),
                    ));
                }
                rec
            }
            None => self
                .store
                .with(|db| {
                    db.sessions()
                        .create(NewSession::root(workspace.workspace_id))
                })
                .map_err(EngineError::from)
                .map_err(ApiError::from)?,
        };
        mark("session.record");

        let (live_turn_id, awaiting_input, last_turn_id, turn, pending_records) = self
            .store
            .with_named("session_open.reads", |db| {
                let live_turn_id = db.locks().live_turn(rec.session_id)?;
                let awaiting_input = !db
                    .entries()
                    .awaiting_interactions_in(&[rec.session_id])?
                    .is_empty();
                let last_turn_id = db.entries().last_turn_of(rec.session_id)?;
                let turn = self.registry.as_ref().and_then(|reg| {
                    let live = reg.live_of(rec.session_id)?;
                    crate::dispatch::turn_state_of_db(reg, db, live.turn_id)
                });
                let pending = db.mailbox().pending(rec.session_id)?;
                Ok::<_, zlogic_store::StoreError>((
                    live_turn_id,
                    awaiting_input,
                    last_turn_id,
                    turn,
                    pending,
                ))
            })
            .map_err(EngineError::from)
            .map_err(ApiError::from)?;

        let session = self
            .summary(&rec, live_turn_id, awaiting_input)
            .map_err(ApiError::from)?;
        let pending_submissions = pending_submissions_of(pending_records);

        mark("summary + last_turn + turn_state + pending");

        let opened = SessionOpened {
            session,
            exec_cwd: rec
                .exec_cwd
                .clone()
                .unwrap_or_else(|| workspace.root.clone()),
            workspace,
            last_turn_id,
            turn,
            pending_submissions,
            model: self.model_selection(&rec),
            edit_protection: EditProtection::Active,
        };

        log_slow_api("session_open", started, &steps);
        Ok(opened)
    }

    async fn rename(&self, req: SessionRenameReq) -> ApiResult<SessionSummary> {
        let rec = self.record(req.session_id).map_err(ApiError::from)?;
        let title = req.title.trim();

        self.store
            .with(|db| {
                if title.is_empty() {
                    db.sessions().clear_title(req.session_id)
                } else {
                    db.sessions()
                        .set_title(req.session_id, title, zlogic_store::TitleSource::User)
                        .map(|_| ())
                }
            })
            .map_err(EngineError::from)
            .map_err(ApiError::from)?;

        let rec = self.record(rec.session_id).map_err(ApiError::from)?;
        self.summary_of(&rec).map_err(ApiError::from)
    }

    async fn delete(&self, session_id: SessionId) -> ApiResult<()> {
        self.record(session_id).map_err(ApiError::from)?;
        self.store
            .with(|db| db.sessions().delete(session_id))
            .map_err(EngineError::from)
            .map_err(ApiError::from)?;

        if let Some(grants) = &self.grants {
            let dir = grants.session_dir(session_id);
            if dir.exists()
                && let Err(e) = std::fs::remove_dir_all(&dir)
            {
                tracing::warn!(
                    target: "zlogic::engine",
                    dir = %dir.display(),
                    "session was deleted, but its authorization directory was not removed: {e}"
                );
            }
        }
        let attachments = self.attachment_dir.join(session_id.to_string());
        if attachments.exists()
            && let Err(error) = std::fs::remove_dir_all(&attachments)
        {
            tracing::warn!(
                target: "zlogic::engine",
                dir = %attachments.display(),
                "session was deleted, but its attachments directory was not removed: {error}"
            );
        }
        Ok(())
    }

    async fn set_model(
        &self,
        session_id: SessionId,
        model_ref: String,
    ) -> ApiResult<SessionSummary> {
        let rec = self.record(session_id).map_err(ApiError::from)?;
        let model_ref = model_ref.trim();

        if let Some(router) = &self.router
            && router.config().resolve(model_ref).is_err()
        {
            return Err(ApiError::invalid_code(
                "session_model_unknown",
                format!("unknown model: {model_ref}"),
            )
            .with_detail("model_ref", model_ref));
        }

        self.store
            .with(|db| db.sessions().set_model_ref(session_id, model_ref))
            .map_err(EngineError::from)
            .map_err(ApiError::from)?;

        let rec = self.record(rec.session_id).map_err(ApiError::from)?;
        self.summary_of(&rec).map_err(ApiError::from)
    }

    async fn set_effort(&self, session_id: SessionId, effort: String) -> ApiResult<SessionSummary> {
        let rec = self.record(session_id).map_err(ApiError::from)?;
        let effort = effort.trim();

        let Some(effort) = Effort::from_wire(effort) else {
            return Err(ApiError::invalid_code(
                "session_effort_unknown",
                format!("unknown reasoning effort: {effort}"),
            )
            .with_detail("effort", effort));
        };

        self.store
            .with(|db| db.sessions().set_effort(session_id, effort.as_str()))
            .map_err(EngineError::from)
            .map_err(ApiError::from)?;

        let rec = self.record(rec.session_id).map_err(ApiError::from)?;
        self.summary_of(&rec).map_err(ApiError::from)
    }

    async fn search(&self, req: SessionSearchReq) -> ApiResult<Vec<SessionSearchHit>> {
        let needle = req.query.trim().to_lowercase();
        if needle.is_empty() {
            return Ok(Vec::new());
        }

        let scoped_session = match req.session_id {
            Some(session_id) => Some(self.record(session_id).map_err(ApiError::from)?),
            None => None,
        };

        let workspaces: Vec<zlogic_protocol::WorkspaceId> = match (&scoped_session, req.workspace) {
            (Some(rec), Some(sel)) => {
                let workspace = self.workspaces.get(sel).await?;
                if rec.workspace_id != workspace.workspace_id {
                    return Err(ApiError::invalid_code(
                        "session_workspace_mismatch",
                        "session does not belong to the given workspace",
                    ));
                }
                vec![workspace.workspace_id]
            }
            (Some(rec), None) => vec![rec.workspace_id],
            (None, Some(sel)) => vec![self.workspaces.get(sel).await?.workspace_id],
            (None, None) => self
                .store
                .with(|db| db.workspaces().list(false))
                .map_err(EngineError::from)?
                .into_iter()
                .map(|w| w.workspace_id)
                .collect(),
        };

        let limit = req.limit.unwrap_or(50) as usize;
        let mut hits = Vec::new();
        for ws in workspaces {
            let sessions = match &scoped_session {
                Some(rec) => vec![rec.clone()],
                None => self
                    .store
                    .with(|db| db.sessions().list(ws))
                    .map_err(EngineError::from)?,
            };
            let ids: Vec<SessionId> = sessions.iter().map(|s| s.session_id).collect();
            let (live_turns, awaiting) = self
                .store
                .with(|db| {
                    let live_turns = db.locks().live_turns_in(ws)?;
                    let awaiting = db.entries().awaiting_interactions_in(&ids)?;
                    Ok::<_, zlogic_store::StoreError>((live_turns, awaiting))
                })
                .map_err(EngineError::from)?;

            for rec in &sessions {
                let summary = self
                    .summary(
                        rec,
                        live_turns.get(&rec.session_id).copied(),
                        awaiting.contains(&rec.session_id),
                    )
                    .map_err(ApiError::from)?;
                let rows = self
                    .store
                    .with(|db| db.entries().search_candidates(rec.session_id))
                    .map_err(EngineError::from)?;

                for entry in self.project_rows(&rows) {
                    let Some((kind, text)) = searchable(&entry.body, entry.is_final) else {
                        continue;
                    };
                    let Some(at) = text.to_lowercase().find(&needle) else {
                        continue;
                    };
                    hits.push(SessionSearchHit {
                        session: summary.clone(),
                        turn_seq: entry.turn_seq,
                        round_seq: entry.round_seq,
                        kind,
                        at: entry.at,
                        snippet: snippet_around(&text, at, needle.chars().count()),
                    });
                    if hits.len() >= limit {
                        return Ok(hits);
                    }
                }
            }
        }
        Ok(hits)
    }

    async fn transcript(&self, req: TranscriptReq) -> ApiResult<Page<TranscriptEntry>> {
        self.record(req.session_id).map_err(ApiError::from)?;
        self.transcript_of(req.session_id, req.after_turn_seq, req.offset, req.limit)
            .map_err(ApiError::from)
    }

    async fn turns(&self, req: TurnsReq) -> ApiResult<Page<TurnItem>> {
        self.record(req.session_id).map_err(ApiError::from)?;
        self.turn_items_of(req.session_id, req.after_turn_seq, req.offset, req.limit)
            .map_err(ApiError::from)
    }

    async fn entries(&self, req: EntriesReq) -> ApiResult<Page<TranscriptEntry>> {
        self.record(req.session_id).map_err(ApiError::from)?;
        self.entries_of(
            req.session_id,
            req.turn_seq,
            req.turn_id,
            req.role,
            req.offset,
            req.limit,
        )
        .map_err(ApiError::from)
    }
}

#[derive(serde::Deserialize)]
struct WidgetMeta {
    #[serde(default)]
    height: u32,
    #[serde(default)]
    libraries: Vec<String>,
}

fn searchable(body: &TranscriptBody, is_final: bool) -> Option<(TranscriptKind, String)> {
    match body {
        TranscriptBody::User { parts } => Some((TranscriptKind::User, text_of(parts))),
        TranscriptBody::Steering { parts } => Some((TranscriptKind::Steering, text_of(parts))),
        TranscriptBody::Text { text, .. } if is_final => Some((TranscriptKind::Text, text.clone())),
        TranscriptBody::Text { .. }
        | TranscriptBody::Reasoning { .. }
        | TranscriptBody::ToolCall { .. }
        | TranscriptBody::ToolResult { .. }
        | TranscriptBody::InteractionRequest { .. }
        | TranscriptBody::InteractionResponse { .. }
        | TranscriptBody::Notice { .. }
        | TranscriptBody::TaskUpdate { .. }
        | TranscriptBody::Compaction { .. }
        | TranscriptBody::TurnEnd { .. } => None,
    }
}

fn text_of(parts: &[TranscriptPart]) -> String {
    parts
        .iter()
        .filter_map(|p| match p {
            TranscriptPart::Text { text } => Some(text.as_str()),
            TranscriptPart::File { .. } | TranscriptPart::Attachment { .. } => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn snippet_around(text: &str, byte_at: usize, needle_chars: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let hit = text[..byte_at].chars().count();
    let margin = SNIPPET_CHARS.saturating_sub(needle_chars) / 2;
    let start = hit.saturating_sub(margin);
    let end = (hit + needle_chars + margin).min(chars.len());

    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.extend(&chars[start..end]);
    if end < chars.len() {
        out.push('…');
    }
    out
}
