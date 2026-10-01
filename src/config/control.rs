use serde::{Deserialize, Serialize};

use crate::store::ConfigKey;

/// Whether the app serves its control socket: Settings → Command line
/// access. The key is the one the HTTP API's settings had, so the choice
/// carries over; the port and token stored with it then are ignored, and
/// dropped when it is next written.
pub const COMMAND_LINE_ACCESS: ConfigKey<CommandLineAccess> = ConfigKey::new("core.api");

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CommandLineAccess {
    pub enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_http_apis_choice_carries_over() {
        let stored: CommandLineAccess =
            serde_json::from_str(r#"{"enabled":true,"port":25000,"token":"s3cret"}"#).unwrap();
        assert!(stored.enabled);
        assert_eq!(
            serde_json::to_string(&stored).unwrap(),
            r#"{"enabled":true}"#
        );
    }
}
