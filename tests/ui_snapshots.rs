use std::path::PathBuf;

use insta::assert_snapshot;
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use taert::core::profiler::{
    DetailedMemoryStatus, DetailedStorageStatus, FullSystemStatus, MachineInfo, MemoryPressureLevel,
};
use taert::core::scanner::{CategoryScan, ScanItem, ToolGroup};
use taert::core::stores::{Config, IgnoreStore};
use taert::core::telemetry::{
    CpuStatus, DiskIo, GpuStatus, InterfaceAddress, LiveStatus, NetworkStatus, PowerSource,
    PowerStatus, ProcessStatus, VolumeStatus,
};
use taert::ui::analyze::{AnalyzeState, render_analyze};
use taert::ui::env::UiEnv;
use taert::ui::path_list_editor::{PathListEditorState, PathListKind, render_path_list_editor};
use taert::ui::preferences::{PreferencesState, render_preferences};
use taert::ui::progress::{ProgressState, render_progress};
use taert::ui::review::{ReviewState, render_review};
use taert::ui::select::{SelectState, render_select};
use taert::ui::system_status::{SystemStatusState, render_system_status};
use taert::ui::theme::Theme;
use taert::ui::workspace::{StartConfirmKind, WorkspaceState, render_workspace};

// Theme::new reads the terminal's env, and a snapshot must not depend on it.
fn snapshot_theme() -> Theme {
    Theme::with_env(UiEnv::default())
}

fn render_to_string(w: u16, h: u16, draw: impl FnOnce(&mut Frame)) -> String {
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(draw).unwrap();
    let buffer = terminal.backend().buffer();
    let mut lines = Vec::new();
    for y in 0..buffer.area.height {
        let mut line = String::new();
        for x in 0..buffer.area.width {
            line.push_str(buffer[(x, y)].symbol());
        }
        lines.push(line.trim_end().to_string());
    }
    lines.join("\n")
}

fn sample_scan() -> CategoryScan {
    CategoryScan {
        id: "test".to_string(),
        title: "Test Scan".to_string(),
        groups: vec![ToolGroup {
            tool: "Cargo".to_string(),
            items: vec![
                ScanItem::new(
                    PathBuf::from("/Users/test/.cargo/registry/cache"),
                    15_000_000,
                    true,
                    false,
                ),
                ScanItem::new(
                    PathBuf::from("/Users/test/.cargo/git/db"),
                    25_000_000,
                    false,
                    false,
                ),
            ],
        }],
    }
}

fn mock_system_status() -> FullSystemStatus {
    FullSystemStatus {
        machine: MachineInfo {
            chip: Some("Apple M4 Pro".to_string()),
            cores: Some("14 cores (10 performance, 4 efficiency)".to_string()),
            memory_bytes: Some(48 * 1024 * 1024 * 1024),
            macos_version: "27.0.1 (26A434)".to_string(),
            model: Some("Mac16,7".to_string()),
            model_name: Some("MacBook Pro (16-inch, Nov 2024)".to_string()),
            uptime_seconds: 879600,
        },
        memory: DetailedMemoryStatus {
            total_bytes: 48 * 1024 * 1024 * 1024,
            free_bytes: 11 * 1024 * 1024 * 1024,
            app_bytes: 19_500_000_000,
            wired_bytes: 5_700_000_000,
            compressed_bytes: 11_200_000_000,
            cached_bytes: 4_200_000_000,
            swap_used_bytes: 0,
            pressure: Some(MemoryPressureLevel::Normal),
        },
        storage: DetailedStorageStatus {
            mount_point: "/".to_string(),
            total_bytes: 494_400_000_000,
            available_bytes: 118_600_000_000,
            free_bytes: 118_600_000_000,
            managed_bytes: 5_700_000_000,
        },
        live: LiveStatus {
            cpu: CpuStatus {
                usage_percent: Some(48.8),
                core_percents: vec![
                    12.0, 18.0, 9.0, 25.0, 91.0, 76.0, 64.0, 58.0, 44.0, 37.0, 22.0, 15.0, 8.0, 3.0,
                ],
                efficiency_cores: 4,
                load_average: Some([8.43, 6.5, 5.86]),
                temperature_c: Some(61.0),
            },
            gpu: Some(GpuStatus {
                usage_percent: Some(31.0),
                core_count: Some(20),
            }),
            boot_volume: Some("Macintosh HD".to_string()),
            volumes: vec![VolumeStatus {
                name: "Time Machine".to_string(),
                total_bytes: 2_000_000_000_000,
                available_bytes: 150_000_000_000,
            }],
            disk_io: Some(DiskIo {
                read_per_sec: 240_000.0,
                write_per_sec: 5_500_000.0,
            }),
            network: NetworkStatus {
                down_per_sec: Some(1_200_000.0),
                up_per_sec: Some(84_000.0),
                local: Some(InterfaceAddress {
                    interface: "en0".to_string(),
                    address: "192.168.1.20".to_string(),
                }),
                tunnel: Some(InterfaceAddress {
                    interface: "utun8".to_string(),
                    address: "100.64.0.7".to_string(),
                }),
            },
            power: Some(PowerStatus {
                charge_percent: 80,
                health_percent: Some(96),
                cycles: Some(124),
                source: PowerSource::PluggedIn,
                adapter_watts: Some(96),
                draw_watts: None,
                minutes_left: None,
                temperature_c: Some(31.0),
            }),
            processes: vec![
                ("node", 250.1, 1_000_000_000),
                ("WindowServer", 53.2, 171_000_000),
                ("workerd", 36.6, 166_300_000),
            ]
            .into_iter()
            .map(|(name, cpu_percent, memory_bytes)| ProcessStatus {
                name: name.to_string(),
                cpu_percent,
                memory_bytes,
            })
            .collect(),
            memory_processes: vec![
                ("Safari", 4.0, 2_400_000_000),
                ("node", 250.1, 1_000_000_000),
            ]
            .into_iter()
            .map(|(name, cpu_percent, memory_bytes)| ProcessStatus {
                name: name.to_string(),
                cpu_percent,
                memory_bytes,
            })
            .collect(),
        },
    }
}

fn status_with_history() -> SystemStatusState {
    let mut state = SystemStatusState::with_status(mock_system_status());
    for step in 0..200_u32 {
        let wave = (step % 13) as f64 / 12.0;
        let mut status = mock_system_status();
        status.live.cpu.usage_percent = Some((20.0 + 60.0 * wave) as f32);
        status.live.network.down_per_sec = Some(1_200_000.0 * wave);
        status.live.network.up_per_sec = Some(84_000.0 * wave);
        status.live.disk_io = Some(DiskIo {
            read_per_sec: 240_000.0 * wave,
            write_per_sec: 5_500_000.0 * (1.0 - wave),
        });
        state.apply(status);
    }
    state.apply(mock_system_status());
    state
}

const SIZES: &[(u16, u16, &str)] = &[(80, 24, "80x24"), (120, 32, "120x32"), (50, 16, "50x16")];

#[test]
fn snapshot_workspace() {
    let theme = snapshot_theme();
    let mut state = WorkspaceState::new();
    state.disk_status = Some("118.6 GB available · 24% free".to_string());
    for &(w, h, label) in SIZES {
        let output = render_to_string(w, h, |f| render_workspace(f, &state, &theme));
        assert_snapshot!(format!("workspace_{label}"), output);
    }
}

#[test]
fn snapshot_workspace_clean_submenu() {
    let theme = snapshot_theme();
    let mut state = WorkspaceState::new();
    state.disk_status = Some("118.6 GB available · 24% free".to_string());
    state.open_clean_submenu();
    for &(w, h, label) in SIZES {
        let output = render_to_string(w, h, |f| render_workspace(f, &state, &theme));
        assert_snapshot!(format!("workspace_submenu_{label}"), output);
    }
}

#[test]
fn snapshot_workspace_confirm() {
    let theme = snapshot_theme();
    let mut state = WorkspaceState::new();
    state.disk_status = Some("118.6 GB available · 24% free".to_string());
    state.open_confirmation(StartConfirmKind::ExploreHome);
    for &(w, h, label) in SIZES {
        let output = render_to_string(w, h, |f| render_workspace(f, &state, &theme));
        assert_snapshot!(format!("workspace_confirm_{label}"), output);
    }
}

#[test]
fn snapshot_select() {
    let theme = snapshot_theme();
    let state = SelectState::new(sample_scan());
    for &(w, h, label) in SIZES {
        let output = render_to_string(w, h, |f| render_select(f, &state, &theme));
        assert_snapshot!(format!("select_{label}"), output);
    }
}

#[test]
fn snapshot_review() {
    let theme = snapshot_theme();
    let state = ReviewState::new(sample_scan());
    for &(w, h, label) in SIZES {
        let output = render_to_string(w, h, |f| render_review(f, &state, &theme));
        assert_snapshot!(format!("review_{label}"), output);
    }
}

fn sample_analyze() -> AnalyzeState {
    AnalyzeState {
        root: PathBuf::from("/Users/test/Documents"),
        levels: vec![taert::ui::analyze::BrowserLevel {
            snapshot: taert::ui::analyze::FolderSnapshot {
                path: PathBuf::from("/Users/test/Documents"),
                children: vec![
                    taert::ui::analyze::ChildItem {
                        name: "file.bin".to_string(),
                        path: PathBuf::from("/Users/test/Documents/file.bin"),
                        size_bytes: 4096,
                        is_dir: false,
                        is_package: false,
                        selected: false,
                        created: None,
                    },
                    taert::ui::analyze::ChildItem {
                        name: "subdir".to_string(),
                        path: PathBuf::from("/Users/test/Documents/subdir"),
                        size_bytes: 0,
                        is_dir: true,
                        is_package: false,
                        selected: false,
                        created: None,
                    },
                ],
                total_bytes: 4096,
                inaccessible_count: 0,
                skipped_count: 0,
            },
            selected: 0,
            offset: 0,
        }],
        ignore: IgnoreStore::default(),
        measuring_rx: None,
        cancel_flag: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        current_progress: None,
        spinner_idx: 0,
        status_notice: None,
        started_at: std::time::Instant::now(),
        overlay: taert::ui::analyze::AnalyzeOverlay::None,
        cache: std::sync::Arc::new(std::sync::RwLock::new(
            taert::core::cache::StorageCache::new(),
        )),
        sort_order: taert::ui::analyze::SortOrder::default(),
    }
}

#[test]
fn snapshot_analyze() {
    let theme = snapshot_theme();
    let state = sample_analyze();

    for &(w, h, label) in SIZES {
        let output = render_to_string(w, h, |f| render_analyze(f, &state, &theme));
        assert_snapshot!(format!("analyze_{label}"), output);
    }
}

#[test]
fn snapshot_system_status() {
    let theme = snapshot_theme();
    let state = status_with_history();
    for &(w, h, label) in SIZES {
        let output = render_to_string(w, h, |f| render_system_status(f, &state, &theme));
        assert_snapshot!(format!("system_status_{label}"), output);
    }
}

#[test]
fn snapshot_system_status_wide_and_tall() {
    let theme = snapshot_theme();
    let state = status_with_history();
    let output = render_to_string(140, 40, |f| render_system_status(f, &state, &theme));
    assert_snapshot!("system_status_140x40", output);
}

#[test]
fn snapshot_system_status_large_window() {
    let theme = snapshot_theme();
    let state = status_with_history();
    let output = render_to_string(205, 85, |f| render_system_status(f, &state, &theme));
    assert_snapshot!("system_status_205x85", output);
}

#[test]
fn snapshot_progress() {
    let theme = snapshot_theme();
    let mut state = ProgressState::new("Scanning developer caches");
    state.headline = "Scanning Developer, 2 of 5".to_string();
    state.folder = Some("~/Library/Caches/Cargo".to_string());
    state.found_count = 12;
    state.found_bytes = 42_000_000;
    for &(w, h, label) in SIZES {
        let output = render_to_string(w, h, |f| render_progress(f, &state, &theme));
        assert_snapshot!(format!("progress_{label}"), output);
    }
}

#[test]
fn snapshot_preferences() {
    let theme = snapshot_theme();
    let config = Config::default();
    let ignore = IgnoreStore::default();
    let state = PreferencesState::new(&config, &ignore);
    for &(w, h, label) in SIZES {
        let output = render_to_string(w, h, |f| render_preferences(f, &state, &theme));
        assert_snapshot!(format!("preferences_{label}"), output);
    }
}

#[test]
fn snapshot_path_list_editor() {
    let theme = snapshot_theme();
    let ignore = IgnoreStore {
        roots: vec![
            "/Users/test/Documents/Secret".to_string(),
            "/Users/test/Projects/keep-this".to_string(),
        ],
    };
    let state = PathListEditorState::new(PathListKind::ProtectedPaths, &ignore);
    for &(w, h, label) in SIZES {
        let output = render_to_string(w, h, |f| render_path_list_editor(f, &state, &theme));
        assert_snapshot!(format!("path_list_editor_{label}"), output);
    }
}

const WIDE_WINDOW: (u16, u16) = (205, 60);

// The rows between the title bar and the toolbar.
fn body_rows(output: &str) -> Vec<&str> {
    let lines: Vec<&str> = output.lines().collect();
    lines[2..lines.len() - 2].to_vec()
}

fn right_edge(output: &str) -> usize {
    body_rows(output)
        .iter()
        .map(|row| row.chars().count())
        .max()
        .unwrap_or(0)
}

fn left_edge(output: &str) -> usize {
    body_rows(output)
        .iter()
        .filter(|row| !row.trim().is_empty())
        .map(|row| row.chars().take_while(|c| *c == ' ').count())
        .min()
        .unwrap_or(0)
}

#[test]
fn data_screens_fill_a_wide_window() {
    let theme = snapshot_theme();
    let (w, h) = WIDE_WINDOW;
    let select = SelectState::new(sample_scan());
    let analyze = sample_analyze();
    let status = status_with_history();

    let screens = [
        (
            "select",
            render_to_string(w, h, |f| render_select(f, &select, &theme)),
        ),
        (
            "analyze",
            render_to_string(w, h, |f| render_analyze(f, &analyze, &theme)),
        ),
        (
            "status",
            render_to_string(w, h, |f| render_system_status(f, &status, &theme)),
        ),
    ];

    for (name, output) in screens {
        assert!(right_edge(&output) > 190, "{name} stops early:\n{output}");
        assert!(left_edge(&output) < 10, "{name} is indented:\n{output}");
    }
}

#[test]
fn the_home_menu_starts_at_the_left_edge_of_a_wide_window() {
    let theme = snapshot_theme();
    let (w, h) = WIDE_WINDOW;
    let state = WorkspaceState::new();

    let output = render_to_string(w, h, |f| render_workspace(f, &state, &theme));

    assert!(left_edge(&output) < 10, "{output}");
}

#[test]
fn text_screens_keep_a_readable_width_at_the_left_edge_of_a_wide_window() {
    let theme = snapshot_theme();
    let (w, h) = WIDE_WINDOW;
    let config = Config::default();
    let ignore = IgnoreStore::default();
    let state = PreferencesState::new(&config, &ignore);

    let output = render_to_string(w, h, |f| render_preferences(f, &state, &theme));

    assert!(right_edge(&output) <= 113, "{output}");
    assert!(left_edge(&output) < 10, "{output}");
}
