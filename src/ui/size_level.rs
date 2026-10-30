//! SizeLevel color coding according to size magnitude:
//! < 100 MB -> Green
//! 100 MB ..< 1 GB -> Yellow
//! >= 1 GB -> Red

use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeLevel {
    Small,  // < 100 MB
    Medium, // 100 MB ..< 1 GB
    Large,  // >= 1 GB
}

impl SizeLevel {
    pub fn of(bytes: u64) -> Self {
        if bytes < 100_000_000 {
            Self::Small
        } else if bytes < 1_000_000_000 {
            Self::Medium
        } else {
            Self::Large
        }
    }

    pub fn color(&self) -> Color {
        match self {
            Self::Small => Color::Rgb(70, 200, 110),  // Green
            Self::Medium => Color::Rgb(240, 200, 70), // Yellow
            Self::Large => Color::Rgb(250, 80, 100),  // Red
        }
    }

    pub fn style(&self, is_focused: bool) -> Style {
        if is_focused {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Rgb(210, 215, 220))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(self.color())
                .add_modifier(Modifier::BOLD)
        }
    }
}
