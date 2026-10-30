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
use taert::ui::analyze::{AnalyzeState, render_analyze};
use taert::ui::path_list_editor::{PathListEditorState, PathListKind, render_path_list_editor};
use taert::ui::preferences::{PreferencesState, render_preferences};
use taert::ui::progress::{ProgressState, render_progress};
use taert::ui::review::{ReviewState, render_review};
use taert::ui::select::{SelectState, render_select};
use taert::ui::system_status::{SystemStatusState, render_system_status};
use taert::ui::theme::Theme;
use taert::ui::workspace::{StartConfirmKind, WorkspaceState, render_workspace};

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
            uptime_seconds: 879600,
        },
        memory: DetailedMemoryStatus {
            total_bytes: 48 * 1024 * 1024 * 1024,
            free_bytes: 11 * 1024 * 1024 * 1024,
            active_bytes: 9_900_000_000,
            inactive_bytes: 9_600_000_000,
            wired_bytes: 5_700_000_000,
            compressed_bytes: 11_200_000_000,
            pressure: Some(MemoryPressureLevel::Normal),
        },
        storage: DetailedStorageStatus {
            mount_point: "/".to_string(),
            total_bytes: 494_400_000_000,
            available_bytes: 118_600_000_000,
            free_bytes: 118_600_000_000,
            managed_bytes: 5_700_000_000,
        },
    }
}

const SIZES: &[(u16, u16, &str)] = &[(80, 24, "80x24"), (120, 32, "120x32"), (50, 16, "50x16")];

#[test]
fn snapshot_workspace() {
    let theme = Theme::new();
    let mut state = WorkspaceState::new();
    state.disk_status = Some("118.6 GB available · 24% free".to_string());
    for &(w, h, label) in SIZES {
        let output = render_to_string(w, h, |f| render_workspace(f, &state, &theme));
        assert_snapshot!(format!("workspace_{label}"), output);
    }
}

#[test]
fn snapshot_workspace_clean_submenu() {
    let theme = Theme::new();
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
    let theme = Theme::new();
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
    let theme = Theme::new();
    let state = SelectState::new(sample_scan());
    for &(w, h, label) in SIZES {
        let output = render_to_string(w, h, |f| render_select(f, &state, &theme));
        assert_snapshot!(format!("select_{label}"), output);
    }
}

#[test]
fn snapshot_review() {
    let theme = Theme::new();
    let state = ReviewState::new(sample_scan());
    for &(w, h, label) in SIZES {
        let output = render_to_string(w, h, |f| render_review(f, &state, &theme));
        assert_snapshot!(format!("review_{label}"), output);
    }
}

#[test]
fn snapshot_analyze() {
    let theme = Theme::new();
    let state = AnalyzeState {
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
                    },
                    taert::ui::analyze::ChildItem {
                        name: "subdir".to_string(),
                        path: PathBuf::from("/Users/test/Documents/subdir"),
                        size_bytes: 0,
                        is_dir: true,
                        is_package: false,
                        selected: false,
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
    };

    for &(w, h, label) in SIZES {
        let output = render_to_string(w, h, |f| render_analyze(f, &state, &theme));
        assert_snapshot!(format!("analyze_{label}"), output);
    }
}

#[test]
fn snapshot_system_status() {
    let theme = Theme::new();
    let state = SystemStatusState {
        status: mock_system_status(),
        scroll_offset: 0,
    };
    for &(w, h, label) in SIZES {
        let output = render_to_string(w, h, |f| render_system_status(f, &state, &theme));
        assert_snapshot!(format!("system_status_{label}"), output);
    }
}

#[test]
fn snapshot_progress() {
    let theme = Theme::new();
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
    let theme = Theme::new();
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
    let theme = Theme::new();
    let mut ignore = IgnoreStore::default();
    ignore.roots = vec![
        "/Users/test/Documents/Secret".to_string(),
        "/Users/test/Projects/keep-this".to_string(),
    ];
    let state = PathListEditorState::new(PathListKind::ProtectedPaths, &ignore);
    for &(w, h, label) in SIZES {
        let output = render_to_string(w, h, |f| render_path_list_editor(f, &state, &theme));
        assert_snapshot!(format!("path_list_editor_{label}"), output);
    }
}
