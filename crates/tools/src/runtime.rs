use std::path::{Path, PathBuf};

pub fn executable_on_path(name: &str) -> Option<PathBuf> {
    executable_on_path_in(std::env::var_os("PATH").as_deref(), name)
}

pub fn executable_on_path_in(path: Option<&std::ffi::OsStr>, name: &str) -> Option<PathBuf> {
    let path = path?;
    let mut candidates = vec![PathBuf::from(name)];
    if cfg!(windows) && candidates[0].extension().is_none() {
        candidates.push(candidates[0].clone().with_extension("exe"));
    }
    std::env::split_paths(path)
        .flat_map(|dir| candidates.iter().map(move |candidate| dir.join(candidate)))
        .find(|candidate| candidate.is_file() && is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|meta| meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_lookup_finds_an_executable_by_bare_name() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join(if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        });
        std::fs::write(&exe, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = std::env::join_paths([dir.path()]).unwrap();
        assert_eq!(executable_on_path_in(Some(&path), "ffmpeg"), Some(exe));
        assert_eq!(executable_on_path_in(Some(&path), "ffprobe"), None);
        assert_eq!(executable_on_path_in(None, "ffmpeg"), None);
    }
}
