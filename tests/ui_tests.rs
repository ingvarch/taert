use std::path::PathBuf;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use taert::core::scanner::{CategoryScan, ScanItem, ToolGroup};
use taert::ui::review::{ReviewState, render_review};
use taert::ui::select::{SelectState, render_select};
use taert::ui::theme::Theme;
use taert::ui::workspace::{WorkspaceState, render_workspace};

fn sample_scan() -> CategoryScan {
    CategoryScan {
        notes: Vec::new(),
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

#[test]
fn test_render_workspace_screen() {
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let state = WorkspaceState::new();
    let theme = Theme::new();

    terminal
        .draw(|f| {
            render_workspace(f, &state, &theme);
        })
        .unwrap();

    let buffer = terminal.backend().buffer();
    let rendered_text = format!("{buffer:?}");
    assert!(rendered_text.contains("taert"));
    assert!(rendered_text.contains("Clean caches"));
    assert!(rendered_text.contains("Project cleanup"));
    assert!(rendered_text.contains("Explore storage"));
    assert!(rendered_text.contains("System status"));
    assert!(rendered_text.contains("Preferences"));
    assert!(rendered_text.contains("Protected paths"));
}

#[test]
fn test_render_workspace_clean_submenu() {
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut state = WorkspaceState::new();
    state.open_clean_submenu();
    let theme = Theme::new();

    terminal
        .draw(|f| {
            render_workspace(f, &state, &theme);
        })
        .unwrap();

    let buffer = terminal.backend().buffer();
    let rendered_text = format!("{buffer:?}");
    assert!(rendered_text.contains("Clean caches"));
    assert!(rendered_text.contains("Developer caches"));
    assert!(rendered_text.contains("User junk"));
    assert!(rendered_text.contains("AI tools"));
    assert!(rendered_text.contains("Trash"));
}

#[test]
fn the_user_junk_row_says_it_holds_app_and_browser_caches() {
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    let state = WorkspaceState::at(0, Some(2));
    let theme = Theme::new();
    terminal
        .draw(|f| render_workspace(f, &state, &theme))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let text: String = (0..30)
        .flat_map(|y| (0..100).map(move |x| (x, y)))
        .map(|(x, y)| buffer[(x, y)].symbol())
        .collect();
    assert!(text.contains("Logs, Xcode files, app and browser caches"));
}

#[test]
fn test_render_workspace_confirm_dialog() {
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut state = WorkspaceState::new();
    state.open_confirmation(taert::ui::workspace::StartConfirmKind::ExploreHome);
    let theme = Theme::new();

    terminal
        .draw(|f| {
            render_workspace(f, &state, &theme);
        })
        .unwrap();

    let buffer = terminal.backend().buffer();
    let rendered_text = format!("{buffer:?}");
    assert!(rendered_text.contains("Explore your home folder?"));
    assert!(rendered_text.contains("Cancel"));
    assert!(rendered_text.contains("Start"));
    assert!(rendered_text.contains("Enter"));
    assert!(rendered_text.contains("select"));
    assert!(rendered_text.contains("Esc"));
    assert!(rendered_text.contains("back"));
}

#[test]
fn test_render_workspace_project_cleanup_dialog() {
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut state = WorkspaceState::new();
    state.open_confirmation(taert::ui::workspace::StartConfirmKind::ProjectCleanup(6));
    let theme = Theme::new();

    terminal
        .draw(|f| {
            render_workspace(f, &state, &theme);
        })
        .unwrap();

    let buffer = terminal.backend().buffer();
    let rendered_text = format!("{buffer:?}");
    assert!(rendered_text.contains("Scan your project folders?"));
    assert!(rendered_text.contains("Searching 6 folders for build artifacts"));
    assert!(rendered_text.contains("minutes."));
    assert!(rendered_text.contains("Cancel"));
    assert!(rendered_text.contains("Start"));
}

#[test]
fn test_render_select_screen() {
    let backend = TestBackend::new(120, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let scan = sample_scan();
    let state = SelectState::new(scan);
    assert_eq!(state.scan.selected_items().len(), 0);
    let theme = Theme::new();

    terminal
        .draw(|f| {
            render_select(f, &state, &theme);
        })
        .unwrap();

    let buffer = terminal.backend().buffer();
    let rendered_text = format!("{buffer:?}");
    assert!(rendered_text.contains("Test Scan"));
    assert!(rendered_text.contains("Cargo"));
}

#[test]
fn test_render_review_screen() {
    let backend = TestBackend::new(120, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let scan = sample_scan();
    let state = ReviewState::new(scan);
    let theme = Theme::new();

    terminal
        .draw(|f| {
            render_review(f, &state, &theme);
        })
        .unwrap();

    let buffer = terminal.backend().buffer();
    let rendered_text = format!("{buffer:?}");
    assert!(rendered_text.contains("permanently deleted"));
}

#[test]
fn test_render_analyze_screen() {
    let backend = TestBackend::new(120, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let dir_path = temp.path().to_path_buf();

    std::fs::create_dir_all(dir_path.join("subdir")).unwrap();
    std::fs::write(dir_path.join("file.bin"), b"1234567890").unwrap();

    let state = taert::ui::analyze::AnalyzeState::new_sync(
        dir_path,
        taert::core::stores::IgnoreStore::default(),
    );
    let theme = Theme::new();

    terminal
        .draw(|f| {
            taert::ui::analyze::render_analyze(f, &state, &theme);
        })
        .unwrap();

    let buffer = terminal.backend().buffer();
    let rendered_text = format!("{buffer:?}");
    assert!(rendered_text.contains("Explore"));
    assert!(rendered_text.contains("subdir/"));
    assert!(rendered_text.contains("file.bin"));
}

#[test]
fn test_render_system_status_screen() {
    let backend = TestBackend::new(120, 90);
    let mut terminal = Terminal::new(backend).unwrap();
    let state = taert::ui::system_status::SystemStatusState::new();
    let theme = Theme::new();

    terminal
        .draw(|f| {
            taert::ui::system_status::render_system_status(f, &state, &theme);
        })
        .unwrap();

    let buffer = terminal.backend().buffer();
    let rendered_text = format!("{buffer:?}");
    assert!(rendered_text.contains("System status"));
    assert!(rendered_text.contains("Memory"));
    assert!(rendered_text.contains("Storage"));
    assert!(rendered_text.contains("Free"));
    assert!(rendered_text.contains("of"));
    assert!(rendered_text.contains("Managed space may include purgeable"));
}

#[test]
fn test_select_state_page_and_home_end_navigation() {
    let mut scan = sample_scan();
    // Add multiple items to test page and boundary navigation
    for i in 0..25 {
        scan.groups[0].items.push(ScanItem::new(
            PathBuf::from(format!("/Users/test/.cargo/item_{i}")),
            1_000_000,
            false,
            false,
        ));
    }

    let mut state = SelectState::new(scan);
    // Initially positioned at first selectable item (row 1, since row 0 is header)
    assert_eq!(state.focused_index, 1);

    // Page down moves by 10 items
    state.page_down();
    assert_eq!(state.focused_index, 11);

    // Page down again
    state.page_down();
    assert_eq!(state.focused_index, 21);

    // Move to end moves to last selectable item
    state.move_to_end();
    assert_eq!(state.focused_index, 27);

    // Page up moves backwards by 10
    state.page_up();
    assert_eq!(state.focused_index, 17);

    // Move to start moves to first selectable item
    state.move_to_start();
    assert_eq!(state.focused_index, 1);
}

#[test]
fn test_analyze_and_review_page_navigation() {
    let scan = sample_scan();
    let mut review = ReviewState::new(scan);
    assert_eq!(review.scroll_offset, 0);
    review.page_down();
    assert_eq!(review.scroll_offset, 10);
    review.page_up();
    assert_eq!(review.scroll_offset, 0);

    let temp = tempfile::tempdir().unwrap();
    let dir_path = temp.path().to_path_buf();
    for i in 0..30 {
        std::fs::write(dir_path.join(format!("file_{i}.txt")), b"hello").unwrap();
    }
    let mut analyze = taert::ui::analyze::AnalyzeState::new_sync(
        dir_path,
        taert::core::stores::IgnoreStore::default(),
    );
    assert_eq!(analyze.current_level().unwrap().selected, 0);
    analyze.page_down();
    assert_eq!(analyze.current_level().unwrap().selected, 10);
    analyze.move_to_end();
    assert_eq!(analyze.current_level().unwrap().selected, 29);
    analyze.page_up();
    assert_eq!(analyze.current_level().unwrap().selected, 19);
    analyze.move_to_start();
    assert_eq!(analyze.current_level().unwrap().selected, 0);
}

#[test]
fn test_render_progress_screen_with_live_folders() {
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut state = taert::ui::progress::ProgressState::new("Scanning project artifacts");
    let theme = Theme::new();

    state.apply_purge(taert::core::purger::PurgeScanEvent::Root(
        "~/Repos".to_string(),
    ));
    assert_eq!(state.headline, "Scanning ~/Repos");
    assert!(state.folder.is_none());

    state.apply_purge(taert::core::purger::PurgeScanEvent::Walking(
        "~/Repos/ingvarch/taert/target".to_string(),
    ));
    assert_eq!(
        state.folder.as_deref(),
        Some("~/Repos/ingvarch/taert/target")
    );

    state.apply_purge(taert::core::purger::PurgeScanEvent::Found { bytes: 50_000_000 });
    assert_eq!(state.found_count, 1);
    assert_eq!(state.found_bytes, 50_000_000);

    terminal
        .draw(|f| {
            taert::ui::progress::render_progress(f, &state, &theme);
        })
        .unwrap();

    let buffer = terminal.backend().buffer();
    let rendered = format!("{buffer:?}");
    assert!(rendered.contains("Scanning ~/Repos"));
    assert!(rendered.contains("~/Repos/ingvarch/taert/target"));
    assert!(rendered.contains("Found 1 item · 50.0 MB"));
    assert!(rendered.contains("Cancel"));
}

#[test]
fn select_state_carries_the_notes_of_a_scan() {
    let mut scan = sample_scan();
    scan.notes = vec!["Safari needs Full Disk Access.".to_string()];
    let state = SelectState::new(scan);
    assert_eq!(state.notes, ["Safari needs Full Disk Access."]);
}

fn render_text(state: &SelectState, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let theme = Theme::new();
    terminal.draw(|f| render_select(f, state, &theme)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn scan_with_a_row_of_two_folders() -> CategoryScan {
    let mut scan = sample_scan();
    scan.groups[0].items[0].also = vec![ScanItem::new(
        PathBuf::from("/Users/test/Library/Containers/second"),
        5_000_000,
        true,
        false,
    )];
    scan
}

#[test]
fn a_row_of_two_folders_says_it_holds_one_more() {
    let state = SelectState::new(scan_with_a_row_of_two_folders());
    let text = render_text(&state, 100, 30);
    assert!(text.contains("+1 folder"), "{text}");
}

#[test]
fn a_narrow_screen_cuts_the_path_and_keeps_the_folder_count() {
    let state = SelectState::new(scan_with_a_row_of_two_folders());
    let text = render_text(&state, 50, 16);
    assert!(text.contains("+1 folder"), "{text}");
}

#[test]
fn search_finds_a_row_by_its_second_folder() {
    let mut state = SelectState::new(scan_with_a_row_of_two_folders());
    state.search_query = "containers/second".to_string();
    let found: Vec<_> = state
        .flattened_rows()
        .into_iter()
        .filter(|row| row.is_item())
        .collect();
    assert_eq!(found.len(), 1);
}

#[test]
fn the_review_screen_names_every_folder_of_a_row() {
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    let state = ReviewState::new(scan_with_a_row_of_two_folders());
    let theme = Theme::new();
    terminal.draw(|f| render_review(f, &state, &theme)).unwrap();
    let text = format!("{:?}", terminal.backend().buffer());
    assert!(text.contains("/.cargo/registry/cache"), "{text}");
    assert!(text.contains("Library/Containers/second"), "{text}");
}

#[test]
fn the_confirmation_names_the_second_folder_of_a_selected_row() {
    let mut state = SelectState::with_preserved_selection(scan_with_a_row_of_two_folders());
    state.open_confirmation();
    let text = render_text(&state, 100, 30);
    assert!(text.contains("Also removes"), "{text}");
    assert!(text.contains("Library/Containers/second"), "{text}");
}

#[test]
fn the_confirmation_leaves_out_the_folders_of_a_row_not_selected() {
    let mut scan = scan_with_a_row_of_two_folders();
    scan.groups[0].items[0].selected = false;
    scan.groups[0].items[1].selected = true;
    let mut state = SelectState::with_preserved_selection(scan);
    state.open_confirmation();
    let text = render_text(&state, 100, 30);
    assert!(!text.contains("Also removes"), "{text}");
}

#[test]
fn a_note_shows_on_the_list_without_opening_the_dialog() {
    let mut scan = sample_scan();
    scan.notes = vec!["Safari needs Full Disk Access.".to_string()];
    let text = render_text(&SelectState::new(scan), 100, 30);
    assert!(text.contains("Safari needs Full Disk Access."), "{text}");
}

const LONG_NOTES: [&str; 2] = [
    "Safari keeps a cache that macOS hides from this terminal \
     (~/Library/Caches/com.apple.Safari). \
     Give the terminal Full Disk Access in System Settings to include it.",
    "Safari keeps a cache that macOS hides from this terminal \
     (~/Library/Containers/com.apple.Safari/Data/Library/Caches). \
     Give the terminal Full Disk Access in System Settings to include it.",
];

fn without_spaces(text: &str) -> String {
    text.split_whitespace().collect()
}

fn assert_notes_fit(rows_found: bool) {
    for height in [24, 30, 40, 50] {
        for width in 60..=160 {
            let mut scan = sample_scan();
            if !rows_found {
                scan.groups.clear();
            }
            scan.notes = LONG_NOTES.map(String::from).to_vec();
            let text = without_spaces(&render_text(&SelectState::new(scan), width, height));
            for note in LONG_NOTES {
                let note = without_spaces(note);
                assert!(
                    text.contains(&note),
                    "{width}x{height} lost: {note}\n{text}"
                );
            }
        }
    }
}

#[test]
fn long_notes_show_whole_at_every_width() {
    assert_notes_fit(true);
}

#[test]
fn long_notes_show_whole_when_the_scan_found_no_rows() {
    assert_notes_fit(false);
}

#[test]
fn a_note_shows_when_the_scan_found_no_rows() {
    let mut scan = sample_scan();
    scan.groups.clear();
    scan.notes = vec!["Safari needs Full Disk Access.".to_string()];
    let text = render_text(&SelectState::new(scan), 100, 30);
    assert!(text.contains("Safari needs Full Disk Access."), "{text}");
}

mod doctor {
    use ratatui::buffer::Buffer;
    use ratatui::style::Color;
    use taert::core::doctor::{CheckResult, DoctorReport, Finding, FindingAction, FindingSeverity};
    use taert::ui::doctor::{DoctorActionConfirm, DoctorScreenState, render_doctor};
    use taert::ui::env::UiEnv;

    use super::*;

    fn theme() -> Theme {
        Theme::with_env(UiEnv::default())
    }

    fn draw(w: u16, h: u16, state: &DoctorScreenState, theme: &Theme) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| render_doctor(f, state, theme)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn row_text(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }

    fn text(buffer: &Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| row_text(buffer, y))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn finding(
        id: &str,
        severity: FindingSeverity,
        title: &str,
        evidence: &str,
        action: &str,
        target: Option<FindingAction>,
    ) -> Finding {
        Finding {
            id: id.to_string(),
            severity,
            title: title.to_string(),
            evidence: evidence.to_string(),
            action: action.to_string(),
            action_target: target,
        }
    }

    fn report(findings: Vec<Finding>) -> DoctorReport {
        DoctorReport {
            seconds: 10,
            findings,
            checks: Vec::new(),
        }
    }

    fn cpu_report() -> DoctorReport {
        report(vec![
            finding(
                "sustained_cpu",
                FindingSeverity::Warning,
                "node is using 190% CPU",
                "pid 4123, for the whole check",
                "Quit it if you don't need it.",
                Some(FindingAction::KillProcess {
                    pid: 4123,
                    name: "node".to_string(),
                }),
            ),
            finding(
                "wake_reasons",
                FindingSeverity::Notice,
                "The Mac woke 14 times in the last 24 hours",
                "12 times for wifibt",
                "Network features can wake the Mac while it sleeps.",
                None,
            ),
        ])
    }

    fn snapshot_report() -> DoctorReport {
        let mut report = cpu_report();
        report.findings.insert(
            1,
            finding(
                "sleep_blocker",
                FindingSeverity::Warning,
                "Something is keeping the Mac awake",
                "caffeinate has held PreventUserIdleSystemSleep for 02:14:09",
                "Quit it to let the Mac sleep.",
                Some(FindingAction::KillProcess {
                    pid: 70001,
                    name: "caffeinate".to_string(),
                }),
            ),
        );
        report.checks = [
            "sustained_cpu",
            "energy",
            "memory",
            "disk_space",
            "sleep_blockers",
            "wake_reasons",
            "heat",
            "network",
            "startup",
        ]
        .into_iter()
        .map(CheckResult::ok)
        .collect();
        report
    }

    #[test]
    fn all_clear_card_marks_a_skipped_check() {
        let theme = theme();
        let mut report = report(Vec::new());
        report.checks = vec![
            CheckResult::ok("sustained_cpu"),
            CheckResult::skipped("network", "nettop timed out"),
        ];
        let state = DoctorScreenState::new_done(report);

        let buffer = draw(120, 32, &state, &theme);

        let shown = text(&buffer);
        assert!(shown.contains("Sustained CPU usage"));
        assert!(shown.contains("Sustained network bandwidth (skipped: nettop timed out)"));
        assert!(!shown.contains("Power assertions"));
        assert!(!shown.contains("look normal"));
    }

    #[test]
    fn doctor_shows_evidence_and_action_when_narrow() {
        let theme = theme();
        let state = DoctorScreenState::new_done(snapshot_report());

        for (w, h) in [(80, 24), (50, 16)] {
            let shown = text(&draw(w, h, &state, &theme));
            assert!(shown.contains("pid 4123, for the whole check"), "{w}x{h}");
            assert!(shown.contains("Quit it if you don't need it."), "{w}x{h}");
        }
    }

    #[test]
    fn doctor_wraps_long_title_inside_the_narrow_card() {
        let theme = theme();
        let title = "The Mac woke 14 times in the last 24 hours";
        let state = DoctorScreenState::new_done(report(vec![finding(
            "x",
            FindingSeverity::Notice,
            title,
            "e",
            "a",
            None,
        )]));

        let buffer = draw(50, 16, &state, &theme);

        let rows: Vec<String> = (0..buffer.area.height)
            .map(|y| row_text(&buffer, y))
            .filter(|row| ["Mac", "woke", "hours"].iter().any(|w| row.contains(w)))
            .collect();
        for word in title.split(' ') {
            assert!(rows.iter().any(|r| r.contains(word)), "{word} was cut");
        }
        for row in rows
            .iter()
            .filter(|r| r.contains("woke") || r.contains("hours"))
        {
            assert!(row.trim_end().ends_with('\u{2502}'), "border lost: {row:?}");
        }
    }

    #[test]
    fn doctor_splits_a_long_path_inside_the_narrow_card() {
        let theme = theme();
        let path = "/opt/homebrew/Cellar/postgresql@16/16.4/bin/postgres";
        let state = DoctorScreenState::new_done(report(vec![finding(
            "x",
            FindingSeverity::Notice,
            "Title",
            &format!("Missing executable at {path}"),
            "a",
            None,
        )]));

        let buffer = draw(50, 16, &state, &theme);

        let shown: String = (0..buffer.area.height)
            .map(|y| row_text(&buffer, y))
            .flat_map(|row| {
                row.trim_end_matches(['\u{2502}', ' '])
                    .trim_start_matches(['\u{2502}', ' '])
                    .to_string()
                    .chars()
                    .collect::<Vec<_>>()
            })
            .collect();
        assert!(shown.contains(path), "path was cut");
    }

    #[test]
    fn doctor_wraps_long_evidence_by_words_when_narrow() {
        let theme = theme();
        let state = DoctorScreenState::new_done(report(vec![finding(
            "x",
            FindingSeverity::Notice,
            "Title",
            "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo",
            "Do something sensible about it.",
            None,
        )]));

        let shown = text(&draw(40, 20, &state, &theme));

        for word in ["alpha", "juliet", "kilo", "sensible"] {
            assert!(shown.contains(word), "{word} was cut");
        }
    }

    #[test]
    fn doctor_wraps_long_evidence_and_action_by_words_when_wide() {
        let theme = theme();
        let evidence = "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november";
        let action = "oscar papa quebec romeo sierra tango uniform victor whiskey xray yankee zulu";
        let state = DoctorScreenState::new_done(report(vec![finding(
            "x",
            FindingSeverity::Warning,
            "Title",
            evidence,
            action,
            None,
        )]));

        let buffer = draw(120, 32, &state, &theme);

        assert!(
            text(&buffer).contains("Suggested Action:"),
            "not the wide layout"
        );
        let words: Vec<&str> = evidence.split(' ').chain(action.split(' ')).collect();
        let rows: Vec<String> = (0..buffer.area.height)
            .map(|y| row_text(&buffer, y))
            .filter(|row| row.split_whitespace().any(|t| words.contains(&t)))
            .collect();
        for word in &words {
            assert!(
                rows.iter()
                    .any(|r| r.split_whitespace().any(|t| t == *word)),
                "{word} was cut"
            );
        }
        for row in &rows {
            assert!(row.trim_end().ends_with('\u{2502}'), "border lost: {row:?}");
        }
    }

    #[test]
    fn doctor_warning_is_yellow_and_critical_is_red() {
        let theme = theme();
        let state = DoctorScreenState::new_done(report(vec![
            finding("a", FindingSeverity::Warning, "Warn title", "e", "a", None),
            finding("b", FindingSeverity::Critical, "Crit title", "e", "a", None),
        ]));

        let buffer = draw(120, 32, &state, &theme);

        let symbol_fg = |title: &str| {
            let y = (0..buffer.area.height)
                .find(|&y| row_text(&buffer, y).contains(title))
                .unwrap();
            let x = (0..buffer.area.width)
                .find(|&x| buffer[(x, y)].symbol() == theme.g.warning)
                .unwrap();
            buffer[(x, y)].fg
        };
        assert_eq!(symbol_fg("Warn title"), theme.warning().fg.unwrap());
        assert_eq!(symbol_fg("Crit title"), theme.danger().fg.unwrap());
    }

    #[test]
    fn doctor_status_line_styles_errors() {
        let theme = theme();
        let message = "Failed to disable daemon: The administrator password was not given";
        let first_letter_fg = |state: &DoctorScreenState| {
            let buffer = draw(120, 32, state, &theme);
            let x = buffer.area.width - 1 - message.chars().count() as u16;
            assert_eq!(buffer[(x, 0)].symbol(), "F");
            buffer[(x, 0)].fg
        };

        let mut state = DoctorScreenState::new_done(cpu_report());
        state.message = Some((message.to_string(), true));
        assert_eq!(first_letter_fg(&state), theme.danger().fg.unwrap());

        state.message = Some((message.to_string(), false));
        assert_eq!(first_letter_fg(&state), theme.success().fg.unwrap());

        state.message = None;
        let buffer = draw(120, 32, &state, &theme);
        let x = buffer.area.width - 1 - "2 findings".len() as u16;
        assert_eq!(buffer[(x, 0)].symbol(), "2");
        assert_eq!(buffer[(x, 0)].fg, Color::Reset);
    }

    #[test]
    fn the_stop_dialog_names_sigterm() {
        let theme = theme();
        let mut state = DoctorScreenState::new_done(cpu_report());
        state.confirm = Some(DoctorActionConfirm::KillProcess {
            pid: 4123,
            name: "node".to_string(),
        });

        let shown = text(&draw(80, 24, &state, &theme));

        assert!(shown.contains("SIGTERM"));
        assert!(shown.contains("Stop Process"));
        assert!(!shown.contains("Force Quit"));
    }
}
