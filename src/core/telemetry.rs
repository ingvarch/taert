//! Live machine telemetry for the System status screen, sampled on a background thread.

use std::net::IpAddr;
use std::path::Path;
use std::process::Command;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sysinfo::{Components, Disks, Networks, System};

use super::iokit::{self, Properties, Value};
use super::profiler::{FullSystemStatus, MemoryPressureLevel, SystemProfiler};

pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

// Storage, battery and temperatures change slowly and cost more to read.
const SLOW_SAMPLE_EVERY: u32 = 5;
const TOP_PROCESSES: usize = 60;
// Skips mounted installers and other small disk images.
const MIN_VOLUME_BYTES: u64 = 1_000_000_000;
const UNKNOWN_MINUTES: i64 = 65535;
const MAX_SENSOR_CELSIUS: f32 = 130.0;
const DISK_CRITICAL_FREE: f64 = 0.05;
const DISK_WARNING_FREE: f64 = 0.10;
const BATTERY_WARNING_HEALTH: u8 = 80;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CpuStatus {
    pub usage_percent: Option<f32>,
    pub core_percents: Vec<f32>,
    pub efficiency_cores: usize,
    pub load_average: Option<[f64; 3]>,
    pub temperature_c: Option<f32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GpuStatus {
    pub usage_percent: Option<f32>,
    pub core_count: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VolumeStatus {
    pub name: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DiskIo {
    pub read_per_sec: f64,
    pub write_per_sec: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterfaceAddress {
    pub interface: String,
    pub address: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NetworkStatus {
    pub down_per_sec: Option<f64>,
    pub up_per_sec: Option<f64>,
    pub local: Option<InterfaceAddress>,
    pub tunnel: Option<InterfaceAddress>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PowerSource {
    Battery,
    Charging,
    PluggedIn,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PowerStatus {
    pub charge_percent: u8,
    pub health_percent: Option<u8>,
    pub cycles: Option<i64>,
    pub source: PowerSource,
    pub adapter_watts: Option<i64>,
    pub draw_watts: Option<f64>,
    pub minutes_left: Option<i64>,
    pub temperature_c: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessStatus {
    pub pid: u32,
    pub name: String,
    pub cpu_percent: f32,
    pub memory_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LiveStatus {
    pub cpu: CpuStatus,
    pub gpu: Option<GpuStatus>,
    pub boot_volume: Option<String>,
    pub volumes: Vec<VolumeStatus>,
    pub disk_io: Option<DiskIo>,
    pub network: NetworkStatus,
    pub power: Option<PowerStatus>,
    pub processes: Vec<ProcessStatus>,
    pub memory_processes: Vec<ProcessStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Good,
    Warning,
    Critical,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Health {
    pub severity: Severity,
    pub message: String,
}

pub fn disk_severity(free_fraction: f64) -> Severity {
    if free_fraction < DISK_CRITICAL_FREE {
        Severity::Critical
    } else if free_fraction < DISK_WARNING_FREE {
        Severity::Warning
    } else {
        Severity::Good
    }
}

/// The most serious problem on this Mac, or that there is none.
pub fn health(status: &FullSystemStatus) -> Health {
    let problem = |severity, message: &str| Health {
        severity,
        message: message.to_string(),
    };
    let disk = disk_severity(status.storage.free_fraction());
    let battery_health = status.live.power.as_ref().and_then(|p| p.health_percent);

    match (status.memory.pressure, disk) {
        (Some(MemoryPressureLevel::Critical), _) => {
            return problem(Severity::Critical, "Memory pressure critical");
        }
        (_, Severity::Critical) => return problem(Severity::Critical, "Disk almost full"),
        (Some(MemoryPressureLevel::Warning), _) => {
            return problem(Severity::Warning, "Memory pressure high");
        }
        (_, Severity::Warning) => return problem(Severity::Warning, "Disk space low"),
        _ => {}
    }

    match battery_health {
        Some(percent) if percent < BATTERY_WARNING_HEALTH => Health {
            severity: Severity::Warning,
            message: format!("Battery health {percent}%"),
        },
        _ => problem(Severity::Good, "All clear"),
    }
}

pub fn product_name(properties: &Properties) -> Option<String> {
    let bytes = properties.get("product-name")?.as_bytes()?;
    let name = String::from_utf8_lossy(bytes);
    Some(name.trim_end_matches('\0').to_string())
}

pub fn parse_processes(output: &str) -> Vec<ProcessStatus> {
    output
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            let mut words = trimmed.split_whitespace();
            let t1 = words.next()?;
            let t2 = words.next()?;
            let t3 = words.next()?;
            let t4 = words.next();

            if t4.is_some()
                && let Ok(pid) = t1.parse::<u32>()
                && let Ok(cpu_percent) = t2.parse::<f32>()
                && let Ok(rss_kb) = t3.parse::<u64>()
            {
                let offset = (t3.as_ptr() as usize - trimmed.as_ptr() as usize) + t3.len();
                let name = trimmed[offset..].trim_start().to_string();
                return Some(ProcessStatus {
                    pid,
                    name,
                    cpu_percent,
                    memory_bytes: rss_kb.saturating_mul(1024),
                });
            }

            // Fallback for 3-column input without pid: cpu, rss_kb, name
            if let Ok(cpu_percent) = t1.parse::<f32>()
                && let Ok(rss_kb) = t2.parse::<u64>()
            {
                let offset = (t2.as_ptr() as usize - trimmed.as_ptr() as usize) + t2.len();
                let name = trimmed[offset..].trim_start().to_string();
                return Some(ProcessStatus {
                    pid: 0,
                    name,
                    cpu_percent,
                    memory_bytes: rss_kb.saturating_mul(1024),
                });
            }

            None
        })
        .collect()
}

/// The name of the startup volume and the other volumes worth showing.
pub fn mounted_volumes() -> (Option<String>, Vec<VolumeStatus>) {
    let disks = Disks::new_with_refreshed_list();
    let boot_volume = disks
        .list()
        .iter()
        .find(|disk| disk.mount_point() == Path::new("/"))
        .map(|disk| disk.name().to_string_lossy().into_owned());
    let volumes = disks
        .list()
        .iter()
        .filter(|disk| {
            is_external_volume(&disk.mount_point().to_string_lossy(), disk.total_space())
        })
        .map(|disk| VolumeStatus {
            name: disk.name().to_string_lossy().into_owned(),
            total_bytes: disk.total_space(),
            available_bytes: disk.available_space(),
        })
        .collect();

    (boot_volume, volumes)
}

// `output` comes sorted by CPU.
fn rank_processes(output: &str, limit: usize) -> (Vec<ProcessStatus>, Vec<ProcessStatus>) {
    let by_cpu = parse_processes(output);

    let mut by_memory = by_cpu.clone();
    by_memory.sort_by_key(|process| std::cmp::Reverse(process.memory_bytes));
    by_memory.truncate(limit);

    (by_cpu.into_iter().take(limit).collect(), by_memory)
}

// ps runs setuid root on macOS, so it sees the processes of every user.
fn top_processes() -> (Vec<ProcessStatus>, Vec<ProcessStatus>) {
    Command::new("/bin/ps")
        .args(["-Aceo", "pid=,pcpu=,rss=,comm=", "-r"])
        .output()
        .map(|output| rank_processes(&String::from_utf8_lossy(&output.stdout), TOP_PROCESSES))
        .unwrap_or_default()
}

fn per_second(previous: u64, current: u64, elapsed: Duration) -> f64 {
    let seconds = elapsed.as_secs_f64();
    if seconds > 0.0 {
        current.saturating_sub(previous) as f64 / seconds
    } else {
        0.0
    }
}

fn power_from(properties: &Properties) -> Option<PowerStatus> {
    let number = |key: &str| properties.get(key).and_then(Value::as_i64);
    let flag = |key: &str| properties.get(key).and_then(Value::as_bool);
    let nested = |map: &str, key: &str| {
        let map = properties.get(map).and_then(Value::as_map)?;
        map.get(key).and_then(Value::as_i64)
    };

    let charge = number("CurrentCapacity")?;
    // Apple Silicon reports a percentage here, Intel milliamp-hours.
    let charge_percent =
        (charge * 100 / number("MaxCapacity").filter(|&max| max > 0)?).clamp(0, 100);

    let design = number("DesignCapacity").or_else(|| nested("BatteryData", "DesignCapacity"));
    let full =
        nested("BatteryData", "NominalChargeCapacity").or_else(|| number("AppleRawMaxCapacity"));
    let health_percent = match (full, design) {
        (Some(full), Some(design)) if design > 0 => Some((full * 100 / design).min(100) as u8),
        _ => None,
    };

    let source = match (flag("ExternalConnected"), flag("IsCharging")) {
        (Some(true), Some(true)) => PowerSource::Charging,
        (Some(true), _) => PowerSource::PluggedIn,
        _ => PowerSource::Battery,
    };
    let minutes = match source {
        PowerSource::Battery => number("AvgTimeToEmpty"),
        PowerSource::Charging => number("AvgTimeToFull"),
        PowerSource::PluggedIn => None,
    };
    let draw_watts = match (source, number("Voltage"), number("InstantAmperage")) {
        (PowerSource::Battery, Some(millivolts), Some(milliamps)) => {
            Some((millivolts * milliamps.abs()) as f64 / 1_000_000.0)
        }
        _ => None,
    };

    Some(PowerStatus {
        charge_percent: charge_percent as u8,
        health_percent,
        cycles: number("CycleCount"),
        source,
        adapter_watts: nested("AdapterDetails", "Watts"),
        draw_watts,
        minutes_left: minutes.filter(|&m| m != UNKNOWN_MINUTES),
        temperature_c: None,
    })
}

fn gpu_from(properties: &Properties) -> Option<GpuStatus> {
    let statistics = properties.get("PerformanceStatistics")?.as_map()?;
    Some(GpuStatus {
        usage_percent: statistics
            .get("Device Utilization %")
            .and_then(Value::as_i64)
            .map(|percent| percent as f32),
        core_count: properties.get("gpu-core-count").and_then(Value::as_i64),
    })
}

fn disk_totals(drivers: &[Properties]) -> (u64, u64) {
    let counter = |statistics: &Properties, key: &str| {
        let bytes = statistics.get(key).and_then(Value::as_i64).unwrap_or(0);
        bytes.max(0) as u64
    };

    drivers
        .iter()
        .filter_map(|driver| driver.get("Statistics")?.as_map())
        .fold((0, 0), |(read, write), statistics| {
            (
                read + counter(statistics, "Bytes (Read)"),
                write + counter(statistics, "Bytes (Write)"),
            )
        })
}

fn is_physical_interface(name: &str) -> bool {
    name.starts_with("en") || name.starts_with("pdp_ip")
}

fn is_tunnel_interface(name: &str) -> bool {
    ["utun", "ipsec", "ppp"].iter().any(|p| name.starts_with(p))
}

fn first_ipv4(
    interfaces: &[(String, Vec<IpAddr>)],
    matches: fn(&str) -> bool,
) -> Option<InterfaceAddress> {
    interfaces
        .iter()
        .filter(|(name, _)| matches(name))
        .find_map(|(name, addresses)| {
            let address = addresses.iter().find(|address| address.is_ipv4())?;
            Some(InterfaceAddress {
                interface: name.clone(),
                address: address.to_string(),
            })
        })
}

fn is_valid_reading(celsius: f32) -> bool {
    (0.0..MAX_SENSOR_CELSIUS).contains(&celsius)
}

fn cpu_temperature(readings: &[(String, f32)]) -> Option<f32> {
    let dies: Vec<f32> = readings
        .iter()
        .filter(|(label, celsius)| {
            (label.contains("tdie") || label.starts_with("CPU")) && is_valid_reading(*celsius)
        })
        .map(|(_, celsius)| *celsius)
        .collect();

    (!dies.is_empty()).then(|| dies.iter().sum::<f32>() / dies.len() as f32)
}

fn battery_temperature(readings: &[(String, f32)]) -> Option<f32> {
    readings
        .iter()
        .find(|(label, celsius)| label.contains("battery") && is_valid_reading(*celsius))
        .map(|(_, celsius)| *celsius)
}

fn is_external_volume(mount_point: &str, total_bytes: u64) -> bool {
    mount_point.starts_with("/Volumes/") && total_bytes >= MIN_VOLUME_BYTES
}

/// Keeps what a reading needs from the last one: rates and CPU load are differences.
pub struct Sampler {
    system: System,
    networks: Networks,
    components: Components,
    disk_totals: Option<(u64, u64)>,
    sampled_at: Option<Instant>,
    samples: u32,
    status: FullSystemStatus,
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

impl Sampler {
    pub fn new() -> Self {
        Self {
            system: System::new(),
            networks: Networks::new_with_refreshed_list(),
            components: Components::new_with_refreshed_list(),
            disk_totals: None,
            sampled_at: None,
            samples: 0,
            status: SystemProfiler::current_full_status(),
        }
    }

    pub fn sample(&mut self) -> FullSystemStatus {
        let elapsed = self.sampled_at.map(|at| at.elapsed());
        self.sampled_at = Some(Instant::now());

        if self.samples.is_multiple_of(SLOW_SAMPLE_EVERY) {
            self.sample_slow();
        }
        self.samples = self.samples.wrapping_add(1);

        self.sample_memory();
        self.sample_cpu(elapsed.is_some());
        self.sample_disk_io(elapsed);
        self.sample_network(elapsed);
        self.status.live.gpu = iokit::services("IOAccelerator").iter().find_map(gpu_from);
        (
            self.status.live.processes,
            self.status.live.memory_processes,
        ) = top_processes();

        self.status.clone()
    }

    fn sample_slow(&mut self) {
        self.status.storage = SystemProfiler::detailed_storage(Path::new("/"));
        self.status.machine.uptime_seconds = SystemProfiler::uptime_seconds();

        let live = &mut self.status.live;
        (live.boot_volume, live.volumes) = mounted_volumes();

        self.components.refresh(false);
        let readings: Vec<(String, f32)> = self
            .components
            .list()
            .iter()
            .filter_map(|c| Some((c.label().to_string(), c.temperature()?)))
            .collect();
        live.cpu.temperature_c = cpu_temperature(&readings);
        live.power = iokit::services("AppleSmartBattery")
            .iter()
            .find_map(power_from)
            .map(|power| PowerStatus {
                temperature_c: battery_temperature(&readings),
                ..power
            });
    }

    fn sample_memory(&mut self) {
        self.system.refresh_memory();
        self.status.memory = SystemProfiler::detailed_memory();
        self.status.memory.swap_used_bytes = self.system.used_swap();
    }

    fn sample_cpu(&mut self, has_previous: bool) {
        self.system.refresh_cpu_usage();
        let cpu = &mut self.status.live.cpu;

        cpu.usage_percent = has_previous.then(|| self.system.global_cpu_usage());
        cpu.core_percents = if has_previous {
            self.system.cpus().iter().map(|c| c.cpu_usage()).collect()
        } else {
            Vec::new()
        };
        cpu.efficiency_cores =
            SystemProfiler::read_sysctl_u64("hw.perflevel1.physicalcpu").unwrap_or(0) as usize;

        let load = System::load_average();
        cpu.load_average = Some([load.one, load.five, load.fifteen]);
    }

    fn sample_disk_io(&mut self, elapsed: Option<Duration>) {
        let totals = disk_totals(&iokit::services("IOBlockStorageDriver"));
        self.status.live.disk_io = match (self.disk_totals, elapsed) {
            (Some((read, write)), Some(elapsed)) => Some(DiskIo {
                read_per_sec: per_second(read, totals.0, elapsed),
                write_per_sec: per_second(write, totals.1, elapsed),
            }),
            _ => None,
        };
        self.disk_totals = Some(totals);
    }

    fn sample_network(&mut self, elapsed: Option<Duration>) {
        self.networks.refresh(true);

        let mut interfaces: Vec<(String, Vec<IpAddr>)> = self
            .networks
            .iter()
            .map(|(name, data)| {
                let addresses = data.ip_networks().iter().map(|ip| ip.addr).collect();
                (name.clone(), addresses)
            })
            .collect();
        interfaces.sort();

        let (down, up) = self
            .networks
            .iter()
            .filter(|(name, _)| is_physical_interface(name))
            .fold((0, 0), |(down, up), (_, data)| {
                (down + data.received(), up + data.transmitted())
            });

        let network = &mut self.status.live.network;
        network.down_per_sec = elapsed.map(|elapsed| per_second(0, down, elapsed));
        network.up_per_sec = elapsed.map(|elapsed| per_second(0, up, elapsed));
        network.local = first_ipv4(&interfaces, is_physical_interface);
        network.tunnel = first_ipv4(&interfaces, is_tunnel_interface);
    }
}

/// Sends a fresh status every `SAMPLE_INTERVAL` until the receiver goes away.
pub fn spawn_sampler() -> Receiver<FullSystemStatus> {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let mut sampler = Sampler::new();
        while tx.send(sampler.sample()).is_ok() {
            thread::sleep(SAMPLE_INTERVAL);
        }
    });

    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::profiler::{DetailedMemoryStatus, DetailedStorageStatus, MachineInfo};

    fn number(value: i64) -> Value {
        Value::Number(value)
    }

    fn map(entries: &[(&str, Value)]) -> Properties {
        entries
            .iter()
            .map(|(key, value)| (key.to_string(), value.clone()))
            .collect()
    }

    fn battery(entries: &[(&str, Value)]) -> Properties {
        let mut properties = map(&[
            ("CurrentCapacity", number(80)),
            ("MaxCapacity", number(100)),
            ("ExternalConnected", Value::Bool(false)),
            ("IsCharging", Value::Bool(false)),
            ("CycleCount", number(124)),
            ("Voltage", number(12_000)),
            ("InstantAmperage", number(-500)),
            ("AvgTimeToEmpty", number(685)),
            ("AvgTimeToFull", number(UNKNOWN_MINUTES)),
            (
                "BatteryData",
                Value::Map(map(&[
                    ("NominalChargeCapacity", number(7_500)),
                    ("DesignCapacity", number(10_000)),
                ])),
            ),
            ("AdapterDetails", Value::Map(map(&[("Watts", number(96))]))),
        ]);
        properties.extend(map(entries));
        properties
    }

    fn system_status(
        available_fraction: f64,
        pressure: Option<MemoryPressureLevel>,
        battery_health: Option<u8>,
    ) -> FullSystemStatus {
        let total = 1_000_000_000_000_u64;
        FullSystemStatus {
            machine: MachineInfo {
                chip: None,
                cores: None,
                memory_bytes: None,
                macos_version: String::new(),
                model: None,
                model_name: None,
                uptime_seconds: 0,
            },
            memory: DetailedMemoryStatus {
                total_bytes: 100,
                free_bytes: 50,
                app_bytes: 50,
                wired_bytes: 0,
                compressed_bytes: 0,
                cached_bytes: 0,
                swap_used_bytes: 0,
                pressure,
            },
            storage: DetailedStorageStatus {
                mount_point: "/".to_string(),
                total_bytes: total,
                available_bytes: (total as f64 * available_fraction) as u64,
                free_bytes: (total as f64 * available_fraction) as u64,
                managed_bytes: 0,
            },
            live: LiveStatus {
                power: battery_health.map(|health_percent| PowerStatus {
                    charge_percent: 80,
                    health_percent: Some(health_percent),
                    cycles: None,
                    source: PowerSource::PluggedIn,
                    adapter_watts: None,
                    draw_watts: None,
                    minutes_left: None,
                    temperature_c: None,
                }),
                ..LiveStatus::default()
            },
        }
    }

    #[test]
    fn a_healthy_mac_is_all_clear() {
        let health = health(&system_status(
            0.5,
            Some(MemoryPressureLevel::Normal),
            Some(100),
        ));

        assert_eq!(health.severity, Severity::Good);
        assert_eq!(health.message, "All clear");
    }

    #[test]
    fn the_worst_problem_wins() {
        let status = system_status(0.08, Some(MemoryPressureLevel::Critical), Some(70));
        let health = health(&status);

        assert_eq!(health.severity, Severity::Critical);
        assert_eq!(health.message, "Memory pressure critical");
    }

    #[test]
    fn a_nearly_full_disk_is_reported() {
        let low = health(&system_status(0.08, None, None));
        assert_eq!(low.severity, Severity::Warning);
        assert_eq!(low.message, "Disk space low");

        let full = health(&system_status(0.03, None, None));
        assert_eq!(full.severity, Severity::Critical);
        assert_eq!(full.message, "Disk almost full");
    }

    #[test]
    fn a_worn_battery_is_reported() {
        let health = health(&system_status(0.5, None, Some(72)));

        assert_eq!(health.severity, Severity::Warning);
        assert_eq!(health.message, "Battery health 72%");
    }

    #[test]
    fn parses_processes_with_pids() {
        let output = "  417  45.2 350800 WindowServer\n 12096   4.0 1070512 claude helper\n";

        let processes = parse_processes(output);

        assert_eq!(processes.len(), 2);
        assert_eq!(processes[0].pid, 417);
        assert_eq!(processes[0].name, "WindowServer");
        assert_eq!(processes[0].cpu_percent, 45.2);
        assert_eq!(processes[0].memory_bytes, 350_800 * 1024);
        assert_eq!(processes[1].pid, 12096);
        assert_eq!(processes[1].name, "claude helper");
    }

    #[test]
    fn parses_processes_with_spaces_in_their_names() {
        let output = "215.5 969776 node\n 53.5 162400 WindowServer\n 1.0 2048 Google Chrome He\n";

        let processes = parse_processes(output);

        assert_eq!(processes.len(), 3);
        assert_eq!(processes[0].name, "node");
        assert_eq!(processes[0].cpu_percent, 215.5);
        assert_eq!(processes[0].memory_bytes, 969_776 * 1024);
        assert_eq!(processes[2].name, "Google Chrome He");
    }

    #[test]
    fn keeps_only_the_top_processes_and_skips_broken_lines() {
        let output = "garbage\n9.0 100 first\n8.0 100 second\n7.0 100 third\n";

        let (by_cpu, _) = rank_processes(output, 2);

        let names: Vec<_> = by_cpu.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["first", "second"]);
    }

    #[test]
    fn ranks_processes_by_memory_from_the_whole_list() {
        let output = "9.0 100 busy\n8.0 200 middle\n0.0 900 idle\n";

        let (_, by_memory) = rank_processes(output, 2);

        let names: Vec<_> = by_memory.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["idle", "middle"]);
    }

    #[test]
    fn rates_are_bytes_per_second_and_survive_a_counter_reset() {
        assert_eq!(per_second(1_000, 3_000, Duration::from_secs(2)), 1_000.0);
        assert_eq!(per_second(5_000, 100, Duration::from_secs(1)), 0.0);
        assert_eq!(per_second(0, 100, Duration::ZERO), 0.0);
    }

    #[test]
    fn reads_a_battery_on_its_own_power() {
        let power = power_from(&battery(&[])).unwrap();

        assert_eq!(power.charge_percent, 80);
        assert_eq!(power.health_percent, Some(75));
        assert_eq!(power.cycles, Some(124));
        assert_eq!(power.source, PowerSource::Battery);
        assert_eq!(power.adapter_watts, Some(96));
        assert_eq!(power.draw_watts, Some(6.0));
        assert_eq!(power.minutes_left, Some(685));
    }

    #[test]
    fn health_never_passes_one_hundred_percent() {
        let properties = battery(&[(
            "BatteryData",
            Value::Map(map(&[
                ("NominalChargeCapacity", number(8_669)),
                ("DesignCapacity", number(8_579)),
            ])),
        )]);

        assert_eq!(power_from(&properties).unwrap().health_percent, Some(100));
    }

    #[test]
    fn intel_batteries_report_capacity_in_milliamp_hours() {
        let properties = battery(&[
            ("CurrentCapacity", number(3_000)),
            ("MaxCapacity", number(6_000)),
        ]);

        assert_eq!(power_from(&properties).unwrap().charge_percent, 50);
    }

    #[test]
    fn plugged_in_batteries_have_no_draw_and_no_time_left() {
        let properties = battery(&[("ExternalConnected", Value::Bool(true))]);
        let power = power_from(&properties).unwrap();

        assert_eq!(power.source, PowerSource::PluggedIn);
        assert_eq!(power.draw_watts, None);
        assert_eq!(power.minutes_left, None);
    }

    #[test]
    fn charging_reports_the_time_to_full_unless_it_is_unknown() {
        let unknown = battery(&[
            ("ExternalConnected", Value::Bool(true)),
            ("IsCharging", Value::Bool(true)),
        ]);
        let power = power_from(&unknown).unwrap();
        assert_eq!(power.source, PowerSource::Charging);
        assert_eq!(power.minutes_left, None);

        let known = battery(&[
            ("ExternalConnected", Value::Bool(true)),
            ("IsCharging", Value::Bool(true)),
            ("AvgTimeToFull", number(42)),
        ]);
        assert_eq!(power_from(&known).unwrap().minutes_left, Some(42));
    }

    #[test]
    fn a_mac_without_a_battery_has_no_power_status() {
        assert_eq!(power_from(&Properties::new()), None);
    }

    #[test]
    fn reads_gpu_load_and_core_count() {
        let properties = map(&[
            ("gpu-core-count", number(20)),
            (
                "PerformanceStatistics",
                Value::Map(map(&[("Device Utilization %", number(44))])),
            ),
        ]);

        let gpu = gpu_from(&properties).unwrap();

        assert_eq!(gpu.usage_percent, Some(44.0));
        assert_eq!(gpu.core_count, Some(20));
    }

    #[test]
    fn a_gpu_without_statistics_is_ignored() {
        assert_eq!(gpu_from(&map(&[("model", number(1))])), None);
    }

    #[test]
    fn disk_totals_add_up_every_drive() {
        let drive = |read, write| {
            map(&[(
                "Statistics",
                Value::Map(map(&[
                    ("Bytes (Read)", number(read)),
                    ("Bytes (Write)", number(write)),
                ])),
            )])
        };

        assert_eq!(
            disk_totals(&[drive(10, 20), drive(1, 2), Properties::new()]),
            (11, 22)
        );
    }

    #[test]
    fn counts_only_real_network_interfaces() {
        for name in ["en0", "en5", "pdp_ip0"] {
            assert!(is_physical_interface(name), "{name}");
        }
        for name in ["lo0", "utun8", "awdl0", "llw0", "bridge0", "anpi0"] {
            assert!(!is_physical_interface(name), "{name}");
        }
    }

    #[test]
    fn recognizes_tunnels() {
        for name in ["utun8", "ipsec0", "ppp0"] {
            assert!(is_tunnel_interface(name), "{name}");
        }
        assert!(!is_tunnel_interface("en0"));
    }

    #[test]
    fn picks_the_first_matching_ipv4_address() {
        let ipv6: IpAddr = "fe80::1".parse().unwrap();
        let tunnel: IpAddr = "100.117.11.103".parse().unwrap();
        let local: IpAddr = "192.168.86.148".parse().unwrap();
        let interfaces = vec![
            ("en0".to_string(), vec![ipv6, local]),
            ("utun4".to_string(), vec![ipv6]),
            ("utun8".to_string(), vec![tunnel]),
        ];

        let found = first_ipv4(&interfaces, is_tunnel_interface).unwrap();
        assert_eq!(found.interface, "utun8");
        assert_eq!(found.address, "100.117.11.103");

        let found = first_ipv4(&interfaces, is_physical_interface).unwrap();
        assert_eq!(found.address, "192.168.86.148");
    }

    #[test]
    fn cpu_temperature_averages_the_die_sensors_and_drops_broken_ones() {
        let readings = vec![
            ("PMU tdie1".to_string(), 80.0),
            ("PMU tdie2".to_string(), 90.0),
            ("PMU tdev3".to_string(), -9201.1),
            ("PMU tdie3".to_string(), -9201.1),
            ("gas gauge battery".to_string(), 31.0),
        ];

        assert_eq!(cpu_temperature(&readings), Some(85.0));
        assert_eq!(battery_temperature(&readings), Some(31.0));
    }

    #[test]
    fn no_sensors_means_no_temperature() {
        assert_eq!(cpu_temperature(&[]), None);
        assert_eq!(battery_temperature(&[]), None);
    }

    #[test]
    fn lists_real_external_volumes_only() {
        assert!(is_external_volume("/Volumes/T7", 2_000_000_000_000));
        assert!(!is_external_volume("/Volumes/Installer", 40_000_000));
        assert!(!is_external_volume("/", 494_000_000_000));
        assert!(!is_external_volume("/System/Volumes/Data", 494_000_000_000));
    }

    #[test]
    fn reads_the_marketing_name_without_its_terminator() {
        let properties = map(&[(
            "product-name",
            Value::Data(b"MacBook Pro (16-inch, Nov 2024)\0".to_vec()),
        )]);

        assert_eq!(
            product_name(&properties).as_deref(),
            Some("MacBook Pro (16-inch, Nov 2024)")
        );
        assert_eq!(product_name(&Properties::new()), None);
    }

    #[test]
    fn a_second_sample_measures_cpu_and_rates() {
        let mut sampler = Sampler::new();

        let first = sampler.sample();
        assert_eq!(first.live.cpu.usage_percent, None);
        assert_eq!(first.live.network.down_per_sec, None);

        thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        let second = sampler.sample();
        assert!(second.live.cpu.usage_percent.is_some());
        assert!(!second.live.cpu.core_percents.is_empty());
        assert!(second.live.network.down_per_sec.is_some());
        assert!(second.memory.total_bytes > 0);
        assert!(second.live.cpu.load_average.is_some());
    }

    #[test]
    fn spawn_sampler_delivers_a_status() {
        let receiver = spawn_sampler();

        assert!(receiver.recv_timeout(Duration::from_secs(10)).is_ok());
    }
}
