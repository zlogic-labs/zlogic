//! |---|---|---|

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, PoisonError};

use tokio::sync::broadcast;
use zlogic_protocol::stream::{
    StateNotice, StreamEvent, StreamPayload, TaskOutputDelta, WorkspaceInitProgress,
};

const TURN_CHANNEL_CAPACITY: usize = 1024;

const TURN_REPLAY_CAPACITY: usize = 2_048;
const TURN_REPLAY_MAX_BYTES: usize = 8 * 1024 * 1024;

const NOTICE_CHANNEL_CAPACITY: usize = 256;

const INIT_CHANNEL_CAPACITY: usize = 32;

const TASK_CHANNEL_CAPACITY: usize = 256;
const TASK_REPLAY_CAPACITY: usize = 1_024;
const TASK_REPLAY_MAX_BYTES: usize = 2 * 1024 * 1024;
const TASK_CHANNELS_KEPT: usize = 64;

pub struct EventHub {
    turns: Mutex<HashMap<String, TurnChannel>>,
    tasks: Mutex<TaskChannels>,
    parents: Mutex<HashMap<String, String>>,
    notices: broadcast::Sender<StateNotice>,
    notice_gaps: broadcast::Sender<u64>,
    inits: Mutex<InitChannel>,
    stream_epoch: String,
}

struct InitChannel {
    live: broadcast::Sender<WorkspaceInitProgress>,
    latest: HashMap<String, WorkspaceInitProgress>,
}

#[derive(Default)]
struct TaskChannels {
    streams: HashMap<String, TaskStream>,
    order: VecDeque<String>,
}

impl TaskChannels {
    fn push(&mut self, delta: TaskOutputDelta) {
        let task_id = delta.task_id.clone();
        let stream = self.streams.entry(task_id.clone()).or_default();
        if !self.order.contains(&task_id) {
            self.order.push_back(task_id);
        }
        stream.history_bytes = stream.history_bytes.saturating_add(approx_bytes(&delta));
        stream.history.push_back(delta.clone());
        while stream.history.len() > TASK_REPLAY_CAPACITY
            || stream.history_bytes > TASK_REPLAY_MAX_BYTES
        {
            let Some(dropped) = stream.history.pop_front() else {
                break;
            };
            stream.history_bytes = stream.history_bytes.saturating_sub(approx_bytes(&dropped));
        }
        let _ = stream.live.send(delta);
        self.evict();
    }

    fn snapshot(
        &mut self,
        task_id: &str,
    ) -> (Vec<TaskOutputDelta>, broadcast::Receiver<TaskOutputDelta>) {
        let stream = self.streams.entry(task_id.to_string()).or_default();
        if !self.order.contains(&task_id.to_string()) {
            self.order.push_back(task_id.to_string());
        }
        (
            stream.history.iter().cloned().collect(),
            stream.live.subscribe(),
        )
    }

    fn close(&mut self, task_id: &str) {
        if let Some(stream) = self.streams.get_mut(task_id) {
            stream.closed = true;
        }
    }

    fn evict(&mut self) {
        while self.streams.len() > TASK_CHANNELS_KEPT {
            let closed = self
                .order
                .iter()
                .position(|id| self.streams.get(id).is_some_and(|stream| stream.closed));
            let Some(id) = self.order.remove(closed.unwrap_or(0)) else {
                break;
            };
            self.streams.remove(&id);
        }
    }
}

struct TaskStream {
    live: broadcast::Sender<TaskOutputDelta>,
    history: VecDeque<TaskOutputDelta>,
    history_bytes: usize,
    closed: bool,
}

impl Default for TaskStream {
    fn default() -> Self {
        Self {
            live: broadcast::channel(TASK_CHANNEL_CAPACITY).0,
            history: VecDeque::new(),
            history_bytes: 0,
            closed: false,
        }
    }
}

fn approx_bytes(delta: &TaskOutputDelta) -> usize {
    const FRAMING: usize = 64;
    delta.task_id.len() + delta.chunk.len() + FRAMING
}

struct TurnChannel {
    live: broadcast::Sender<StreamEvent>,
    replay_live: broadcast::Sender<ReplayEvent>,
    gaps: broadcast::Sender<u64>,
    history: VecDeque<ReplayEvent>,
    history_bytes: usize,
    next_cursor: u64,
    /// Cursor of the current turn's `TurnStart`. A client attaching without a cursor needs the
    /// complete active projection, not merely events emitted after it connected.
    active_turn_start: Option<(String, u64)>,
}

impl TurnChannel {
    fn new() -> Self {
        Self {
            live: broadcast::channel(TURN_CHANNEL_CAPACITY).0,
            // A cold attach may need to forward the whole replay window before it starts polling
            // live. Give this receiver the same depth so that handoff itself cannot create a gap.
            replay_live: broadcast::channel(TURN_REPLAY_CAPACITY).0,
            gaps: broadcast::channel(16).0,
            history: VecDeque::with_capacity(TURN_REPLAY_CAPACITY),
            history_bytes: 0,
            next_cursor: 0,
            active_turn_start: None,
        }
    }

    fn trim(&mut self) {
        while self.history.len() > TURN_REPLAY_CAPACITY
            || self.history_bytes > TURN_REPLAY_MAX_BYTES
        {
            let at_active_start = self.history_bytes <= TURN_REPLAY_MAX_BYTES
                && self.active_turn_start.as_ref().is_some_and(|(_, start)| {
                    self.history
                        .front()
                        .is_some_and(|item| item.cursor >= *start)
                });
            if at_active_start {
                break;
            }
            let Some(removed) = self.history.pop_front() else {
                break;
            };
            self.history_bytes = self.history_bytes.saturating_sub(removed.approx_bytes);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayCursor {
    pub epoch: String,
    pub cursor: u64,
}

#[derive(Debug, Clone)]
pub struct ReplayEvent {
    pub cursor: u64,
    pub event: StreamEvent,
    approx_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayGap {
    EpochChanged,
    CursorTooOld { oldest_available: u64 },
    CursorAhead { latest_available: u64 },
}

pub struct TurnReplaySubscription {
    pub epoch: String,
    pub latest_cursor: u64,
    pub replay: Vec<ReplayEvent>,
    pub receiver: broadcast::Receiver<ReplayEvent>,
    pub gap: Option<ReplayGap>,
}

impl EventHub {
    pub fn new() -> Self {
        let (notices, _) = broadcast::channel(NOTICE_CHANNEL_CAPACITY);
        let (notice_gaps, _) = broadcast::channel(16);
        let (inits, _) = broadcast::channel(INIT_CHANNEL_CAPACITY);
        Self {
            turns: Mutex::new(HashMap::new()),
            parents: Mutex::new(HashMap::new()),
            tasks: Mutex::new(TaskChannels::default()),
            notices,
            notice_gaps,
            inits: Mutex::new(InitChannel {
                live: inits,
                latest: HashMap::new(),
            }),
            stream_epoch: uuid::Uuid::now_v7().to_string(),
        }
    }

    pub fn emit_task(&self, delta: TaskOutputDelta) {
        let mut tasks = self.tasks.lock().unwrap_or_else(PoisonError::into_inner);
        tasks.push(delta);
    }

    pub fn subscribe_task(
        &self,
        task_id: &str,
    ) -> (Vec<TaskOutputDelta>, broadcast::Receiver<TaskOutputDelta>) {
        let mut tasks = self.tasks.lock().unwrap_or_else(PoisonError::into_inner);
        tasks.snapshot(task_id)
    }

    pub fn close_task(&self, task_id: &str) {
        let mut tasks = self.tasks.lock().unwrap_or_else(PoisonError::into_inner);
        tasks.close(task_id);
    }

    pub fn subscribe_turns(&self, session_id: &str) -> broadcast::Receiver<StreamEvent> {
        let mut turns = self.lock_turns();
        turns
            .entry(session_id.to_string())
            .or_insert_with(TurnChannel::new)
            .live
            .subscribe()
    }

    pub fn subscribe_turns_after(
        &self,
        session_id: &str,
        after: Option<&ReplayCursor>,
    ) -> TurnReplaySubscription {
        let mut turns = self.lock_turns();
        let channel = turns
            .entry(session_id.to_string())
            .or_insert_with(TurnChannel::new);
        let receiver = channel.replay_live.subscribe();
        let oldest = channel.history.front().map(|item| item.cursor);
        let latest = channel.next_cursor;

        let (replay, gap) = match after {
            None => (Vec::new(), None),
            Some(cursor) if cursor.epoch != self.stream_epoch => {
                (Vec::new(), Some(ReplayGap::EpochChanged))
            }
            Some(cursor) if cursor.cursor > latest => (
                Vec::new(),
                Some(ReplayGap::CursorAhead {
                    latest_available: latest,
                }),
            ),
            Some(cursor)
                if cursor.cursor < latest
                    && oldest.is_none_or(|oldest| cursor.cursor.saturating_add(1) < oldest) =>
            {
                (
                    Vec::new(),
                    Some(ReplayGap::CursorTooOld {
                        oldest_available: oldest.unwrap_or_else(|| latest.saturating_add(1)),
                    }),
                )
            }
            Some(cursor) => (
                channel
                    .history
                    .iter()
                    .filter(|item| item.cursor > cursor.cursor)
                    .cloned()
                    .collect(),
                None,
            ),
        };

        TurnReplaySubscription {
            epoch: self.stream_epoch.clone(),
            latest_cursor: latest,
            replay,
            receiver,
            gap,
        }
    }

    /// Subscribe to the complete currently-running turn and then its live tail.
    /// This is the cold-attach/session-remount path. Finished turns are deliberately not replayed:
    /// their authoritative representation is the transcript. If the beginning of the active turn
    /// has already fallen out of the bounded window, the caller receives a gap and must reload the
    /// authoritative query state.
    pub fn subscribe_current_turn(&self, session_id: &str) -> TurnReplaySubscription {
        let mut turns = self.lock_turns();
        let channel = turns
            .entry(session_id.to_string())
            .or_insert_with(TurnChannel::new);
        let receiver = channel.replay_live.subscribe();
        let latest = channel.next_cursor;

        let (replay, gap) = match &channel.active_turn_start {
            None => (Vec::new(), None),
            Some((_, start)) => {
                let oldest = channel.history.front().map(|item| item.cursor);
                if oldest.is_none_or(|oldest| *start < oldest) {
                    (
                        Vec::new(),
                        Some(ReplayGap::CursorTooOld {
                            oldest_available: oldest.unwrap_or_else(|| latest.saturating_add(1)),
                        }),
                    )
                } else {
                    (
                        channel
                            .history
                            .iter()
                            .filter(|item| item.cursor >= *start)
                            .cloned()
                            .collect(),
                        None,
                    )
                }
            }
        };

        TurnReplaySubscription {
            epoch: self.stream_epoch.clone(),
            latest_cursor: latest,
            replay,
            receiver,
            gap,
        }
    }

    pub fn signal_turn_gap(&self, session_id: &str, skipped: u64) {
        let mut turns = self.lock_turns();
        let channel = turns
            .entry(session_id.to_string())
            .or_insert_with(TurnChannel::new);
        channel.history.clear();
        channel.history_bytes = 0;
        channel.active_turn_start = None;
        let _ = channel.gaps.send(skipped.max(1));
    }

    pub fn subscribe_turn_gaps(&self, session_id: &str) -> broadcast::Receiver<u64> {
        let mut turns = self.lock_turns();
        turns
            .entry(session_id.to_string())
            .or_insert_with(TurnChannel::new)
            .gaps
            .subscribe()
    }

    pub fn subscribe_notices(&self) -> broadcast::Receiver<StateNotice> {
        self.notices.subscribe()
    }

    pub fn signal_notice_gap(&self, skipped: u64) {
        let _ = self.notice_gaps.send(skipped.max(1));
    }

    pub fn subscribe_notice_gaps(&self) -> broadcast::Receiver<u64> {
        self.notice_gaps.subscribe()
    }

    pub fn stream_epoch(&self) -> &str {
        &self.stream_epoch
    }

    pub fn emit(&self, event: StreamEvent) {
        let mut turns = self.lock_turns();
        if let Some(parent) = &event.agent.parent_agent_id {
            self.parents
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(event.session_id.clone(), parent.clone());
        }
        Self::push(&mut turns, &event.session_id, &event);
        if !event.agent.is_root() {
            let parents = self.parents.lock().unwrap_or_else(PoisonError::into_inner);
            let mut ancestor = event.agent.parent_agent_id.clone();
            while let Some(session) = ancestor {
                Self::push(&mut turns, &session, &event);
                ancestor = parents.get(&session).cloned();
            }
        }
    }

    fn push(turns: &mut HashMap<String, TurnChannel>, session_id: &str, event: &StreamEvent) {
        let channel = turns
            .entry(session_id.to_string())
            .or_insert_with(TurnChannel::new);
        channel.next_cursor = channel.next_cursor.saturating_add(1);
        if matches!(&event.payload, StreamPayload::TurnStart { .. })
            && event.session_id == session_id
        {
            channel.active_turn_start = Some((event.turn_id.clone(), channel.next_cursor));
        }
        let replay = ReplayEvent {
            cursor: channel.next_cursor,
            approx_bytes: serde_json::to_vec(event).map_or(0, |body| body.len()),
            event: event.clone(),
        };
        channel.history_bytes = channel.history_bytes.saturating_add(replay.approx_bytes);
        channel.history.push_back(replay.clone());
        channel.trim();
        if matches!(&replay.event.payload, StreamPayload::TurnEnd { .. })
            && replay.event.session_id == session_id
            && channel
                .active_turn_start
                .as_ref()
                .is_some_and(|(turn_id, _)| turn_id == &replay.event.turn_id)
        {
            channel.active_turn_start = None;
        }
        let _ = channel.live.send(event.clone());
        let _ = channel.replay_live.send(replay);
    }

    pub fn notify(&self, notice: StateNotice) {
        let _ = self.notices.send(notice);
    }

    pub fn subscribe_inits(&self) -> broadcast::Receiver<WorkspaceInitProgress> {
        self.inits
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .live
            .subscribe()
    }

    pub fn subscribe_inits_with_latest(
        &self,
    ) -> (
        Vec<WorkspaceInitProgress>,
        broadcast::Receiver<WorkspaceInitProgress>,
    ) {
        let inits = self.inits.lock().unwrap_or_else(PoisonError::into_inner);
        let receiver = inits.live.subscribe();
        let mut latest: Vec<_> = inits.latest.values().cloned().collect();
        latest.sort_by(|a, b| a.workspace_id.cmp(&b.workspace_id));
        (latest, receiver)
    }

    pub fn init_progress(&self, progress: WorkspaceInitProgress) {
        let mut inits = self.inits.lock().unwrap_or_else(PoisonError::into_inner);
        inits
            .latest
            .insert(progress.workspace_id.clone(), progress.clone());
        let _ = inits.live.send(progress);
    }

    pub fn drop_session(&self, session_id: &str) {
        self.lock_turns().remove(session_id);
    }

    fn lock_turns(&self) -> std::sync::MutexGuard<'_, HashMap<String, TurnChannel>> {
        self.turns.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Default for EventHub {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zlogic_protocol::stream::{
        AgentRef, InitPhase, ModelRef, NoticeLevel, StateChange, StreamPayload, TurnStats,
        TurnStatus,
    };

    fn event(session: &str) -> StreamEvent {
        StreamEvent {
            seq: 1,
            session_id: session.into(),
            turn_id: "t1".into(),
            agent: AgentRef::root(),
            payload: StreamPayload::TurnEnd {
                status: TurnStatus::Completed,
                reason: None,
                stats: TurnStats::default(),
            },
        }
    }

    fn turn_start(session: &str, turn: &str) -> StreamEvent {
        StreamEvent {
            seq: 1,
            session_id: session.into(),
            turn_id: turn.into(),
            agent: AgentRef::root(),
            payload: StreamPayload::TurnStart {
                model: ModelRef {
                    provider_id: "test".into(),
                    model_id: "test".into(),
                    display_name: "test".into(),
                },
                resumed: false,
                proactive: false,
            },
        }
    }

    fn console_delta(task: &str, chunk: &str) -> TaskOutputDelta {
        TaskOutputDelta {
            task_id: task.into(),
            stream: zlogic_protocol::stream::OutputStream::Stdout,
            chunk: chunk.into(),
        }
    }

    #[test]
    fn a_task_console_subscription_replays_the_buffer_then_follows_live() {
        let hub = EventHub::new();
        hub.emit_task(console_delta("t1", "compiling\n"));

        let (buffered, mut live) = hub.subscribe_task("t1");
        assert_eq!(
            buffered
                .iter()
                .map(|d| d.chunk.as_str())
                .collect::<String>(),
            "compiling\n"
        );

        hub.emit_task(console_delta("t1", "done\n"));
        assert_eq!(live.try_recv().unwrap().chunk, "done\n");

        hub.emit_task(console_delta("t2", "other\n"));
        assert!(live.try_recv().is_err());
    }

    #[test]
    fn a_long_task_console_keeps_only_the_tail() {
        let hub = EventHub::new();
        for i in 0..TASK_REPLAY_CAPACITY + 10 {
            hub.emit_task(console_delta("t1", &format!("line {i}\n")));
        }
        let (buffered, _) = hub.subscribe_task("t1");
        assert_eq!(buffered.len(), TASK_REPLAY_CAPACITY);
        assert_eq!(buffered.first().unwrap().chunk, "line 10\n");
    }

    #[test]
    fn closed_task_consoles_are_reclaimed_before_running_ones() {
        let hub = EventHub::new();
        for i in 0..TASK_CHANNELS_KEPT {
            hub.emit_task(console_delta(&format!("t{i}"), "x\n"));
        }
        hub.close_task("t0");
        hub.emit_task(console_delta("extra", "y\n"));

        let (closed, _) = hub.subscribe_task("t0");
        assert!(closed.is_empty(), "the finished one should be reclaimed");
        let (running, _) = hub.subscribe_task("t1");
        assert_eq!(running.len(), 1, "a running buffer should not be touched");
    }

    fn child_event(session: &str, parent: &str, name: &str) -> StreamEvent {
        StreamEvent {
            seq: 1,
            session_id: session.into(),
            turn_id: "turn-child".into(),
            agent: AgentRef {
                agent_id: Some(session.into()),
                parent_agent_id: Some(parent.into()),
                name: name.into(),
            },
            payload: StreamPayload::Notice {
                level: NoticeLevel::Info,
                code: "c".into(),
                message: zlogic_protocol::LocalizedMessage::new("notice.c", "m"),
            },
        }
    }

    #[tokio::test]
    async fn events_go_only_to_that_session() {
        let hub = EventHub::new();
        let mut a = hub.subscribe_turns("s-a");
        let mut b = hub.subscribe_turns("s-b");

        hub.emit(event("s-a"));

        assert_eq!(a.recv().await.unwrap().session_id, "s-a");
        assert!(b.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_sub_agent_event_reaches_the_parent_channel() {
        let hub = EventHub::new();
        let mut parent = hub.subscribe_turns("parent");
        let mut child_own = hub.subscribe_turns("child");

        let ev = child_event("child", "parent", "researcher");
        hub.emit(ev.clone());

        let seen = parent.recv().await.unwrap();
        assert_eq!(
            seen.session_id, "child",
            "no forged identity: it still carries the child session's own id"
        );
        assert_eq!(seen.agent.name, "researcher");
        assert_eq!(seen.payload, ev.payload);
        assert_eq!(child_own.recv().await.unwrap().payload, ev.payload);
    }

    #[tokio::test]
    async fn nested_sub_agent_events_reach_every_ancestor() {
        let hub = EventHub::new();
        let mut root = hub.subscribe_turns("root");
        let mut middle = hub.subscribe_turns("middle");
        let mut leaf_own = hub.subscribe_turns("leaf");

        hub.emit(child_event("middle", "root", "mid"));
        let ev = child_event("leaf", "middle", "leafy");
        hub.emit(ev.clone());

        assert_eq!(root.recv().await.unwrap().session_id, "middle");
        let root_seen = root.recv().await.unwrap();
        assert_eq!(root_seen.session_id, "leaf");
        assert_eq!(middle.recv().await.unwrap().session_id, "middle");
        let middle_seen = middle.recv().await.unwrap();
        assert_eq!(middle_seen.session_id, "leaf");
        assert_eq!(leaf_own.recv().await.unwrap().payload, ev.payload);
        assert!(root.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_forwarded_child_turn_start_does_not_claim_the_parent_active_turn() {
        let hub = EventHub::new();
        hub.emit(turn_start("parent", "turn-parent"));

        let mut child = child_event("child", "parent", "researcher");
        child.payload = StreamPayload::TurnStart {
            model: ModelRef {
                provider_id: "test".into(),
                model_id: "test".into(),
                display_name: "test".into(),
            },
            resumed: false,
            proactive: false,
        };
        hub.emit(child);
        let mut child_end = child_event("child", "parent", "researcher");
        child_end.payload = StreamPayload::TurnEnd {
            status: TurnStatus::Completed,
            reason: None,
            stats: TurnStats::default(),
        };
        hub.emit(child_end);

        let attached = hub.subscribe_current_turn("parent");
        assert_eq!(attached.replay.len(), 3);
        assert_eq!(attached.replay[0].event.turn_id, "turn-parent");
        assert!(
            matches!(
                &attached.replay[2].event.payload,
                StreamPayload::TurnEnd { .. }
            ),
            "a sub-agent's interaction must also replay through the parent channel"
        );

        let mut parent_end = turn_start("parent", "turn-parent");
        parent_end.payload = StreamPayload::TurnEnd {
            status: TurnStatus::Completed,
            reason: None,
            stats: TurnStats::default(),
        };
        hub.emit(parent_end);
        let settled = hub.subscribe_current_turn("parent");
        assert!(settled.replay.is_empty());
    }

    #[tokio::test]
    async fn notices_reach_everyone() {
        let hub = EventHub::new();
        let mut one = hub.subscribe_notices();
        let mut two = hub.subscribe_notices();

        hub.notify(StateNotice {
            session_id: "s".into(),
            turn_id: None,
            change: StateChange::TurnStateChanged,
        });

        assert_eq!(
            one.recv().await.unwrap().change,
            StateChange::TurnStateChanged
        );
        assert_eq!(
            two.recv().await.unwrap().change,
            StateChange::TurnStateChanged
        );
    }

    #[test]
    fn emitting_with_no_subscriber_is_fine() {
        let hub = EventHub::new();
        hub.emit(event("nobody-home"));
    }

    #[tokio::test]
    async fn replay_uses_a_session_cursor_across_turn_sequence_resets() {
        let hub = EventHub::new();
        let mut first = event("s");
        first.turn_id = "turn-1".into();
        first.seq = 1;
        hub.emit(first);

        let initial = hub.subscribe_turns_after("s", None);
        let cursor = ReplayCursor {
            epoch: initial.epoch,
            cursor: 1,
        };

        let mut second = event("s");
        second.turn_id = "turn-2".into();
        second.seq = 1;
        hub.emit(second);

        let resumed = hub.subscribe_turns_after("s", Some(&cursor));
        assert_eq!(resumed.replay.len(), 1);
        assert_eq!(resumed.replay[0].cursor, 2);
        assert_eq!(resumed.replay[0].event.turn_id, "turn-2");
        assert_eq!(resumed.replay[0].event.seq, 1);
        assert_eq!(resumed.gap, None);
    }

    #[tokio::test]
    async fn replay_snapshot_and_live_subscription_have_no_gap() {
        let hub = EventHub::new();
        hub.emit(event("s"));
        let cursor = ReplayCursor {
            epoch: hub.stream_epoch.clone(),
            cursor: 0,
        };
        let mut resumed = hub.subscribe_turns_after("s", Some(&cursor));
        assert_eq!(resumed.replay.len(), 1);

        let mut next = event("s");
        next.seq = 2;
        hub.emit(next);
        let live = resumed.receiver.recv().await.unwrap();
        assert_eq!(live.cursor, 2);
        assert_eq!(live.event.seq, 2);
    }

    #[tokio::test]
    async fn cold_attach_replays_only_the_complete_active_turn() {
        let hub = EventHub::new();
        hub.emit(turn_start("s", "turn-1"));
        let mut ended = event("s");
        ended.turn_id = "turn-1".into();
        ended.seq = 2;
        hub.emit(ended);

        hub.emit(turn_start("s", "turn-2"));
        let mut delta = event("s");
        delta.turn_id = "turn-2".into();
        delta.seq = 2;
        delta.payload = StreamPayload::BlockDelta {
            block_id: "round:0".into(),
            delta: "hello".into(),
        };
        hub.emit(delta);

        let mut attached = hub.subscribe_current_turn("s");
        assert_eq!(attached.replay.len(), 2);
        assert!(
            attached
                .replay
                .iter()
                .all(|item| item.event.turn_id == "turn-2")
        );

        let mut live = event("s");
        live.turn_id = "turn-2".into();
        live.seq = 3;
        hub.emit(live);
        assert_eq!(attached.receiver.recv().await.unwrap().event.seq, 3);
    }

    #[test]
    fn a_cursor_from_an_old_process_reports_a_gap() {
        let hub = EventHub::new();
        let resumed = hub.subscribe_turns_after(
            "s",
            Some(&ReplayCursor {
                epoch: "old-process".into(),
                cursor: 9,
            }),
        );
        assert_eq!(resumed.gap, Some(ReplayGap::EpochChanged));
        assert!(resumed.replay.is_empty());
    }

    #[test]
    fn cursor_older_than_the_bounded_window_requires_authoritative_recovery() {
        let hub = EventHub::new();
        for seq in 1..=(TURN_REPLAY_CAPACITY as u64 + 2) {
            let mut item = event("s");
            item.seq = seq;
            hub.emit(item);
        }
        let resumed = hub.subscribe_turns_after(
            "s",
            Some(&ReplayCursor {
                epoch: hub.stream_epoch.clone(),
                cursor: 1,
            }),
        );
        assert_eq!(
            resumed.gap,
            Some(ReplayGap::CursorTooOld {
                oldest_available: 3,
            })
        );
        assert!(resumed.replay.is_empty());
    }

    #[test]
    fn replay_window_is_also_bounded_by_serialized_bytes() {
        let hub = EventHub::new();
        for seq in 1..=2 {
            let mut item = event("s");
            item.seq = seq;
            item.payload = StreamPayload::BlockDelta {
                block_id: "large".into(),
                delta: "x".repeat(TURN_REPLAY_MAX_BYTES / 2 + 1),
            };
            hub.emit(item);
        }

        let resumed = hub.subscribe_turns_after(
            "s",
            Some(&ReplayCursor {
                epoch: hub.stream_epoch.clone(),
                cursor: 0,
            }),
        );
        assert_eq!(
            resumed.gap,
            Some(ReplayGap::CursorTooOld {
                oldest_available: 2,
            })
        );
    }

    #[test]
    fn cold_attach_replays_the_whole_active_turn_past_the_count_cap() {
        let hub = EventHub::new();
        hub.emit(turn_start("s", "turn-active"));
        for seq in 2..=(TURN_REPLAY_CAPACITY as u64 + 2) {
            let mut item = event("s");
            item.turn_id = "turn-active".into();
            item.seq = seq;
            item.payload = StreamPayload::BlockDelta {
                block_id: "round:0".into(),
                delta: "x".into(),
            };
            hub.emit(item);
        }

        let attached = hub.subscribe_current_turn("s");
        assert_eq!(attached.gap, None);
        assert_eq!(attached.replay.len(), TURN_REPLAY_CAPACITY + 2);
        assert!(matches!(
            attached.replay[0].event.payload,
            StreamPayload::TurnStart { .. }
        ));
    }

    #[test]
    fn the_byte_cap_is_hard_even_for_the_active_turn() {
        let hub = EventHub::new();
        hub.emit(turn_start("s", "turn-active"));
        for seq in 2..=3 {
            let mut item = event("s");
            item.turn_id = "turn-active".into();
            item.seq = seq;
            item.payload = StreamPayload::BlockDelta {
                block_id: "large".into(),
                delta: "x".repeat(TURN_REPLAY_MAX_BYTES / 2 + 1),
            };
            hub.emit(item);
        }

        let attached = hub.subscribe_current_turn("s");
        assert_eq!(attached.replay.len(), 0);
        match attached.gap {
            Some(ReplayGap::CursorTooOld { oldest_available }) => {
                assert!(oldest_available >= 2, "got {oldest_available}");
            }
            other => panic!("expected a hard byte-cap gap, got {other:?}"),
        }
    }

    #[test]
    fn an_upstream_gap_discards_the_incomplete_cold_attach_projection() {
        let hub = EventHub::new();
        hub.emit(turn_start("s1", "turn-1"));
        let mut partial = event("s1");
        partial.turn_id = "turn-1".into();
        partial.seq = 2;
        partial.payload = StreamPayload::BlockDelta {
            block_id: "round-1:0".into(),
            delta: "partial".into(),
        };
        hub.emit(partial);

        hub.signal_turn_gap("s1", 4);

        let attached = hub.subscribe_current_turn("s1");
        assert!(attached.replay.is_empty());
        assert!(attached.gap.is_none());
    }

    #[tokio::test]
    async fn workspace_init_subscriber_gets_latest_then_live_without_a_gap() {
        let hub = EventHub::new();
        hub.init_progress(WorkspaceInitProgress {
            workspace_id: "w".into(),
            root: "/tmp/w".into(),
            phase: InitPhase::Scanning { files: 3, bytes: 9 },
        });
        let (latest, mut live) = hub.subscribe_inits_with_latest();
        assert_eq!(latest.len(), 1);

        hub.init_progress(WorkspaceInitProgress {
            workspace_id: "w".into(),
            root: "/tmp/w".into(),
            phase: InitPhase::Done {
                files: 4,
                bytes: 12,
                skipped: 0,
                elapsed_ms: 1,
                unchanged: false,
            },
        });
        assert!(matches!(
            live.recv().await.unwrap().phase,
            InitPhase::Done { .. }
        ));
    }
}
