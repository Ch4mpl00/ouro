// Rust-only: the skill overlay and boot validation (skills.ts has no test file
// of its own in packages/agent).
use agent::skills::*;
#[tokio::test]
async fn overlay_defaults_patch_listing_and_validation() {
    let root = tempfile::tempdir().unwrap();
    let defaults = root.path().join("skills.default");
    std::fs::create_dir(&defaults).unwrap();
    std::fs::write(defaults.join("domain.md"), "---\ntools: [echo]\n---\n# Default\n").unwrap();
    std::fs::write(defaults.join("meta.md"), "---\ntools: []\n---\n# Meta\n").unwrap();
    let store = FsSkillStore::new(root.path());
    assert!(store.read_skill("missing").await.unwrap().is_none());
    let original = store.read_skill("domain").await.unwrap().unwrap();
    assert_eq!(original.source, SkillSource::Default);
    assert_eq!(original.tools, SkillTools::Named(vec!["echo".into()]));
    let content = "---\r\ntools: *\r\n---\r\n# Live 🦊\r\n";
    let written = store.save_skill("domain", content).await.unwrap();
    assert_eq!(written.size_bytes, content.len());
    assert_eq!(store.read_skill("domain").await.unwrap().unwrap().source, SkillSource::Live);
    std::fs::write(root.path().join("skills/domain.patch.md"), "lesson").unwrap();
    assert_eq!(store.read_patch("domain").await.unwrap().as_deref(), Some("lesson"));
    let list = store.list_skills().await.unwrap();
    assert_eq!(list.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["domain", "meta"]);
    store.validate_all(&["echo".into()]).await.unwrap();
    std::fs::remove_file(written.path).unwrap();
    assert_eq!(store.read_skill("domain").await.unwrap().unwrap(), original);
    assert_eq!(std::fs::read_to_string(defaults.join("domain.md")).unwrap(), "---\ntools: [echo]\n---\n# Default\n");
    assert!(store.save_skill("domain", "no frontmatter").await.is_err());
    assert!(!root.path().join("skills/domain.md").exists());
}
#[tokio::test]
async fn names_cannot_escape_and_boot_reports_all_broken_skills() {
    let root = tempfile::tempdir().unwrap();
    let store = FsSkillStore::new(root.path());
    for name in ["../escape", "x.patch", "/tmp/x", "", "a/b"] {
        assert!(store.read_skill(name).await.is_err());
        assert!(store.save_skill(name, "---\ntools: []\n---\ntext\n").await.is_err());
    }
    std::fs::create_dir(root.path().join("skills")).unwrap();
    std::fs::write(root.path().join("skills/broken.md"), "no header").unwrap();
    std::fs::write(root.path().join("skills/unknown.md"), "---\ntools: [missing]\n---\ntext\n").unwrap();
    let error = store.validate_all(&["echo".into()]).await.unwrap_err().to_string();
    assert!(error.contains("2 issue(s)"));
    assert!(error.contains("broken"));
    assert!(error.contains("missing"));
    for raw in ["text", "---\ntitle: only\n---\ntext\n", "---\ntools: [Bad-Name]\n---\ntext\n"] {
        assert!(declared_tools("domain", raw).is_err());
    }
}
#[test]
fn patch_goes_at_end_and_blank_patch_is_noop() {
    assert_eq!(append_patch("body \n", "  lesson\n"), "body\n\n<!-- improver-patch -->\nlesson\n");
    assert_eq!(append_patch("body \n", " \n"), "body \n");
}
