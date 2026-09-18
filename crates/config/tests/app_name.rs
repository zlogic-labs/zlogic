//! The claimed app name is process-wide state, so it gets a test binary of its
//! own: claiming a name here would leak into every `discover_with` assertion in
//! the crate's unit tests if they shared a process.

use zlogic_config::{ConfigError, Dirs};

#[test]
fn a_host_claims_the_name_that_names_every_discovered_directory() {
    assert_eq!(
        Dirs::app_name(),
        "zlogic",
        "nothing has claimed a name until a host does"
    );

    Dirs::set_app_name("mochuno").unwrap();
    assert_eq!(Dirs::app_name(), "mochuno");

    // Claiming the same name again is idempotent; switching it is an error rather
    // than a silent move onto another state directory.
    Dirs::set_app_name("mochuno").unwrap();
    assert!(matches!(
        Dirs::set_app_name("somewhere-else"),
        Err(ConfigError::AppName { .. })
    ));
    assert_eq!(Dirs::app_name(), "mochuno");

    let dirs = Dirs::discover().unwrap();
    assert!(dirs.config.ends_with("mochuno"), "{:?}", dirs.config);
    assert!(dirs.data.ends_with("mochuno"), "{:?}", dirs.data);
    assert!(dirs.state.ends_with("mochuno"), "{:?}", dirs.state);
    assert!(dirs.cache.ends_with("mochuno"), "{:?}", dirs.cache);

    assert!(Dirs::set_app_name("../elsewhere").is_err());
}
