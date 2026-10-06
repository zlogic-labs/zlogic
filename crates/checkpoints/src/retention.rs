//! Keeping the store finite.
//!
//! Snapshots are cheap to repeat and expensive to keep forever, so the policy is three caps — age,
//! count, bytes — and the first one reached wins. Dropping a snapshot cannot simply forget it: the
//! chain is linear, so the oldest kept commit still has the dropped ones as ancestors and they
//! would stay alive. The kept snapshots are therefore rewritten as a fresh chain over the same
//! trees, which is cheap (a commit object is a few hundred bytes and the trees are shared) and
//! leaves the dropped commit objects unreferenced, at which point their files are removed.

use std::collections::HashSet;
use std::path::Path;

use git2::{Commit, Oid, Repository, Signature};

use crate::store::{CheckpointError, Config, HEAD_REF, Snapshot, decode, encode, walk_from_tip};

const SIGNATURE_NAME: &str = "Zlogic Checkpoint";
const SIGNATURE_EMAIL: &str = "checkpoint@zlogic.invalid";

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SweepReport {
    pub kept: usize,
    pub dropped: usize,
    /// The bytes the kept snapshots account for, counted once per distinct blob.
    pub bytes: u64,
}

pub fn sweep(repo: &Repository, config: &Config, now: i64) -> Result<SweepReport, CheckpointError> {
    let chain = chain(repo)?;
    if chain.is_empty() {
        return Ok(SweepReport::default());
    }
    let cutoff = now - i64::from(config.retention_days) * 86_400;
    let ceiling = if config.max_bytes == 0 {
        u64::MAX
    } else {
        config.max_bytes
    };
    let over_budget = directory_size(repo.path()) > ceiling / 10 * 9;

    let mut keep = 0usize;
    for (index, (_, snapshot)) in chain.iter().enumerate() {
        if index >= config.max_snapshots || snapshot.at < cutoff {
            break;
        }
        keep += 1;
    }
    if over_budget {
        keep = keep.min(by_budget(repo, &chain, keep, ceiling));
    }

    if keep == chain.len() {
        return Ok(SweepReport {
            kept: keep,
            dropped: 0,
            bytes: unique_blob_bytes(repo, &chain[..keep]),
        });
    }

    let signature = Signature::now(SIGNATURE_NAME, SIGNATURE_EMAIL)?;
    let mut parent: Option<Commit> = None;
    let mut tip = None;
    for (_, snapshot) in chain[..keep].iter().rev() {
        let tree = repo.find_tree(Oid::from_str(&snapshot.tree)?)?;
        let parents = parent.iter().collect::<Vec<&Commit>>();
        let id = repo.commit(
            None,
            &signature,
            &signature,
            &encode(snapshot),
            &tree,
            &parents,
        )?;
        parent = Some(repo.find_commit(id)?);
        tip = Some(id);
    }
    match tip {
        Some(tip) => {
            repo.reference(HEAD_REF, tip, true, "zlogic: retention")?;
        }
        // Nothing survives the policy — a repository too large for even one snapshot. The ref has
        // to go with them: leaving it would point at a commit whose object is about to be deleted.
        None => {
            if let Ok(mut reference) = repo.find_reference(HEAD_REF) {
                reference.delete()?;
            }
        }
    }
    for (oid, _) in chain.iter() {
        let _ = remove_loose(repo, *oid);
    }
    // The dropped snapshots' own blobs are not on that list: a blob no kept tree names is a loose
    // file nothing else knows about, and it is most of what a busy repository accumulated. Nothing
    // else ever reclaims those — no `git gc` runs here — so the sweep does it itself.
    if let Err(error) = prune_loose(repo, &reachable(repo, &chain[..keep])) {
        tracing::warn!(target: "zlogic::checkpoints", %error, "checkpoint prune failed");
    }

    Ok(SweepReport {
        kept: keep,
        dropped: chain.len() - keep,
        bytes: unique_blob_bytes(repo, &chain[..keep]),
    })
}

/// Every object the kept snapshots can still reach: their commits, their trees, and the blobs
/// under those trees. An object outside this set is unreachable from the store's only ref, so
/// removing it cannot change what a restore reads.
fn reachable(repo: &Repository, chain: &[(Oid, Snapshot)]) -> HashSet<Oid> {
    let mut seen = HashSet::new();
    let mut stack = Vec::new();
    // The private index is the one other reference to a blob: a path the walker decides it
    // already has is not re-read, so an object the index still names has to outlive the sweep
    // even when no kept tree does.
    if let Ok(index) = repo.index() {
        for entry in index.iter() {
            seen.insert(entry.id);
        }
    }
    for (oid, snapshot) in chain {
        seen.insert(*oid);
        if let Ok(hex) = Oid::from_str(&snapshot.tree)
            && seen.insert(hex)
        {
            stack.push(hex);
        }
    }
    while let Some(oid) = stack.pop() {
        let Ok(tree) = repo.find_tree(oid) else { continue };
        for entry in tree.iter() {
            match entry.kind() {
                Some(git2::ObjectType::Tree) => {
                    if seen.insert(entry.id()) {
                        stack.push(entry.id());
                    }
                }
                Some(git2::ObjectType::Blob) => {
                    seen.insert(entry.id());
                }
                _ => {}
            }
        }
    }
    seen
}

/// Deletes the loose object files no kept tree reaches, and returns how many went.
///
/// Everything this store writes is loose — a commit, a tree and a blob per changed file — so
/// dropping the files is the whole of the compaction and no repack pass is needed over the objects
/// that are staying. `pack` is left alone for the same reason as everywhere else: those objects are
/// shared, and pruning inside a pack is `git gc`'s business.
fn prune_loose(repo: &Repository, keep: &HashSet<Oid>) -> Result<usize, CheckpointError> {
    let objects = repo.path().join("objects");
    let mut removed = 0usize;
    for fanout in std::fs::read_dir(&objects)?.flatten() {
        let name = fanout.file_name();
        let Some(hex) = name.to_str().filter(|hex| is_hex(hex, 2)) else {
            continue;
        };
        if !fanout.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        for file in std::fs::read_dir(fanout.path())?.flatten() {
            let Some(rest) = file.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !is_hex(&rest, 38) {
                continue;
            }
            let Ok(oid) = Oid::from_str(&format!("{hex}{rest}")) else {
                continue;
            };
            if keep.contains(&oid) {
                continue;
            }
            if std::fs::remove_file(file.path()).is_ok() {
                removed += 1;
            }
        }
    }
    Ok(removed)
}

fn is_hex(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Newest first.
fn chain(repo: &Repository) -> Result<Vec<(Oid, Snapshot)>, CheckpointError> {
    let mut out = Vec::new();
    for oid in walk_from_tip(repo)? {
        let commit = repo.find_commit(oid)?;
        if let Some(snapshot) = decode(&commit) {
            out.push((commit.id(), snapshot));
        }
    }
    Ok(out)
}

/// How many of the newest `limit` snapshots fit the byte ceiling.
fn by_budget(repo: &Repository, chain: &[(Oid, Snapshot)], limit: usize, ceiling: u64) -> usize {
    let mut seen = HashSet::new();
    let mut total = 0u64;
    let mut keep = 0usize;
    // Oldest first, so what a snapshot adds on top of its predecessor is what it costs.
    for (_, snapshot) in chain[..limit].iter().rev() {
        let Ok(tree) = Oid::from_str(&snapshot.tree).map(|oid| repo.find_tree(oid)) else {
            continue;
        };
        let Ok(tree) = tree else { continue };
        let mut added = 0u64;
        for blob in blobs(repo, &tree) {
            if seen.insert(blob) {
                added += repo.find_blob(blob).map(|b| b.size() as u64).unwrap_or(0);
            }
        }
        if !seen.is_empty() && total + added > ceiling {
            break;
        }
        total += added;
        keep += 1;
    }
    keep
}

fn unique_blob_bytes(repo: &Repository, chain: &[(Oid, Snapshot)]) -> u64 {
    let mut seen = HashSet::new();
    let mut total = 0u64;
    for (_, snapshot) in chain {
        let Ok(oid) = Oid::from_str(&snapshot.tree) else {
            continue;
        };
        let Ok(tree) = repo.find_tree(oid) else {
            continue;
        };
        for blob in blobs(repo, &tree) {
            if seen.insert(blob) {
                total += repo.find_blob(blob).map(|b| b.size() as u64).unwrap_or(0);
            }
        }
    }
    total
}

fn blobs(repo: &Repository, tree: &git2::Tree<'_>) -> Vec<Oid> {
    let mut out = Vec::new();
    let mut stack = vec![tree.clone()];
    while let Some(tree) = stack.pop() {
        for entry in tree.iter() {
            match entry.kind() {
                Some(git2::ObjectType::Tree) => {
                    if let Ok(object) = entry.to_object(repo)
                        && let Ok(child) = object.into_tree()
                    {
                        stack.push(child);
                    }
                }
                Some(git2::ObjectType::Blob) => out.push(entry.id()),
                _ => {}
            }
        }
    }
    out
}

pub(crate) fn directory_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            total += directory_size(&entry.path());
        } else if let Ok(meta) = entry.metadata() {
            total += meta.len();
        }
    }
    total
}

/// Removes a loose object file. Packed objects are left alone: they are shared with whatever
/// pack they live in, and an unreachable packed object is `git gc`'s business, not ours.
fn remove_loose(repo: &Repository, oid: Oid) -> Result<(), git2::Error> {
    let hex = oid.to_string();
    let path = repo.path().join("objects").join(&hex[..2]).join(&hex[2..]);
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(git2::Error::from_str(&error.to_string())),
    }
}
