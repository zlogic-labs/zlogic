//! Session tools — acting on the conversation rather than on the machine.
//! [`AskUser`] touches neither the filesystem nor the network. What makes it a domain of its own is
//! its subject: the person having the conversation. Everything else in this crate acts on the
//! world and reports back; this one suspends and waits for an answer.
//! It is also the only place a **sub-agent** can reach the user at all. A sub-agent has no channel
//! of its own, so without this it has to guess and return a conclusion resting on an assumption
//! nobody checked.
//! [`EnterWorktree`] and [`ExitWorktree`] have the same subject from the other side: they do not
//! act on a file the model named, they change **where the session acts** — every later tool call
//! resolves its paths somewhere else. A migration of the conversation's working directory belongs
//! here rather than with the file tools, which all take a path and touch nothing but it.

pub mod ask_user;
pub mod memory;
pub mod skill;
pub mod worktree;

use std::sync::Arc;

pub use ask_user::AskUser;
pub use memory::{MemoryHost, MemoryUpdate};
pub use skill::{LoadedSkill, Skill, SkillHost};
pub use worktree::{
    EnterWorktree, ExitAction, ExitWorktree, ExitedWorktree, WorktreeChanges, WorktreeHost,
    WorktreeState,
};

use crate::Tool;

pub fn all() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(AskUser),
        Arc::new(Skill),
        Arc::new(EnterWorktree),
        Arc::new(ExitWorktree),
    ]
}
