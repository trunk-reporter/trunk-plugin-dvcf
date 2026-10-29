//! The plugin's settings: the form is drawn from these structs' schema.

use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The plugin's settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    /// Output folder
    ///
    /// Where the .dvcf files go. Empty: beside each call in the capture folder. Otherwise this folder, with the calls' own system/date layout under it.
    pub output_dir: String,
    /// Keep for (days)
    ///
    /// Delete .dvcf files older than this. 0 keeps everything. Only .dvcf files are touched.
    pub retention_days: u32,
    /// Catch up
    ///
    /// Convert past calls that still have a frames file and no .dvcf: at start, then every 10 minutes.
    pub catch_up: bool,
    /// Catch up (days)
    ///
    /// How far back the catch-up looks. 0: as far as the call store goes.
    pub catch_up_days: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config { output_dir: String::new(), retention_days: 0, catch_up: true, catch_up_days: 30 }
    }
}

impl Config {
    /// Refuses settings the plugin cannot run with, by the form's own label.
    pub fn validate(&self) -> Result<(), String> {
        if !self.output_dir.is_empty() && !Path::new(&self.output_dir).is_absolute() {
            return Err("Output folder: must be an absolute path".into());
        }
        Ok(())
    }

    /// The output folder, or `None` when the .dvcf files go beside their calls.
    pub fn output_dir(&self) -> Option<PathBuf> {
        (!self.output_dir.is_empty()).then(|| PathBuf::from(&self.output_dir))
    }
}

/// Settings for each system.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct SystemConfig {
    /// Write this system
    ///
    /// Turn off to leave this system's calls out.
    pub enabled: bool,
}

impl Default for SystemConfig {
    fn default() -> Self {
        SystemConfig { enabled: true }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_specs() {
        let c = Config::default();
        assert_eq!((c.output_dir.as_str(), c.retention_days, c.catch_up, c.catch_up_days), ("", 0, true, 30));
        assert!(SystemConfig::default().enabled);
        assert_eq!(c.output_dir(), None);
    }

    #[test]
    fn a_relative_output_folder_is_refused_by_name() {
        let mut c = Config::default();
        assert!(c.validate().is_ok());
        c.output_dir = "dvcf".into();
        assert_eq!(c.validate().unwrap_err(), "Output folder: must be an absolute path");
        c.output_dir = if cfg!(windows) { r"D:\dvcf".into() } else { "/srv/dvcf".into() };
        assert!(c.validate().is_ok());
        assert!(c.output_dir().is_some());
    }

    #[test]
    fn the_schema_names_the_fields_in_camel_case() {
        let s = trunk_recorder_plugin::schema::schema_for::<Config>();
        let keys: Vec<&String> = s["properties"].as_object().unwrap().keys().collect();
        assert_eq!(keys, ["outputDir", "retentionDays", "catchUp", "catchUpDays"]);
        assert_eq!(s["properties"]["catchUpDays"]["default"], serde_json::json!(30));
    }
}
