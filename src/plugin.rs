//! The plugin: for now it only validates its settings and starts.

use trunk_recorder_plugin::{Host, Manifest, Plugin, Setup, topic};

use crate::config::{Config, SystemConfig};

pub struct Dvcf {
    #[allow(dead_code)]
    host: Host,
}

impl Plugin for Dvcf {
    type Config = Config;
    type SystemConfig = SystemConfig;

    fn manifest() -> Manifest {
        Manifest {
            name: "DVCF".into(),
            subscribe: vec![topic::CALL_CONCLUDED.into()],
            audio_formats: vec![],
            live_settings: true,
            ..trunk_recorder_plugin::manifest!()
        }
    }

    fn start(host: Host, setup: Setup<Config, SystemConfig>) -> Result<Self, String> {
        setup.config.validate()?;
        Ok(Dvcf { host })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use trunk_recorder_plugin::HostMessage;
    use trunk_recorder_plugin::testing;

    #[test]
    fn the_manifest_is_dvcf() {
        let m = trunk_recorder_plugin::describe::<Dvcf>();
        assert_eq!((m.id.as_str(), m.name.as_str(), m.api), ("dvcf", "DVCF", 1));
        assert_eq!(m.subscribe, ["call.concluded"]);
        assert!(m.audio_formats.is_empty() && m.live_settings && m.config.is_some() && m.system_config.is_some());
        assert_eq!(m.license, "GPL-3.0-or-later");
    }

    #[test]
    fn bad_settings_stop_it_and_good_ones_start_it() {
        let dir = testing::temp_dir("dvcf");
        let out = testing::run::<Dvcf>([HostMessage::Hello(testing::hello(&dir, json!({ "outputDir": "relative" })))]);
        assert_eq!(out.exit_code, trunk_recorder_plugin::EXIT_CONFIG);
        let out = testing::run::<Dvcf>([HostMessage::Hello(testing::hello(&dir, json!({})))]);
        assert!(out.ready());
    }
}
