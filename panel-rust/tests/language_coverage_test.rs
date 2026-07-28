//! language-switch-sync plan: a static drift check between Qt's language
//! menu (the real, authoritative list of languages a user can select in
//! Shotcut) and panel-rust's own translation catalogs.
//!
//! Qt's list lives in `shotcut/src/mainwindow.cpp`, hardcoded as one
//! `QAction`/`a->setData("<code>")` pair per language inside the
//! `m_languagesGroup` block -- there's no shared manifest file today, so
//! this test parses that block directly rather than trusting a second,
//! easily-stale copy of the list. If Qt gains a language and panel-rust's
//! `translations/` directory doesn't get a (possibly still-untranslated)
//! stub for it, this test fails loudly instead of silently falling back to
//! English forever for that locale.

use std::collections::BTreeSet;
use std::path::Path;

/// Extracts the `a->setData("<code>")` locale codes from `mainwindow.cpp`'s
/// `m_languagesGroup` construction block. Scoped between the block's start
/// (`m_languagesGroup = new QActionGroup`) and its end
/// (`ui->menuLanguage->addActions(...)`) so a `setData` call belonging to
/// some *other* unrelated `QActionGroup` elsewhere in this large file can
/// never be picked up by accident.
fn qt_language_codes(mainwindow_cpp: &str) -> Vec<String> {
    let start = mainwindow_cpp
        .find("m_languagesGroup = new QActionGroup")
        .expect("mainwindow.cpp: m_languagesGroup construction not found -- has this been renamed/restructured?");
    let end = mainwindow_cpp[start..]
        .find("ui->menuLanguage->addActions(m_languagesGroup->actions())")
        .map(|offset| start + offset)
        .expect("mainwindow.cpp: end-of-language-block marker not found -- has this been renamed/restructured?");
    let block = &mainwindow_cpp[start..end];

    let mut codes = Vec::new();
    let mut rest = block;
    while let Some(open) = rest.find("setData(\"") {
        let after_open = &rest[open + "setData(\"".len()..];
        let close = after_open
            .find('"')
            .expect("mainwindow.cpp: unterminated setData(\"...\" in language block");
        codes.push(after_open[..close].to_string());
        rest = &after_open[close..];
    }
    codes
}

fn translations_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("translations")
}

fn panel_rust_translated_codes() -> BTreeSet<String> {
    let dir = translations_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return BTreeSet::new();
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .filter(|entry| entry.path().join("LC_MESSAGES/panel-rust.po").is_file())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect()
}

#[test]
fn every_qt_selectable_language_has_a_panel_rust_translation_stub() {
    let mainwindow_cpp_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../shotcut/src/mainwindow.cpp");
    let Ok(mainwindow_cpp) = std::fs::read_to_string(&mainwindow_cpp_path) else {
        eprintln!(
            "language_coverage_test: skipping -- {} not found (shotcut submodule not \
             checked out in this checkout). This check only runs where both repos are present.",
            mainwindow_cpp_path.display()
        );
        return;
    };

    if !translations_dir().is_dir() {
        eprintln!(
            "language_coverage_test: skipping -- panel-rust/translations/ doesn't exist yet \
             (language-switch-sync plan phase 3/4 not done). This check activates once it does."
        );
        return;
    }

    let qt_codes: BTreeSet<String> = qt_language_codes(&mainwindow_cpp).into_iter().collect();
    assert!(
        qt_codes.len() >= 40,
        "expected ~41 languages parsed out of mainwindow.cpp's language block, got {} \
         ({qt_codes:?}) -- the parser's start/end markers or setData pattern may no longer \
         match the real source",
        qt_codes.len()
    );

    let panel_rust_codes = panel_rust_translated_codes();
    let missing: Vec<&String> = qt_codes.difference(&panel_rust_codes).collect();
    assert!(
        missing.is_empty(),
        "Qt's language menu (mainwindow.cpp) offers {missing:?} but panel-rust/translations/ \
         has no matching <code>/LC_MESSAGES/panel-rust.po for {} of them -- add a stub file \
         (even untranslated) so panel-rust doesn't silently stay English-only for a language \
         Shotcut itself lets the user pick",
        missing.len()
    );

    let extra: Vec<&String> = panel_rust_codes.difference(&qt_codes).collect();
    if !extra.is_empty() {
        eprintln!(
            "language_coverage_test: note -- panel-rust/translations/ has entries Qt's menu \
             doesn't offer: {extra:?} (harmless, just unreachable from the Qt language picker)"
        );
    }
}
