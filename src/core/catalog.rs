//! Target catalog for known caches and junk locations.

use std::path::{Path, PathBuf};

use super::safety::Safety;

#[derive(Debug, Clone)]
pub struct Target {
    pub tool: String,
    pub paths: Vec<PathBuf>,
    pub selected_by_default: bool,
    pub contents_only: bool,
}

impl Target {
    pub fn new(tool: impl Into<String>, paths: Vec<PathBuf>) -> Self {
        Self {
            tool: tool.into(),
            paths,
            selected_by_default: true,
            contents_only: false,
        }
    }

    pub fn unselected(tool: impl Into<String>, paths: Vec<PathBuf>) -> Self {
        Self {
            tool: tool.into(),
            paths,
            selected_by_default: false,
            contents_only: false,
        }
    }

    pub fn contents_only(tool: impl Into<String>, paths: Vec<PathBuf>) -> Self {
        Self {
            tool: tool.into(),
            paths,
            selected_by_default: true,
            contents_only: true,
        }
    }
}

pub struct Catalog;

impl Catalog {
    fn h(rel: &str) -> PathBuf {
        Safety::home_dir().join(rel)
    }

    pub fn dev() -> Vec<Target> {
        vec![
            Target::new("Homebrew", vec![Self::h("Library/Caches/Homebrew")]),
            Target::new("npm", vec![Self::h(".npm/_cacache"), Self::h(".npm/_logs")]),
            Target::new(
                "Yarn",
                vec![
                    Self::h("Library/Caches/Yarn"),
                    Self::h(".cache/yarn"),
                    Self::h(".yarn/berry/cache"),
                ],
            ),
            Target::new(
                "pnpm",
                vec![
                    Self::h("Library/pnpm/store"),
                    Self::h(".pnpm-store"),
                    Self::h(".cache/pnpm"),
                ],
            ),
            Target::new(
                "pip",
                vec![Self::h("Library/Caches/pip"), Self::h(".cache/pip")],
            ),
            Target::new(
                "Cargo (Rust)",
                vec![
                    Self::h(".cargo/registry/cache"),
                    Self::h(".cargo/registry/index"),
                    Self::h(".cargo/git/db"),
                    Self::h(".cargo/git/checkouts"),
                ],
            ),
            Target::new(
                "Go",
                vec![
                    Self::h("Library/Caches/go-build"),
                    Self::h("go/pkg/mod/cache"),
                ],
            ),
            Target::new("CocoaPods", vec![Self::h("Library/Caches/CocoaPods")]),
            Target::new(
                "VS Code",
                vec![
                    Self::h("Library/Application Support/Code/Cache"),
                    Self::h("Library/Application Support/Code/CachedData"),
                    Self::h("Library/Application Support/Code/CachedExtensionVSIXs"),
                    Self::h("Library/Application Support/Code/logs"),
                ],
            ),
            Target::new(
                "JetBrains",
                vec![
                    Self::h("Library/Caches/JetBrains"),
                    Self::h("Library/Logs/JetBrains"),
                ],
            ),
            Target::unselected("Maven (~/.m2 repository)", vec![Self::h(".m2/repository")]),
            Target::new(
                "Gradle",
                vec![
                    Self::h(".gradle/caches"),
                    Self::h(".gradle/daemon"),
                    Self::h(".gradle/wrapper/dists"),
                ],
            ),
            Target::new(
                "Poetry",
                vec![
                    Self::h("Library/Caches/pypoetry"),
                    Self::h(".cache/pypoetry"),
                ],
            ),
            Target::new(
                "uv",
                vec![Self::h(".cache/uv"), Self::h("Library/Caches/uv")],
            ),
            Target::new("Bun", vec![Self::h(".bun/install/cache")]),
            Target::new(
                "Deno",
                vec![Self::h("Library/Caches/deno"), Self::h(".cache/deno")],
            ),
            Target::new("mise", vec![Self::h(".cache/mise")]),
            Target::new("Flutter / Dart pub", vec![Self::h(".pub-cache")]),
            Target::unselected(
                "NuGet / .NET",
                vec![
                    Self::h(".nuget/packages"),
                    Self::h(".local/share/NuGet/http-cache"),
                ],
            ),
            Target::new(
                "Swift Package Manager",
                vec![Self::h("Library/Caches/org.swift.swiftpm")],
            ),
            Target::new(
                "Puppeteer (browser downloads)",
                vec![Self::h(".cache/puppeteer")],
            ),
            Target::new(
                "Playwright (browser downloads)",
                vec![
                    Self::h("Library/Caches/ms-playwright"),
                    Self::h(".cache/ms-playwright"),
                ],
            ),
            Target::new(
                "node-gyp (cached headers)",
                vec![
                    Self::h("Library/Caches/node-gyp"),
                    Self::h(".cache/node-gyp"),
                ],
            ),
            Target::new(
                "Electron (download cache)",
                vec![
                    Self::h("Library/Caches/electron"),
                    Self::h("Library/Caches/electron-builder"),
                ],
            ),
            Target::new(
                "Docker Desktop",
                vec![
                    Self::h("Library/Containers/com.docker.docker/Data/cache"),
                    Self::h("Library/Containers/com.docker.docker/Data/log"),
                    Self::h("Library/Containers/com.docker.docker/Data/tmp"),
                    Self::h("Library/Group Containers/group.com.docker/Caches"),
                    Self::h(".docker/cli-plugins/.cache"),
                    Self::h(".docker/buildx/cache"),
                ],
            ),
            Target::new(
                "OrbStack",
                vec![
                    Self::h(".orbstack/log"),
                    Self::h("Library/Caches/dev.kdrag0n.MacVirt"),
                    Self::h("Library/Logs/OrbStack"),
                ],
            ),
        ]
    }

    pub fn junk() -> Vec<Target> {
        vec![
            Target::contents_only("User logs", vec![Self::h("Library/Logs")]),
            Target::unselected(
                "Saved application state",
                vec![Self::h("Library/Saved Application State")],
            ),
            Target::new(
                "Xcode DerivedData",
                vec![Self::h("Library/Developer/Xcode/DerivedData")],
            ),
            Target::unselected(
                "Xcode Archives",
                vec![Self::h("Library/Developer/Xcode/Archives")],
            ),
            Target::new(
                "iOS DeviceSupport",
                vec![Self::h("Library/Developer/Xcode/iOS DeviceSupport")],
            ),
            Target::new(
                "watchOS DeviceSupport",
                vec![Self::h("Library/Developer/Xcode/watchOS DeviceSupport")],
            ),
            Target::new(
                "tvOS DeviceSupport",
                vec![Self::h("Library/Developer/Xcode/tvOS DeviceSupport")],
            ),
            Target::new(
                "CoreSimulator caches",
                vec![Self::h("Library/Developer/CoreSimulator/Caches")],
            ),
            Target::new(
                "Xcode app cache",
                vec![Self::h("Library/Caches/com.apple.dt.Xcode")],
            ),
            Target::new(
                "XCTestDevices",
                vec![Self::h("Library/Developer/XCTestDevices")],
            ),
            Target::new(
                "SwiftUI Previews",
                vec![Self::h("Library/Developer/Xcode/UserData/Previews")],
            ),
        ]
    }

    pub fn ai() -> Vec<Target> {
        vec![
            Target::new(
                "Ollama (logs/cache)",
                vec![
                    Self::h(".ollama/logs"),
                    Self::h("Library/Caches/ollama"),
                    Self::h("Library/Caches/com.electron.ollama"),
                ],
            ),
            Target::new("LM Studio (logs)", vec![Self::h(".lmstudio/server-logs")]),
            Target::new(
                "Cursor (cache)",
                vec![
                    Self::h("Library/Application Support/Cursor/Cache"),
                    Self::h("Library/Application Support/Cursor/CachedData"),
                    Self::h("Library/Application Support/Cursor/logs"),
                ],
            ),
            Target::unselected(
                "Hugging Face (~/.cache/huggingface)",
                vec![
                    Self::h(".cache/huggingface/hub"),
                    Self::h(".cache/huggingface/datasets"),
                    Self::h(".cache/huggingface/xet"),
                ],
            ),
        ]
    }

    pub fn trash() -> Vec<Target> {
        let mut paths = vec![Self::h(".Trash")];

        // Also discover mounted volume trashes: /Volumes/*/.Trashes/<uid>
        let uid = unsafe { libc::getuid() };
        if let Ok(entries) = std::fs::read_dir("/Volumes") {
            for entry in entries.flatten() {
                let volume_trash = entry.path().join(".Trashes").join(uid.to_string());
                if volume_trash.exists() {
                    paths.push(volume_trash);
                }
            }
        }

        vec![Target::contents_only("Trash", paths)]
    }

    /// Every path a `dev`, `junk`, or `ai` target covers.
    pub fn named_paths() -> Vec<PathBuf> {
        [Self::dev(), Self::junk(), Self::ai()]
            .into_iter()
            .flatten()
            .flat_map(|target| target.paths)
            .collect()
    }

    /// The same paths with `home` in place of the real home folder.
    pub fn named_paths_in(home: &Path) -> Vec<PathBuf> {
        let real = Safety::home_dir();
        Self::named_paths()
            .into_iter()
            .map(|path| match path.strip_prefix(&real) {
                Ok(rest) => home.join(rest),
                Err(_) => path,
            })
            .collect()
    }

    pub fn targets_for(category: &str) -> Vec<Target> {
        match category {
            "dev" => Self::dev(),
            "junk" => Self::junk(),
            "ai" => Self::ai(),
            "trash" => Self::trash(),
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_catalog_targets_exist_or_safe() {
        let targets = Catalog::targets_for("dev");
        assert!(!targets.is_empty());
        for t in &targets {
            assert!(!t.paths.is_empty());
        }
    }

    #[test]
    fn named_paths_hold_the_paths_of_dev_junk_and_ai() {
        let named = Catalog::named_paths();
        for path in [
            "Library/Caches/Homebrew",
            "Library/Logs",
            "Library/Caches/ollama",
        ] {
            assert!(named.contains(&Catalog::h(path)), "{path}");
        }
        assert!(!named.contains(&Catalog::h(".Trash")));
    }
}
