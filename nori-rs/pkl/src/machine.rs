//! Portable machine metrics exposed to Pkl policy modules.
//!
//! Capture this value once when evaluating a policy. Serialize it with the
//! evaluation input and restore it during replay; [`MachineSnapshot::properties`]
//! only uses the captured values and never probes the machine again.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde::Serialize;
use sysinfo::System;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineSnapshot {
    /// Logical CPUs available to this process, when the host can report it.
    pub cpu_available_parallelism: Option<i64>,
    /// System load averages are unavailable on some platforms, including Windows.
    pub cpu_load_average: Option<LoadAverage>,
    /// Total installed/visible memory in bytes, when reported by the host.
    pub memory_total_bytes: Option<i64>,
    /// Memory currently available for reuse in bytes, when reported by the host.
    pub memory_available_bytes: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoadAverage {
    pub one_minute: f64,
    pub five_minutes: f64,
    pub fifteen_minutes: f64,
}

impl MachineSnapshot {
    /// Read portable machine metrics using Rust and sysinfo APIs, never shell commands.
    pub fn capture() -> Self {
        let mut system = System::new();
        system.refresh_memory();

        let total_memory = system.total_memory();
        let available_memory = system.available_memory();
        let load_average = load_average();

        Self {
            cpu_available_parallelism: std::thread::available_parallelism()
                .ok()
                .and_then(|count| i64::try_from(count.get()).ok()),
            cpu_load_average: load_average,
            memory_total_bytes: i64::try_from(total_memory).ok().filter(|value| *value > 0),
            memory_available_bytes: i64::try_from(available_memory).ok(),
        }
    }

    /// Flatten a captured snapshot into Pkl `prop:` values.
    ///
    /// Missing or unsupported readings are explicit strings (`unsupported`),
    /// rather than plausible zero values. Journal the serialized snapshot and
    /// derive properties from that snapshot during replay.
    pub fn properties(&self) -> BTreeMap<String, String> {
        let mut properties = BTreeMap::from([
            (
                "machine.cpu.available_parallelism".into(),
                display_metric(self.cpu_available_parallelism),
            ),
            (
                "machine.memory.total_bytes".into(),
                display_metric(self.memory_total_bytes),
            ),
            (
                "machine.memory.available_bytes".into(),
                display_metric(self.memory_available_bytes),
            ),
        ]);

        let load = self.cpu_load_average;
        properties.insert(
            "machine.cpu.load_average.one_minute".into(),
            load.map_or_else(
                || "unsupported".into(),
                |value| value.one_minute.to_string(),
            ),
        );
        properties.insert(
            "machine.cpu.load_average.five_minutes".into(),
            load.map_or_else(
                || "unsupported".into(),
                |value| value.five_minutes.to_string(),
            ),
        );
        properties.insert(
            "machine.cpu.load_average.fifteen_minutes".into(),
            load.map_or_else(
                || "unsupported".into(),
                |value| value.fifteen_minutes.to_string(),
            ),
        );
        properties
    }
}

#[cfg(windows)]
fn load_average() -> Option<LoadAverage> {
    None
}

#[cfg(not(windows))]
fn load_average() -> Option<LoadAverage> {
    let load = System::load_average();
    (load.one.is_finite() && load.five.is_finite() && load.fifteen.is_finite()).then_some(
        LoadAverage {
            one_minute: load.one,
            five_minutes: load.five,
            fifteen_minutes: load.fifteen,
        },
    )
}

fn display_metric<T: ToString>(value: Option<T>) -> String {
    value.map_or_else(|| "unsupported".into(), |value| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn missing_metrics_are_explicit_and_properties_are_stable() {
        let snapshot = MachineSnapshot {
            cpu_available_parallelism: None,
            cpu_load_average: None,
            memory_total_bytes: None,
            memory_available_bytes: None,
        };

        let serialized = serde_json::to_string(&snapshot).unwrap();
        let replayed: MachineSnapshot = serde_json::from_str(&serialized).unwrap();

        assert_eq!(replayed, snapshot);
        assert_eq!(
            snapshot
                .properties()
                .get("machine.cpu.available_parallelism"),
            Some(&"unsupported".to_string())
        );
        assert_eq!(
            snapshot
                .properties()
                .get("machine.cpu.load_average.one_minute"),
            Some(&"unsupported".to_string())
        );
        assert_eq!(
            snapshot.properties().get("machine.memory.total_bytes"),
            Some(&"unsupported".to_string())
        );
    }

    #[test]
    fn captured_metrics_round_trip_and_include_cpu_and_memory_properties() {
        let snapshot = MachineSnapshot::capture();
        let serialized = serde_json::to_string(&snapshot).unwrap();
        let replayed: MachineSnapshot = serde_json::from_str(&serialized).unwrap();

        assert_eq!(replayed.properties(), snapshot.properties());
        assert!(
            snapshot
                .properties()
                .contains_key("machine.cpu.available_parallelism")
        );
        assert!(
            snapshot
                .properties()
                .contains_key("machine.cpu.load_average.one_minute")
        );
        assert!(
            snapshot
                .properties()
                .contains_key("machine.memory.total_bytes")
        );
        assert!(
            snapshot
                .properties()
                .contains_key("machine.memory.available_bytes")
        );
    }
}
