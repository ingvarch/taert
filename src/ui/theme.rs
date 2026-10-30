//! TUI Color Theme and Styling.

use std::env;

use ratatui::style::{Color, Modifier, Style};

pub struct Theme {
    pub border: Style,
    pub title: Style,
    pub subtitle: Style,
    pub info: Style,
    pub accent: Style,
    pub normal: Style,
    pub muted: Style,
    pub selected_marker: Style,
    pub unselected_marker: Style,
    pub cursor: Style,
    pub focused_row: Style,
    pub footer_key: Style,
    pub footer_desc: Style,
    pub warning: Style,
    pub error: Style,
    pub success: Style,
}

impl Default for Theme {
    fn default() -> Self {
        Self::new()
    }
}

impl Theme {
    pub fn new() -> Self {
        let no_color =
            env::var("NO_COLOR").is_ok() || env::var("TERM").map(|t| t == "dumb").unwrap_or(false);

        if no_color {
            return Self::plain();
        }

        Self {
            border: Style::default().fg(Color::Rgb(90, 95, 105)),
            title: Style::default()
                .fg(Color::Rgb(0, 210, 235)) // Cyan accent
                .add_modifier(Modifier::BOLD),
            subtitle: Style::default()
                .fg(Color::Rgb(0, 210, 235))
                .add_modifier(Modifier::BOLD),
            info: Style::default()
                .fg(Color::Rgb(220, 225, 230))
                .add_modifier(Modifier::BOLD),
            accent: Style::default().fg(Color::Rgb(0, 210, 235)),
            normal: Style::default().fg(Color::Rgb(215, 220, 225)),
            muted: Style::default().fg(Color::Rgb(125, 130, 140)),
            selected_marker: Style::default().fg(Color::Rgb(0, 210, 235)),
            unselected_marker: Style::default().fg(Color::Rgb(110, 115, 125)),
            cursor: Style::default()
                .fg(Color::Rgb(0, 210, 235))
                .add_modifier(Modifier::BOLD),
            focused_row: Style::default()
                .bg(Color::Rgb(215, 220, 225)) // Bright selection highlight
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
            footer_key: Style::default()
                .fg(Color::Rgb(200, 205, 210))
                .add_modifier(Modifier::BOLD),
            footer_desc: Style::default().fg(Color::Rgb(120, 125, 135)),
            warning: Style::default()
                .fg(Color::Rgb(255, 180, 50))
                .add_modifier(Modifier::BOLD),
            error: Style::default()
                .fg(Color::Rgb(255, 80, 80))
                .add_modifier(Modifier::BOLD),
            success: Style::default()
                .fg(Color::Rgb(80, 220, 120))
                .add_modifier(Modifier::BOLD),
        }
    }

    pub fn plain() -> Self {
        Self {
            border: Style::default(),
            title: Style::default().add_modifier(Modifier::BOLD),
            subtitle: Style::default().add_modifier(Modifier::BOLD),
            info: Style::default().add_modifier(Modifier::BOLD),
            accent: Style::default(),
            normal: Style::default(),
            muted: Style::default(),
            selected_marker: Style::default(),
            unselected_marker: Style::default(),
            cursor: Style::default().add_modifier(Modifier::BOLD),
            focused_row: Style::default().add_modifier(Modifier::REVERSED),
            footer_key: Style::default().add_modifier(Modifier::BOLD),
            footer_desc: Style::default(),
            warning: Style::default().add_modifier(Modifier::BOLD),
            error: Style::default().add_modifier(Modifier::BOLD),
            success: Style::default().add_modifier(Modifier::BOLD),
        }
    }
}
