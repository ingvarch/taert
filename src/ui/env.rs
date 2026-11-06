//! Environment detection for Terminal UI (depth, appearance, glyphs, reduce motion).

use crate::core::stores::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appearance {
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorDepth {
    TrueColor,
    Ansi256,
    Ansi16,
    NoColor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UiEnv {
    pub appearance: Appearance,
    /// Set when `TAERT_APPEARANCE` decided `appearance`.
    pub forced_appearance: Option<Appearance>,
    pub depth: ColorDepth,
    pub ascii_glyphs: bool,
    pub reduce_motion: bool,
    /// Inside tmux or screen.
    pub multiplexer: bool,
}

impl Default for UiEnv {
    fn default() -> Self {
        Self {
            appearance: Appearance::Dark,
            forced_appearance: None,
            depth: ColorDepth::TrueColor,
            ascii_glyphs: false,
            reduce_motion: false,
            multiplexer: false,
        }
    }
}

impl UiEnv {
    pub fn detect(config: &Config) -> Self {
        Self::detect_from(&|k| std::env::var(k).ok(), config)
    }

    pub fn detect_from(vars: &dyn Fn(&str) -> Option<String>, config: &Config) -> Self {
        let no_color = vars("NO_COLOR").is_some();
        let term = vars("TERM").unwrap_or_default();
        let colorterm = vars("COLORTERM").unwrap_or_default();

        let depth = if no_color || term == "dumb" {
            ColorDepth::NoColor
        } else if colorterm == "truecolor" || colorterm == "24bit" {
            ColorDepth::TrueColor
        } else if term.contains("256color") {
            ColorDepth::Ansi256
        } else {
            ColorDepth::Ansi16
        };

        let forced_appearance =
            vars("TAERT_APPEARANCE").map(|app| match app.to_lowercase().as_str() {
                "light" => Appearance::Light,
                _ => Appearance::Dark,
            });
        let appearance = if let Some(forced) = forced_appearance {
            forced
        } else {
            match config.appearance.to_lowercase().as_str() {
                "light" => Appearance::Light,
                "dark" => Appearance::Dark,
                _ => {
                    if let Some(fgbg) = vars("COLORFGBG") {
                        let last = fgbg.rsplit(';').next().unwrap_or("");
                        match last.trim().parse::<u8>() {
                            Ok(bg) if (0..=6).contains(&bg) || bg == 8 => Appearance::Dark,
                            Ok(bg) if bg == 7 || (9..=15).contains(&bg) => Appearance::Light,
                            _ => Appearance::Dark,
                        }
                    } else {
                        Appearance::Dark
                    }
                }
            }
        };

        let ascii_glyphs = vars("TAERT_ASCII").map(|v| v == "1").unwrap_or(false);
        let reduce_motion = vars("TAERT_REDUCE_MOTION")
            .map(|v| v == "1")
            .unwrap_or(false);

        let multiplexer = ["TMUX", "STY"]
            .into_iter()
            .any(|name| vars(name).is_some_and(|value| !value.is_empty()));

        Self {
            appearance,
            forced_appearance,
            depth,
            ascii_glyphs,
            reduce_motion,
            multiplexer,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmux_or_screen_is_told_by_the_variables_passed_in() {
        let detect = |name: &'static str, value: &'static str| {
            UiEnv::detect_from(
                &|k| (k == name).then(|| value.to_string()),
                &Config::default(),
            )
            .multiplexer
        };

        assert!(detect("TMUX", "/tmp/tmux-501/default,1,0"));
        assert!(detect("STY", "1234.pts-0"));
        assert!(!detect("TMUX", ""));
        assert!(!detect("TERM", "xterm"));
    }

    #[test]
    fn test_depth_detection() {
        let cfg = Config::default();

        // NO_COLOR precedence
        let env = UiEnv::detect_from(
            &|k| match k {
                "NO_COLOR" => Some("1".to_string()),
                "COLORTERM" => Some("truecolor".to_string()),
                _ => None,
            },
            &cfg,
        );
        assert_eq!(env.depth, ColorDepth::NoColor);

        // dumb terminal
        let env = UiEnv::detect_from(&|k| (k == "TERM").then(|| "dumb".to_string()), &cfg);
        assert_eq!(env.depth, ColorDepth::NoColor);

        // truecolor
        let env = UiEnv::detect_from(
            &|k| (k == "COLORTERM").then(|| "truecolor".to_string()),
            &cfg,
        );
        assert_eq!(env.depth, ColorDepth::TrueColor);

        // 256color
        let env = UiEnv::detect_from(
            &|k| (k == "TERM").then(|| "xterm-256color".to_string()),
            &cfg,
        );
        assert_eq!(env.depth, ColorDepth::Ansi256);

        // default ANSI16
        let env = UiEnv::detect_from(&|_| None, &cfg);
        assert_eq!(env.depth, ColorDepth::Ansi16);
    }

    #[test]
    fn test_appearance_detection() {
        let mut cfg = Config::default();

        // env override
        let env = UiEnv::detect_from(
            &|k| (k == "TAERT_APPEARANCE").then(|| "light".to_string()),
            &cfg,
        );
        assert_eq!(env.appearance, Appearance::Light);

        // config override
        cfg.appearance = "light".to_string();
        let env = UiEnv::detect_from(&|_| None, &cfg);
        assert_eq!(env.appearance, Appearance::Light);

        // COLORFGBG auto
        cfg.appearance = "auto".to_string();
        let env = UiEnv::detect_from(&|k| (k == "COLORFGBG").then(|| "0;15".to_string()), &cfg);
        assert_eq!(env.appearance, Appearance::Light);

        let env = UiEnv::detect_from(&|k| (k == "COLORFGBG").then(|| "15;0".to_string()), &cfg);
        assert_eq!(env.appearance, Appearance::Dark);
    }

    #[test]
    fn the_variable_is_reported_only_when_it_decides_the_appearance() {
        let cfg = Config {
            appearance: "dark".to_string(),
            ..Config::default()
        };
        let env = UiEnv::detect_from(
            &|k| (k == "TAERT_APPEARANCE").then(|| "light".to_string()),
            &cfg,
        );
        assert_eq!(env.appearance, Appearance::Light);
        assert_eq!(env.forced_appearance, Some(Appearance::Light));

        let env = UiEnv::detect_from(&|_| None, &cfg);
        assert_eq!(env.forced_appearance, None);
    }

    #[test]
    fn test_flags_detection() {
        let cfg = Config::default();
        let env = UiEnv::detect_from(
            &|k| match k {
                "TAERT_ASCII" => Some("1".to_string()),
                "TAERT_REDUCE_MOTION" => Some("1".to_string()),
                _ => None,
            },
            &cfg,
        );
        assert!(env.ascii_glyphs);
        assert!(env.reduce_motion);
    }
}
