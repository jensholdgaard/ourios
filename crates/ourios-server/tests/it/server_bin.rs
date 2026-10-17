//! The `ourios-server` child process every served-binary test spawns.
//!
//! A developer's shell often points `OTEL_*` at a local Collector, and a
//! child inherits it: every test server would then export its telemetry
//! there as `service.name=ourios-server`, mixed into the real workload. So
//! the child starts with each inherited `OTEL_*` variable removed and the
//! SDK disabled. A test that needs telemetry sets the variables it wants on
//! the returned command, `OTEL_SDK_DISABLED` included.

use std::ffi::OsString;

use tokio::process::Command;

/// A command for the `ourios-server` binary under test, with no inherited
/// `OTEL_*` variable and `OTEL_SDK_DISABLED=true`.
pub fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ourios-server"));
    without_ambient_otel(&mut command, std::env::vars_os().map(|(key, _)| key));
    command
}

/// Remove every `OTEL_*` name in `ambient` from `command`'s environment and
/// disable the SDK.
fn without_ambient_otel(command: &mut Command, ambient: impl IntoIterator<Item = OsString>) {
    for key in ambient {
        if key.to_string_lossy().starts_with("OTEL_") {
            command.env_remove(key);
        }
    }
    command.env("OTEL_SDK_DISABLED", "true");
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::{OsStr, OsString};
    use std::path::Path;

    use super::*;

    fn envs(command: &Command) -> BTreeMap<OsString, Option<OsString>> {
        command
            .as_std()
            .get_envs()
            .map(|(key, value)| (key.to_owned(), value.map(OsStr::to_owned)))
            .collect()
    }

    #[test]
    fn an_inherited_otel_variable_is_stripped_and_the_sdk_disabled() {
        let mut command = Command::new("ourios-server");
        let ambient = [
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            "OTEL_RESOURCE_ATTRIBUTES",
            "OTEL_SERVICE_NAME",
            "PATH",
            "RUST_LOG",
        ]
        .map(OsString::from);
        without_ambient_otel(&mut command, ambient);

        let envs = envs(&command);
        for removed in [
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            "OTEL_RESOURCE_ATTRIBUTES",
            "OTEL_SERVICE_NAME",
        ] {
            assert_eq!(envs.get(OsStr::new(removed)), Some(&None), "{removed}");
        }
        assert_eq!(
            envs.get(OsStr::new("OTEL_SDK_DISABLED")),
            Some(&Some(OsString::from("true")))
        );
        assert!(!envs.contains_key(OsStr::new("PATH")));
        assert!(!envs.contains_key(OsStr::new("RUST_LOG")));
    }

    #[test]
    fn a_test_can_still_opt_back_in() {
        let mut command = command();
        command.env("OTEL_SDK_DISABLED", "false");
        assert_eq!(
            envs(&command).get(OsStr::new("OTEL_SDK_DISABLED")),
            Some(&Some(OsString::from("false")))
        );
    }

    /// Every served-binary test spawns through [`command`], so none
    /// inherits the developer's `OTEL_*` environment.
    #[test]
    fn no_test_spawns_the_binary_directly() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/it");
        let direct: Vec<_> = std::fs::read_dir(&dir)
            .expect("tests/it")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension() == Some(OsStr::new("rs")))
            .filter(|path| path.file_name() != Some(OsStr::new("server_bin.rs")))
            .filter(|path| {
                std::fs::read_to_string(path)
                    .expect("read")
                    .contains("Command::new(env!(\"CARGO_BIN_EXE_ourios-server\"))")
            })
            .collect();
        assert!(
            direct.is_empty(),
            "spawn via server_bin::command(): {direct:?}"
        );
    }
}
