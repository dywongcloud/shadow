//! Control-plane configuration: the shape of a Hive and its scheduling knobs.

use hive_core::{HiveId, ResourceSpec};
use std::collections::BTreeMap;
use std::time::Duration;

/// One bare-metal-equivalent host. In real Hive a Box is a bare metal machine
/// divided into cells; here it is a capacity ledger the scheduler places against.
#[derive(Clone, Debug)]
pub struct BoxConfig {
    pub vcpus: u32,
    pub mem_mib: u32,
}

impl Default for BoxConfig {
    fn default() -> Self {
        BoxConfig {
            vcpus: 16,
            mem_mib: 32 * 1024,
        }
    }
}

/// Full configuration for a Hive (one regional cluster).
#[derive(Clone, Debug)]
pub struct HiveConfig {
    pub hive_id: HiveId,
    /// The boxes that make up this Hive.
    pub boxes: Vec<BoxConfig>,
    /// Desired number of pre-warmed idle cells per image (the warm pool).
    /// This is the mechanism that turns a ~90s cold provision into a ~5s start.
    pub warm_targets: BTreeMap<String, usize>,
    /// Warm target for images not listed in `warm_targets`.
    pub default_warm_target: usize,
    /// Resource spec used for pre-warmed cells.
    pub warm_spec: ResourceSpec,
    /// How long an idle warm cell may live before being reaped (cost control).
    pub warm_idle_ttl: Duration,
    /// Max builds executing concurrently across the whole Hive.
    pub max_concurrent_builds: usize,
    /// How often the autoscaler/reaper loop runs.
    pub autoscaler_interval: Duration,
}

impl Default for HiveConfig {
    fn default() -> Self {
        HiveConfig {
            hive_id: HiveId::from("hive-local"),
            boxes: vec![BoxConfig::default(), BoxConfig::default()],
            warm_targets: BTreeMap::new(),
            default_warm_target: 2,
            warm_spec: ResourceSpec::default(),
            warm_idle_ttl: Duration::from_secs(120),
            max_concurrent_builds: 8,
            autoscaler_interval: Duration::from_millis(500),
        }
    }
}

impl HiveConfig {
    pub fn warm_target_for(&self, image: &str) -> usize {
        self.warm_targets
            .get(image)
            .copied()
            .unwrap_or(self.default_warm_target)
    }

    /// All images the warm pool should actively maintain.
    pub fn warm_images(&self) -> Vec<String> {
        self.warm_targets.keys().cloned().collect()
    }
}
