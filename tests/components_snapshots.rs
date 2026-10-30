use insta::assert_snapshot;
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::text::{Line, Span};
use taert::ui::components::button::{Button, render_button_group};
use taert::ui::components::chrome::AppChrome;
use taert::ui::components::empty::EmptyState;
use taert::ui::components::gauge::{bar, legend, segmented_bar};
use taert::ui::components::help::HelpSheet;
use taert::ui::components::hints::{Hint, PrimaryAction};
use taert::ui::components::list::{ListRow, render_section_header};
use taert::ui::components::modal::Modal;
use taert::ui::components::progress::ProgressView;
use taert::ui::components::toast::Toast;
use taert::ui::env::{Appearance, ColorDepth, UiEnv};
use taert::ui::theme::Theme;

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

fn dark_theme() -> Theme {
    Theme::with_env(UiEnv {
        appearance: Appearance::Dark,
        depth: ColorDepth::TrueColor,
        ascii_glyphs: false,
        reduce_motion: false,
    })
}

fn nocolor_theme() -> Theme {
    Theme::plain()
}

#[test]
fn snapshot_component_hints() {
    for (name, theme) in [("dark", dark_theme()), ("nocolor", nocolor_theme())] {
        let output = render_to_string(80, 5, |f| {
            let chrome = AppChrome::new(vec!["taert", "Clean caches"])
                .hints(vec![
                    Hint::new("↑↓", "Move"),
                    Hint::new("␣", "Select"),
                    Hint::new("/", "Search"),
                    Hint::new("?", "More"),
                ])
                .primary(PrimaryAction::new("Review 2 items", "↵"));
            chrome.render(f, &theme);
        });
        assert_snapshot!(format!("hints_{name}"), output);
    }
}

#[test]
fn snapshot_component_button() {
    for (name, theme) in [("dark", dark_theme()), ("nocolor", nocolor_theme())] {
        let output = render_to_string(60, 3, |f| {
            let buttons = vec![
                Button::new("Cancel").focused(false),
                Button::destructive("Remove 12 items").focused(true),
            ];
            let line = render_button_group(&buttons, &theme);
            f.render_widget(ratatui::widgets::Paragraph::new(line), f.area());
        });
        assert_snapshot!(format!("button_{name}"), output);
    }
}

#[test]
fn snapshot_component_list() {
    for (name, theme) in [("dark", dark_theme()), ("nocolor", nocolor_theme())] {
        let output = render_to_string(80, 6, |f| {
            let sec = render_section_header("Cargo", Some("2 items · 40.0 MB"), 80, &theme);
            let row1 = ListRow::new(vec![
                Span::styled("~/.cargo/registry/", theme.callout()),
                Span::styled("cache", theme.body()),
            ])
            .leading(Span::styled("●", theme.accent_text()))
            .detail(Span::styled("15.0 MB", theme.body()))
            .chevron(false)
            .selected(true);

            let row2 = ListRow::new(vec![
                Span::styled("~/.cargo/git/", theme.callout()),
                Span::styled("db", theme.body()),
            ])
            .leading(Span::styled("○", theme.callout()))
            .detail(Span::styled("25.0 MB", theme.body()))
            .chevron(false)
            .selected(false);

            let lines = vec![
                sec,
                row1.render_line(80, &theme),
                row2.render_line(80, &theme),
            ];
            f.render_widget(ratatui::widgets::Paragraph::new(lines), f.area());
        });
        assert_snapshot!(format!("list_{name}"), output);
    }
}

#[test]
fn snapshot_component_gauge() {
    for (name, theme) in [("dark", dark_theme()), ("nocolor", nocolor_theme())] {
        let output = render_to_string(80, 5, |f| {
            let b = bar(0.65, 40, theme.p.accent, &theme);
            let seg = segmented_bar(
                &[
                    (0.3, theme.p.accent),
                    (0.2, theme.p.purple),
                    (0.1, theme.p.teal),
                ],
                40,
                &theme,
            );
            let leg = legend(
                &[
                    ("Used", "370 GB".to_string(), theme.p.accent),
                    ("Purgeable", "5.7 GB".to_string(), theme.p.gray),
                    ("Free", "118 GB".to_string(), theme.p.green),
                ],
                &theme,
            );
            let lines = vec![Line::from(b), Line::from(seg), Line::from(""), leg];
            f.render_widget(ratatui::widgets::Paragraph::new(lines), f.area());
        });
        assert_snapshot!(format!("gauge_{name}"), output);
    }
}

#[test]
fn snapshot_component_modal() {
    for (name, theme) in [("dark", dark_theme()), ("nocolor", nocolor_theme())] {
        let output = render_to_string(70, 16, |f| {
            Modal::new("Remove 12 items?")
                .message(vec![
                    Line::from("3.4 GB will be permanently deleted. This"),
                    Line::from("cannot be undone."),
                ])
                .buttons(vec![
                    Button::new("Cancel").focused(true),
                    Button::destructive("Remove 12 items"),
                ])
                .render(f, f.area(), &theme);
        });
        assert_snapshot!(format!("modal_{name}"), output);
    }
}

#[test]
fn snapshot_component_empty() {
    for (name, theme) in [("dark", dark_theme()), ("nocolor", nocolor_theme())] {
        let output = render_to_string(70, 14, |f| {
            EmptyState::new("✓", "Nothing to clean", "Your caches are already tidy.")
                .glyph_color(theme.p.green)
                .render(f, f.area(), &theme);
        });
        assert_snapshot!(format!("empty_{name}"), output);
    }
}

#[test]
fn snapshot_component_progress() {
    for (name, theme) in [("dark", dark_theme()), ("nocolor", nocolor_theme())] {
        let output = render_to_string(80, 16, |f| {
            ProgressView::new("⠋", "Scanning developer caches")
                .step_info("Step 2 of 5 · Cargo")
                .fraction(Some(0.40))
                .current_path("~/Library/Caches/com.apple.dt.Xcode/Index")
                .counter_info("128 items found · 4.2 GB")
                .elapsed_seconds(12)
                .render(f, f.area(), &theme);
        });
        assert_snapshot!(format!("progress_component_{name}"), output);
    }
}

#[test]
fn snapshot_component_toast() {
    for (name, theme) in [("dark", dark_theme()), ("nocolor", nocolor_theme())] {
        let output = render_to_string(80, 5, |f| {
            let toast = Toast::success("Revealed in Finder");
            let chrome = AppChrome::new(vec!["taert", "Explore"]).toast(Some(&toast));
            chrome.render(f, &theme);
        });
        assert_snapshot!(format!("toast_{name}"), output);
    }
}

#[test]
fn snapshot_component_help() {
    for (name, theme) in [("dark", dark_theme()), ("nocolor", nocolor_theme())] {
        let output = render_to_string(70, 16, |f| {
            let sheet = HelpSheet::new(vec![
                Hint::new("↑↓", "Move"),
                Hint::new("␣", "Select"),
                Hint::new("A", "Select all"),
                Hint::new("N", "Select none"),
                Hint::new("/", "Search"),
                Hint::new("↵", "Review"),
            ]);
            sheet.render(f, f.area(), &theme);
        });
        assert_snapshot!(format!("help_{name}"), output);
    }
}
