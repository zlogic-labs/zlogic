//! |---|---|---|

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::{ConfigError, Result};

/// The name zlogic itself resolves under, and the one a process that never calls
/// [`Dirs::set_app_name`] keeps using.
const DEFAULT_APP: &str = "zlogic";

/// The name [`Dirs::discover`] resolves under, claimed once per process.
static APP: OnceLock<String> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dirs {
    pub config: PathBuf,
    pub data: PathBuf,
    pub state: PathBuf,
    pub cache: PathBuf,
}

impl Dirs {
    /// Claims the name the four directories are resolved under: `~/.config/<app>`,
    /// `~/.local/share/<app>`, `~/.local/state/<app>`, `~/.cache/<app>`.
    ///
    /// Two hosts built on this engine must not share those directories. Sharing
    /// them means sharing one `state.db`, one credential vault and one
    /// `policy.yaml` — and letting an update in one host migrate the database the
    /// other host is still reading. This is where a host claims its own name.
    ///
    /// Process-wide rather than a per-call argument, so that the CLI, the engine's
    /// own fallback and a GUI host cannot end up in different directories within
    /// one process. Claiming the same name twice is accepted; a second, different
    /// name is an error rather than a silent switch of directories.
    pub fn set_app_name(app: impl Into<String>) -> Result<()> {
        let app = app.into();
        check_app_name(&app)?;
        match APP.set(app.clone()) {
            Ok(()) => Ok(()),
            Err(_) if Self::app_name() == app => Ok(()),
            Err(_) => Err(ConfigError::AppName {
                app,
                reason: format!("this process already uses {:?}", Self::app_name()),
            }),
        }
    }

    /// The name [`Dirs::discover`] resolves under.
    pub fn app_name() -> &'static str {
        APP.get().map_or(DEFAULT_APP, String::as_str)
    }

    pub fn discover() -> Result<Self> {
        let env = |k: &str| std::env::var(k).ok();
        Self::discover_in(&env, ::dirs::home_dir(), Self::app_name())
    }

    /// [`Dirs::discover`] under `app` for this call only, without claiming the name
    /// process-wide.
    pub fn discover_as(app: &str) -> Result<Self> {
        check_app_name(app)?;
        let env = |k: &str| std::env::var(k).ok();
        Self::discover_in(&env, ::dirs::home_dir(), app)
    }

    pub fn discover_with(env: impl Fn(&str) -> Option<String>) -> Result<Self> {
        Self::discover_in(&env, None, Self::app_name())
    }

    fn discover_in(
        env: &impl Fn(&str) -> Option<String>,
        os_home: Option<PathBuf>,
        app: &str,
    ) -> Result<Self> {
        if let Some(home) = env(&home_var(app)).filter(|s| !s.is_empty()) {
            return Ok(Self::under(PathBuf::from(home)));
        }
        if let Some(home) = resolve_home(env).or(os_home) {
            return Ok(Self::from_home(home, env, app));
        }
        Err(ConfigError::Env(
            "could not determine the home directory (HOME / USERPROFILE / HOMEDRIVE+HOMEPATH not set)"
                .into(),
        ))
    }

    fn from_home(home: PathBuf, env: &impl Fn(&str) -> Option<String>, app: &str) -> Self {
        let xdg = |var: &str, default: &str| -> PathBuf {
            match env(var).filter(|s| !s.is_empty()) {
                Some(v) => PathBuf::from(v).join(app),
                None => home.join(default).join(app),
            }
        };
        Self {
            config: xdg("XDG_CONFIG_HOME", ".config"),
            data: xdg("XDG_DATA_HOME", ".local/share"),
            state: xdg("XDG_STATE_HOME", ".local/state"),
            cache: xdg("XDG_CACHE_HOME", ".cache"),
        }
    }

    pub fn under(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref();
        Self {
            config: root.join("config"),
            data: root.join("data"),
            state: root.join("state"),
            cache: root.join("cache"),
        }
    }

    pub fn ensure(&self) -> Result<()> {
        for d in [&self.config, &self.data, &self.state, &self.cache] {
            std::fs::create_dir_all(d).map_err(|e| ConfigError::Io {
                path: d.clone(),
                source: e,
            })?;
        }
        Ok(())
    }

    pub fn config_file(&self) -> PathBuf {
        self.config.join("config.yaml")
    }

    pub fn models_file(&self) -> PathBuf {
        self.config.join("models.yaml")
    }

    pub fn state_db(&self) -> PathBuf {
        self.state.join("state.db")
    }

    pub fn security_dir(&self) -> PathBuf {
        self.state.join("security")
    }

    pub fn master_key_file(&self) -> PathBuf {
        self.security_dir().join(".master")
    }

    pub fn secrets_blob(&self) -> PathBuf {
        self.security_dir().join(".key.enc")
    }

    pub fn objects(&self) -> PathBuf {
        self.data.join("objects")
    }

    pub fn workspace(&self, workspace_id: &str) -> PathBuf {
        self.data.join("workspaces").join(workspace_id)
    }

    pub fn logs(&self) -> PathBuf {
        self.state.join("logs")
    }
}

/// The name is joined onto the home directory, so it has to be a single, plain
/// path component: anything that could climb out of it (`/`, `\`, `..`) must not
/// get through.
fn check_app_name(app: &str) -> Result<()> {
    let plain = !app.is_empty()
        && app != "."
        && app != ".."
        && app
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_');
    if plain {
        return Ok(());
    }
    Err(ConfigError::AppName {
        app: app.to_owned(),
        reason: "must be non-empty and use only ASCII letters, digits, '.', '-' and '_'".into(),
    })
}

/// The variable that collapses everything under one root — `ZLOGIC_HOME` for
/// zlogic. Derived from the name on purpose: a second host must not inherit the
/// first one's override, or a stray `ZLOGIC_HOME` would put it right back on the
/// shared directory.
fn home_var(app: &str) -> String {
    let name: String = app
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("{name}_HOME")
}

fn resolve_home(env: &impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    for key in ["HOME", "USERPROFILE"] {
        if let Some(v) = env(key).filter(|s| !s.is_empty()) {
            return Some(PathBuf::from(v));
        }
    }
    match (
        env("HOMEDRIVE").filter(|s| !s.is_empty()),
        env("HOMEPATH").filter(|s| !s.is_empty()),
    ) {
        (Some(drive), Some(path)) => Some(PathBuf::from(drive).join(path)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| m.get(k).cloned()
    }

    #[test]
    fn zlogic_home_overrides_everything() {
        let d = Dirs::discover_with(env_of(&[
            ("ZLOGIC_HOME", "/opt/zlogic"),
            ("HOME", "/home/u"),
            ("XDG_CONFIG_HOME", "/ignored"),
        ]))
        .unwrap();
        assert_eq!(d.config, PathBuf::from("/opt/zlogic/config"));
        assert_eq!(d.state, PathBuf::from("/opt/zlogic/state"));
    }

    #[test]
    fn an_explicit_name_moves_all_four_directories() {
        let d = Dirs::discover_in(&env_of(&[("HOME", "/home/u")]), None, "mochuno").unwrap();
        assert_eq!(d.config, PathBuf::from("/home/u/.config/mochuno"));
        assert_eq!(d.data, PathBuf::from("/home/u/.local/share/mochuno"));
        assert_eq!(d.state, PathBuf::from("/home/u/.local/state/mochuno"));
        assert_eq!(d.cache, PathBuf::from("/home/u/.cache/mochuno"));
    }

    #[test]
    fn the_home_override_follows_the_name_instead_of_zlogic() {
        let d = Dirs::discover_in(
            &env_of(&[
                ("HOME", "/home/u"),
                ("MOCHUNO_HOME", "/opt/mochuno"),
                ("ZLOGIC_HOME", "/opt/zlogic"),
            ]),
            None,
            "mochuno",
        )
        .unwrap();
        assert_eq!(d.config, PathBuf::from("/opt/mochuno/config"));
        assert_eq!(d.state, PathBuf::from("/opt/mochuno/state"));
    }

    #[test]
    fn a_name_that_could_climb_out_of_the_home_is_rejected() {
        for app in ["", ".", "..", "../elsewhere", "a/b", r"a\b", "with space"] {
            assert!(check_app_name(app).is_err(), "{app:?} must be rejected");
        }
        for app in ["zlogic", "mochuno", "my-app_2", "app.v2"] {
            assert!(check_app_name(app).is_ok(), "{app:?} must be accepted");
        }
    }

    #[test]
    fn xdg_vars_win_over_platform_defaults() {
        let d = Dirs::discover_with(env_of(&[
            ("HOME", "/home/u"),
            ("XDG_CONFIG_HOME", "/cfg"),
            ("XDG_STATE_HOME", "/st"),
        ]))
        .unwrap();
        assert_eq!(d.config, PathBuf::from("/cfg/zlogic"));
        assert_eq!(d.state, PathBuf::from("/st/zlogic"));
        assert_eq!(d.data, PathBuf::from("/home/u/.local/share/zlogic"));
        assert_eq!(d.cache, PathBuf::from("/home/u/.cache/zlogic"));
    }

    #[test]
    fn empty_env_vars_are_treated_as_unset() {
        let d =
            Dirs::discover_with(env_of(&[("HOME", "/home/u"), ("XDG_CONFIG_HOME", "")])).unwrap();
        assert_eq!(d.config, PathBuf::from("/home/u/.config/zlogic"));
    }

    #[test]
    fn missing_home_is_an_error_not_a_relative_path() {
        assert!(Dirs::discover_with(env_of(&[])).is_err());
    }

    /// The Windows fallbacks are consulted on every host — what is under test is which variable
    /// wins — so the expectation is built with `join` instead of being written as a Windows
    /// literal, which only matches where the host joins paths the Windows way.
    #[test]
    fn windows_userprofile_is_home_when_home_is_missing() {
        let home = PathBuf::from(r"C:\Users\u");
        let d =
            Dirs::discover_with(env_of(&[("HOME", ""), ("USERPROFILE", r"C:\Users\u")])).unwrap();
        assert_eq!(d.config, home.join(".config").join("zlogic"));
        assert_eq!(d.state, home.join(".local/state").join("zlogic"));
        assert_eq!(d.data, home.join(".local/share").join("zlogic"));
    }

    #[test]
    fn windows_homedrive_homepath_is_home_as_a_last_resort() {
        let home = PathBuf::from("C:").join(r"\Users\u");
        let d =
            Dirs::discover_with(env_of(&[("HOMEDRIVE", "C:"), ("HOMEPATH", r"\Users\u")])).unwrap();
        assert_eq!(d.config, home.join(".config").join("zlogic"));

        assert!(Dirs::discover_with(env_of(&[("HOMEDRIVE", "C:")])).is_err());
        assert!(Dirs::discover_with(env_of(&[("HOMEPATH", r"\Users\u")])).is_err());
    }

    #[test]
    fn empty_userprofile_is_treated_as_unset() {
        let d = Dirs::discover_with(env_of(&[("HOME", "/home/u"), ("USERPROFILE", "")])).unwrap();
        assert_eq!(d.config, PathBuf::from("/home/u/.config/zlogic"));
    }

    #[test]
    fn state_db_is_not_under_cache() {
        let d = Dirs::under("/tmp/x");
        assert!(!d.state_db().starts_with(&d.cache));
        assert!(d.state_db().starts_with(&d.state));
        assert!(!d.objects().starts_with(&d.cache));
    }

    #[test]
    fn secrets_live_under_state_security() {
        let d = Dirs::under("/tmp/x");
        assert!(d.master_key_file().starts_with(&d.state));
        assert!(d.secrets_blob().starts_with(&d.state));
        assert!(!d.master_key_file().starts_with(&d.cache));
        assert_eq!(
            d.master_key_file().parent(),
            Some(d.security_dir().as_path())
        );
        assert_eq!(d.secrets_blob().parent(), Some(d.security_dir().as_path()));
    }

    #[test]
    fn ensure_creates_all_four() {
        let tmp = tempfile::tempdir().unwrap();
        let d = Dirs::under(tmp.path());
        d.ensure().unwrap();
        for p in [&d.config, &d.data, &d.state, &d.cache] {
            assert!(p.is_dir(), "{p:?} must be created");
        }
    }
}
