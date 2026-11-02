//! Status report data model, reading, and formatting.

use serde::Serialize;

use super::bytes::format_bytes;
use super::profiler::{DetailedMemoryStatus, DetailedStorageStatus, FullSystemStatus, MachineInfo};
use super::telemetry::{self, Health, LiveStatus, PowerSource, Sampler};

#[derive(Debug, Serialize)]
pub struct StatusReport {
    pub schema_version: u32,
    pub collected_at: String,
    pub health: Option<Health>,
    pub machine: MachineInfo,
    pub memory: Option<DetailedMemoryStatus>,
    pub storage: Option<DetailedStorageStatus>,
    pub live: LiveStatus,
}

impl StatusReport {
    pub fn from_status(status: FullSystemStatus, collected_at: String) -> (Self, Vec<String>) {
        let memory_unreadable = status.memory.total_bytes == 0;
        let storage_unreadable = status.storage.total_bytes == 0;

        let mut notes = Vec::new();
        if memory_unreadable {
            notes.push("Note: memory could not be read.".to_string());
        }
        let health = if storage_unreadable {
            notes.push("Note: storage could not be read.".to_string());
            notes.push("Note: health could not be worked out without storage.".to_string());
            None
        } else {
            Some(telemetry::health(&status))
        };
        if status.live.cpu.usage_percent.is_none() {
            notes.push("Note: CPU load could not be read.".to_string());
        }

        let memory = if memory_unreadable {
            None
        } else {
            Some(status.memory)
        };
        let storage = if storage_unreadable {
            None
        } else {
            Some(status.storage)
        };

        let report = Self {
            schema_version: 1,
            collected_at,
            health,
            machine: status.machine,
            memory,
            storage,
            live: status.live,
        };

        (report, notes)
    }

    pub fn is_unusable(&self) -> bool {
        self.memory.is_none() && self.storage.is_none() && self.live.cpu.usage_percent.is_none()
    }
}

pub fn read() -> FullSystemStatus {
    let mut sampler = Sampler::new();
    let _ = sampler.sample();
    std::thread::sleep(telemetry::SAMPLE_INTERVAL);
    sampler.sample()
}

pub fn plain_text(report: &StatusReport) -> String {
    let mut out = String::new();

    let health_str = match &report.health {
        Some(h) => h.message.clone(),
        None => "unknown".to_string(),
    };
    out.push_str(&format!("{:<10}{}\n", "Health:", health_str));

    let cpu_str = match report.live.cpu.usage_percent {
        Some(usage) => format!("{usage:.1}%"),
        None => "unknown".to_string(),
    };
    out.push_str(&format!("{:<10}{}\n", "CPU:", cpu_str));

    let memory_str = match &report.memory {
        Some(mem) => {
            let base = format!(
                "{} of {} used",
                format_bytes(mem.used_bytes()),
                format_bytes(mem.total_bytes)
            );
            if let Some(pressure) = mem.pressure {
                format!("{base}, pressure {}", pressure.label())
            } else {
                base
            }
        }
        None => "unknown".to_string(),
    };
    out.push_str(&format!("{:<10}{}\n", "Memory:", memory_str));

    let disk_str = match &report.storage {
        Some(st) => format!(
            "{} of {} available",
            format_bytes(st.available_bytes),
            format_bytes(st.total_bytes)
        ),
        None => "unknown".to_string(),
    };
    out.push_str(&format!("{:<10}{}\n", "Disk:", disk_str));

    let battery_str = match &report.live.power {
        Some(p) => {
            let source_str = match p.source {
                PowerSource::Battery => "battery",
                PowerSource::Charging => "charging",
                PowerSource::PluggedIn => "plugged in",
            };
            format!("{}%, {source_str}", p.charge_percent)
        }
        None => "none".to_string(),
    };
    out.push_str(&format!("{:<10}{}\n", "Battery:", battery_str));

    if report.live.processes.is_empty() {
        out.push_str("Top processes: none\n");
    } else {
        out.push_str("Top processes:\n");
        for proc in report.live.processes.iter().take(3) {
            out.push_str(&format!(
                "  {}  {:.1}%  {}\n",
                proc.name,
                proc.cpu_percent,
                format_bytes(proc.memory_bytes)
            ));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::profiler::MemoryPressureLevel;
    use crate::core::telemetry::{CpuStatus, PowerStatus, ProcessStatus, Severity};

    fn readable() -> FullSystemStatus {
        FullSystemStatus {
            machine: MachineInfo {
                chip: Some("Apple M4 Pro".to_string()),
                cores: None,
                memory_bytes: Some(32_000_000_000),
                macos_version: "27.0.1".to_string(),
                model: None,
                model_name: None,
                uptime_seconds: 3600,
            },
            memory: DetailedMemoryStatus {
                total_bytes: 32_000_000_000,
                free_bytes: 13_800_000_000,
                app_bytes: 18_200_000_000,
                wired_bytes: 0,
                compressed_bytes: 0,
                cached_bytes: 0,
                swap_used_bytes: 0,
                pressure: None,
            },
            storage: DetailedStorageStatus {
                mount_point: "/".to_string(),
                total_bytes: 994_700_000_000,
                available_bytes: 118_600_000_000,
                free_bytes: 118_600_000_000,
                managed_bytes: 0,
            },
            live: LiveStatus {
                cpu: CpuStatus {
                    usage_percent: Some(12.4),
                    ..CpuStatus::default()
                },
                ..LiveStatus::default()
            },
        }
    }

    #[test]
    fn a_readable_status_has_no_nulls_and_no_notes() {
        let status = readable();
        let (report, notes) = StatusReport::from_status(status, "2026-11-01T13:40:00Z".to_string());
        assert!(notes.is_empty());
        assert_eq!(report.schema_version, 1);
        assert_eq!(report.collected_at, "2026-11-01T13:40:00Z");
        assert!(report.health.is_some());
        assert_eq!(report.health.as_ref().unwrap().severity, Severity::Good);
        assert!(report.memory.is_some());
        assert!(report.storage.is_some());
    }

    #[test]
    fn unreadable_memory_is_null() {
        let mut status = readable();
        status.memory.total_bytes = 0;
        let (report, notes) = StatusReport::from_status(status, "2026-11-01T13:40:00Z".to_string());
        let val = serde_json::to_value(&report).unwrap();
        assert!(val["memory"].is_null());
        assert_eq!(notes, ["Note: memory could not be read."]);
    }

    #[test]
    fn unreadable_storage_makes_health_null() {
        let mut status = readable();
        status.storage.total_bytes = 0;
        let (report, notes) = StatusReport::from_status(status, "2026-11-01T13:40:00Z".to_string());
        let val = serde_json::to_value(&report).unwrap();
        assert!(val["storage"].is_null());
        assert!(val["health"].is_null());
        assert_eq!(
            notes,
            [
                "Note: storage could not be read.",
                "Note: health could not be worked out without storage."
            ]
        );
    }

    #[test]
    fn missing_cpu_load_is_null() {
        let mut status = readable();
        status.live.cpu.usage_percent = None;
        let (report, notes) = StatusReport::from_status(status, "2026-11-01T13:40:00Z".to_string());
        let val = serde_json::to_value(&report).unwrap();
        assert!(val["live"]["cpu"]["usage_percent"].is_null());
        assert_eq!(notes, ["Note: CPU load could not be read."]);
    }

    #[test]
    fn no_battery_is_null_without_a_note() {
        let mut status = readable();
        status.live.power = None;
        let (report, notes) = StatusReport::from_status(status, "2026-11-01T13:40:00Z".to_string());
        let val = serde_json::to_value(&report).unwrap();
        assert!(val["live"]["power"].is_null());
        assert!(notes.is_empty());
    }

    #[test]
    fn a_report_is_unusable_only_when_memory_storage_and_cpu_are_all_missing() {
        let mut status = readable();
        status.memory.total_bytes = 0;
        status.storage.total_bytes = 0;
        status.live.cpu.usage_percent = None;
        let (report, _) = StatusReport::from_status(status, "2026-11-01T13:40:00Z".to_string());
        assert!(report.is_unusable());

        let mut status = readable();
        status.storage.total_bytes = 0;
        status.live.cpu.usage_percent = None;
        let (report, _) = StatusReport::from_status(status, "2026-11-01T13:40:00Z".to_string());
        assert!(!report.is_unusable());

        let mut status = readable();
        status.memory.total_bytes = 0;
        status.live.cpu.usage_percent = None;
        let (report, _) = StatusReport::from_status(status, "2026-11-01T13:40:00Z".to_string());
        assert!(!report.is_unusable());

        let mut status = readable();
        status.memory.total_bytes = 0;
        status.storage.total_bytes = 0;
        let (report, _) = StatusReport::from_status(status, "2026-11-01T13:40:00Z".to_string());
        assert!(!report.is_unusable());
    }

    #[test]
    fn the_json_has_the_documented_keys() {
        let status = readable();
        let (report, _) = StatusReport::from_status(status, "2026-11-01T13:40:00Z".to_string());
        let val = serde_json::to_value(&report).unwrap();
        let map = val.as_object().unwrap();
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "collected_at",
                "health",
                "live",
                "machine",
                "memory",
                "schema_version",
                "storage"
            ]
        );
    }

    #[test]
    fn plain_text_matches_the_format() {
        let mut status = readable();
        status.memory.pressure = Some(MemoryPressureLevel::Normal);
        status.live.power = Some(PowerStatus {
            charge_percent: 87,
            health_percent: None,
            cycles: None,
            source: PowerSource::Charging,
            adapter_watts: None,
            draw_watts: None,
            minutes_left: None,
            temperature_c: None,
        });
        status.live.processes = vec![
            ProcessStatus {
                pid: 1,
                name: "Safari".to_string(),
                cpu_percent: 12.4,
                memory_bytes: 1_200_000_000,
            },
            ProcessStatus {
                pid: 2,
                name: "node".to_string(),
                cpu_percent: 8.0,
                memory_bytes: 600_000_000,
            },
            ProcessStatus {
                pid: 3,
                name: "Finder".to_string(),
                cpu_percent: 1.1,
                memory_bytes: 120_000_000,
            },
        ];

        let (report, _) = StatusReport::from_status(status, "2026-11-01T13:40:00Z".to_string());
        let expected = format!(
            "Health:   All clear\nCPU:      12.4%\nMemory:   {} of {} used, pressure Normal\nDisk:     {} of {} available\nBattery:  87%, charging\nTop processes:\n  Safari  12.4%  {}\n  node  8.0%  {}\n  Finder  1.1%  {}\n",
            format_bytes(18_200_000_000),
            format_bytes(32_000_000_000),
            format_bytes(118_600_000_000),
            format_bytes(994_700_000_000),
            format_bytes(1_200_000_000),
            format_bytes(600_000_000),
            format_bytes(120_000_000),
        );
        assert_eq!(plain_text(&report), expected);

        // Also: no battery gives Battery:  none; no processes gives Top processes: none; unreadable memory gives Memory:   unknown.
        let mut status = readable();
        status.live.power = None;
        let (report_no_battery, _) =
            StatusReport::from_status(status, "2026-11-01T13:40:00Z".to_string());
        assert!(plain_text(&report_no_battery).contains("Battery:  none\n"));

        let mut status = readable();
        status.live.processes = vec![];
        let (report_no_procs, _) =
            StatusReport::from_status(status, "2026-11-01T13:40:00Z".to_string());
        assert!(plain_text(&report_no_procs).contains("Top processes: none\n"));

        let mut status = readable();
        status.memory.total_bytes = 0;
        let (report_unreadable_mem, _) =
            StatusReport::from_status(status, "2026-11-01T13:40:00Z".to_string());
        assert!(plain_text(&report_unreadable_mem).contains("Memory:   unknown\n"));
    }
}
