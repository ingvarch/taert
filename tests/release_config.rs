//! Checks on the release configuration.

const GORELEASER: &str = include_str!("../.goreleaser.yaml");

#[test]
fn the_cask_has_no_uninstall_stanza() {
    assert!(
        GORELEASER.lines().all(|l| l.trim() != "uninstall:"),
        "Homebrew runs the uninstall stanza on every upgrade"
    );
    let after_zap: String = GORELEASER
        .lines()
        .skip_while(|l| l.trim() != "zap:")
        .collect();
    assert!(!after_zap.is_empty(), "the cask keeps a zap stanza");
    assert!(after_zap.contains("io.github.ingvarch.taert.schedule"));
}
