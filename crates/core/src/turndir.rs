//! Where a turn's files go: `<workspace>/.zlogic/cache/<session>/<turn>/`.
//!
//! One definition, used by the prompt that tells the model, the environment variable it writes
//! through, the emitter that reports what landed there, and the retention sweep that eventually
//! removes it. Four places computing a path is how a convention stops being one.
//!
//! # Why both names are the id's tail
//!
//! A session id is a v7 UUID, and a v7 UUID's **first twelve hex digits are its millisecond
//! timestamp** — the head is a clock reading, not entropy. The first 8 of them are the timestamp's
//! high 32 bits, so every id minted inside one **65.5-second** window shares them: a leading 8 is
//! not a short id, it is a bucket. Two turns a minute apart landing in one `deliverables/` folder
//! means the first turn's files are read back as the second turn's, which is the one job that
//! folder has. The tail is `rand_b` — 62 random bits, of which 8 are 32.
//!
//! The head was worth keeping while the folder was named with all thirty-six characters, because
//! it sorts by creation time (v7 ids do) and can be read off a session list and pasted back. At
//! eight characters truncation destroys both, so all a leading-8 name still buys is the characters
//! it spends on collisions.
//!
//! # Why a turn and not a session
//!
//! The artifact list is drawn when a turn ends, and a session's delivery folder would answer with
//! every earlier turn's leftovers as well. The turn is also the granularity the rest of the data
//! already uses: entries carry `turn_id`, so a path in this layout is joinable to the transcript
//! without a second lookup.

use std::path::{Path, PathBuf};

use zlogic_protocol::{SessionId, TurnId};

/// How many characters of the id name its folder.
const DIR_CHARS: usize = 8;

/// The folder a session's temporary files go in, and the parent of every turn folder under it.
pub fn session_cache_dir(root: &Path, session_id: Option<SessionId>) -> PathBuf {
    let cache = root.join(".zlogic").join("cache");
    match session_id {
        Some(id) => cache.join(dir_name(id)),
        None => cache,
    }
}

/// This turn's folder: `<cache>/<session tail>/<turn tail>`.
pub fn turn_cache_dir(root: &Path, session_id: SessionId, turn_id: TurnId) -> PathBuf {
    session_cache_dir(root, Some(session_id)).join(dir_name(turn_id))
}

/// The one subfolder the engine reads back. Everything else under the turn folder is scratch.
pub fn deliverables_dir(root: &Path, session_id: SessionId, turn_id: TurnId) -> PathBuf {
    turn_cache_dir(root, session_id, turn_id).join(DELIVERABLES)
}

/// The subfolder name, exported because the prompt and the environment variable both name it.
pub const DELIVERABLES: &str = "deliverables";

fn dir_name(id: impl std::fmt::Display) -> String {
    let id = id.to_string();
    id.chars().skip(id.chars().count().saturating_sub(DIR_CHARS)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> (SessionId, TurnId) {
        (SessionId::new(), TurnId::new())
    }

    #[test]
    fn the_session_folder_is_the_tail_of_the_id() {
        let (session, _) = ids();
        let root = Path::new("/w");
        let id = session.to_string();
        let name = dir_name(session);
        assert_eq!(name.len(), DIR_CHARS);
        assert!(id.ends_with(&name), "{id} should end with {name}");
        assert_eq!(
            session_cache_dir(root, Some(session)),
            root.join(".zlogic/cache").join(name)
        );
        assert_eq!(
            session_cache_dir(root, None),
            root.join(".zlogic/cache"),
            "no session id means the shared root, which is what the pruner walks"
        );
    }

    /// The tail, not the head: a v7 prefix is the clock, and 20,000 ids minted 500-per-millisecond
    /// share one.
    #[test]
    fn the_turn_folder_is_the_tail_of_the_id() {
        let (session, turn) = ids();
        let id = turn.to_string();
        let name = dir_name(turn);
        assert_eq!(name.len(), DIR_CHARS);
        assert!(id.ends_with(&name), "{id} should end with {name}");
        assert_eq!(
            turn_cache_dir(Path::new("/w"), session, turn),
            Path::new("/w")
                .join(".zlogic/cache")
                .join(dir_name(session))
                .join(name)
        );
    }

    #[test]
    fn the_delivery_folder_is_the_one_the_engine_reads() {
        let (session, turn) = ids();
        assert_eq!(
            deliverables_dir(Path::new("/w"), session, turn),
            turn_cache_dir(Path::new("/w"), session, turn).join("deliverables")
        );
    }

    /// The case the tail exists for: turns minted back to back, which a leading 8 would drop into
    /// one folder.
    #[test]
    fn folder_names_do_not_repeat() {
        let names: std::collections::HashSet<String> =
            (0..2000).map(|_| dir_name(TurnId::new())).collect();
        assert_eq!(names.len(), 2000, "two turns must not share a name");
    }
}
