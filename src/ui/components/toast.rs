//! Toast notification component.

use std::time::Instant;

use ratatui::text::Span;

use crate::ui::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Success,
    Info,
    Error,
}

#[derive(Debug, Clone)]
pub struct Toast {
    pub kind: ToastKind,
    pub text: String,
    pub shown_at: Instant,
}

impl Toast {
    pub fn success(text: impl Into<String>) -> Self {
        Self {
            kind: ToastKind::Success,
            text: text.into(),
            shown_at: Instant::now(),
        }
    }

    pub fn info(text: impl Into<String>) -> Self {
        Self {
            kind: ToastKind::Info,
            text: text.into(),
            shown_at: Instant::now(),
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            kind: ToastKind::Error,
            text: text.into(),
            shown_at: Instant::now(),
        }
    }

    pub fn is_expired(&self) -> bool {
        self.shown_at.elapsed().as_secs() >= 3
    }

    pub fn to_span(&self, theme: &Theme) -> Span<'static> {
        let (glyph, style) = match self.kind {
            ToastKind::Success => (theme.g.success, theme.success()),
            ToastKind::Info => (theme.g.bullet, theme.accent_text()),
            ToastKind::Error => (theme.g.error, theme.danger()),
        };

        Span::styled(format!("{glyph}  {}", self.text), style)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_toast_span() {
        let theme = Theme::new();
        let toast = Toast::success("Saved");
        let span = toast.to_span(&theme);
        assert!(span.content.contains("Saved"));
        assert!(!toast.is_expired());
    }
}
