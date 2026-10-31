use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use insta::assert_snapshot;
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use taert::core::cache::StorageCache;
use taert::core::docker::DockerObject;
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
    state.system = Some(Box::new(status_with_history()));
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
    state.system = Some(Box::new(status_with_history()));
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
    state.system = Some(Box::new(status_with_history()));
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
        preview: Default::default(),
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

fn home_state(row: usize) -> WorkspaceState {
    let mut state = WorkspaceState::new();
    state.disk_status = Some("118.6 GB available · 24% free".to_string());
    state.system = Some(Box::new(status_with_history()));
    state.selected_index = row;
    state
}

fn home_screen(state: &WorkspaceState, w: u16, h: u16) -> String {
    let theme = snapshot_theme();
    render_to_string(w, h, |f| render_workspace(f, state, &theme))
}

const CLEAN_CACHES: usize = 0;
const PROJECT_CLEANUP: usize = 1;
const DOCKER_CLEANUP: usize = 2;
const EXPLORE_STORAGE: usize = 3;
const SYSTEM_STATUS: usize = 4;
const PREFERENCES: usize = 5;

#[test]
fn cleanup_rows_show_how_full_the_disk_is() {
    for row in [CLEAN_CACHES, PROJECT_CLEANUP] {
        let output = home_screen(&home_state(row), 120, 32);

        assert!(output.contains("STORAGE"), "{output}");
        assert!(output.contains("Macintosh HD"), "{output}");
    }
}

#[test]
fn the_disk_on_the_main_menu_has_no_speed_rows() {
    let output = home_screen(&home_state(CLEAN_CACHES), 120, 32);

    assert!(!output.contains("Measuring"), "{output}");
    assert!(!output.contains("Read"), "{output}");
}

#[test]
fn the_status_row_shows_cpu_and_memory() {
    let output = home_screen(&home_state(SYSTEM_STATUS), 120, 32);

    assert!(output.contains("CPU"), "{output}");
    assert!(output.contains("MEMORY"), "{output}");
    assert!(!output.contains("PROCESSES"), "{output}");
}

#[test]
fn the_status_row_fits_a_large_window_in_two_columns() {
    let output = home_screen(&home_state(SYSTEM_STATUS), 205, 60);

    let row = output.lines().find(|line| line.contains("CPU")).unwrap();
    assert!(row.contains("MEMORY"), "{output}");
    assert!(body_rows(&output).iter().any(|row| row.contains('█')));
}

#[test]
fn the_explore_row_lists_the_largest_items_once_home_is_measured() {
    let mut state = home_state(EXPLORE_STORAGE);
    let before = home_screen(&state, 120, 32);
    assert!(before.contains("Open Explore storage"), "{before}");

    state.largest = sample_analyze().levels.pop().map(|level| level.snapshot);

    let after = home_screen(&state, 120, 32);
    assert!(after.contains("file.bin"), "{after}");
    assert!(after.contains("LARGEST"), "{after}");
    assert!(!after.contains("Open Explore storage"), "{after}");
}

#[test]
fn other_rows_keep_only_their_description() {
    let output = home_screen(&home_state(PREFERENCES), 120, 32);

    assert!(!output.contains("STORAGE"), "{output}");
    assert!(!output.contains("CPU"), "{output}");
}

#[test]
fn a_narrow_main_menu_shows_no_data() {
    let output = home_screen(&home_state(CLEAN_CACHES), 80, 24);

    assert!(!output.contains("STORAGE"), "{output}");
}

#[test]
fn the_sampler_runs_only_while_the_status_row_is_highlighted() {
    let cache = Arc::new(RwLock::new(StorageCache::new()));
    let is_live = |state: &WorkspaceState| state.system.as_ref().unwrap().is_live();
    let mut state = home_state(SYSTEM_STATUS);

    state.tick(&cache);
    assert!(!is_live(&state), "the pane was never drawn");

    home_screen(&state, 120, 32);
    state.tick(&cache);
    assert!(is_live(&state));

    state.selected_index = CLEAN_CACHES;
    state.tick(&cache);
    assert!(!is_live(&state));
}

#[test]
fn the_explore_row_picks_up_a_measured_home_folder() {
    let home = taert::core::safety::Safety::home_dir();
    let cache = Arc::new(RwLock::new(StorageCache::new()));
    let mut snapshot = sample_analyze().levels.pop().unwrap().snapshot;
    snapshot.path = home.clone();
    cache.write().unwrap().put_snapshot(home, snapshot);
    let mut state = home_state(EXPLORE_STORAGE);

    state.tick(&cache);

    assert!(state.largest.is_some());
}

#[test]
fn snapshot_main_menu_rows_with_data() {
    let theme = snapshot_theme();
    for (row, name) in [(CLEAN_CACHES, "clean"), (SYSTEM_STATUS, "status")] {
        let state = home_state(row);
        let output = render_to_string(140, 40, |f| render_workspace(f, &state, &theme));
        assert_snapshot!(format!("workspace_{name}_140x40"), output);
    }
}

fn docker_item(object: DockerObject, label: &str, size: u64, selected: bool) -> ScanItem {
    ScanItem::docker(object, label.to_string(), size, selected)
}

fn docker_scan() -> CategoryScan {
    let image = |id: &str, tags: &[&str]| DockerObject::Image {
        id: id.to_string(),
        tags: tags.iter().map(|tag| tag.to_string()).collect(),
    };
    CategoryScan {
        id: "docker".to_string(),
        title: "Docker".to_string(),
        groups: vec![
            ToolGroup {
                tool: "Images".to_string(),
                items: vec![
                    docker_item(image("sha256:1", &["app:1"]), "app:1", 900_000_000, false),
                    docker_item(image("sha256:2", &[]), "<untagged> 2", 200_000_000, true),
                ],
            },
            ToolGroup {
                tool: "Volumes (data is deleted for good)".to_string(),
                items: vec![docker_item(
                    DockerObject::Volume {
                        name: "pgdata".to_string(),
                    },
                    "pgdata",
                    400_000_000,
                    false,
                )],
            },
            ToolGroup {
                tool: "Build cache".to_string(),
                items: vec![docker_item(
                    DockerObject::BuildCache,
                    "Build cache",
                    10_000_000_000,
                    false,
                )],
            },
        ],
    }
}

fn select_screen(state: &SelectState) -> String {
    let theme = snapshot_theme();
    render_to_string(120, 32, |f| render_select(f, state, &theme))
}

fn select_item(state: &mut SelectState, group: usize, item: usize) {
    state.scan.groups[group].items[item].selected = true;
}

#[test]
fn snapshot_select_docker() {
    let state = SelectState::with_preserved_selection(docker_scan());

    assert_snapshot!("select_docker_120x32", select_screen(&state));
}

#[test]
fn docker_has_its_own_breadcrumb() {
    let output = select_screen(&SelectState::with_preserved_selection(docker_scan()));

    let title = output.lines().next().unwrap();
    assert!(title.contains("Docker"), "{title}");
    assert!(!title.contains("Clean caches"), "{title}");
}

#[test]
fn removing_a_volume_says_that_its_data_goes_with_it() {
    let mut state = SelectState::with_preserved_selection(docker_scan());
    select_item(&mut state, 1, 0);
    state.open_confirmation();

    let output = select_screen(&state);

    assert!(output.contains("The data in 1 volume"), "{output}");
}

#[test]
fn removing_images_alone_does_not_mention_volumes() {
    let mut state = SelectState::with_preserved_selection(docker_scan());
    state.open_confirmation();

    let output = select_screen(&state);

    assert!(output.contains("Remove 1 item?"), "{output}");
    assert!(!output.contains("The data in"), "{output}");
}

#[test]
fn an_unreachable_docker_explains_itself() {
    let state = SelectState::unavailable("Docker", "Can't reach Docker.".to_string());

    let output = select_screen(&state);

    assert!(output.contains("Docker isn't available"), "{output}");
    assert!(output.contains("Can't reach Docker."), "{output}");
    assert!(!output.contains("Nothing to clean"), "{output}");
}

#[test]
fn a_docker_with_nothing_unused_says_so_in_its_own_words() {
    let scan = CategoryScan {
        groups: Vec::new(),
        ..docker_scan()
    };

    let output = select_screen(&SelectState::new(scan));

    assert!(output.contains("Docker has nothing unused"), "{output}");
    assert!(!output.contains("caches"), "{output}");
}

#[test]
fn the_docker_row_sits_in_cleanup_and_says_what_it_does() {
    let output = home_screen(&home_state(DOCKER_CLEANUP), 120, 32);

    assert!(output.contains("Docker cleanup"), "{output}");
    assert!(output.contains("never selected for you"), "{output}");
}
