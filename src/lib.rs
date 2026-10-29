//! dvcf: a Trunk Recorder Pro plugin that writes every digital call as a .dvcf file.

pub mod catchup;
pub mod config;
pub mod dvcf;
#[cfg(test)]
pub mod fixtures;
pub mod frames;
pub mod output;
pub mod plugin;
pub mod queue;
pub mod saved;
pub mod work;
