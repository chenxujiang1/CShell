use super::*;
use cshell_domain::TerminalPalette;
use std::error::Error;

fn theme(name: &str, color: u8) -> ThemeRecord {
    ThemeRecord {
        name: name.into(),
        palette: TerminalPalette {
            foreground: [color, 2, 3],
            background: [4, 5, 6],
            cursor: [7, 8, 9],
            selection: [10, 11, 12],
            ansi: [[13, 14, 15]; 16],
        },
    }
}

fn document(themes: Vec<ThemeRecord>) -> ThemeDocument {
    ThemeDocument {
        format: THEME_FORMAT.into(),
        version: THEME_VERSION,
        themes,
    }
}

#[test]
fn theme_contract_rejects_hostile_shapes_names_duplicates_and_limits() -> Result<(), Box<dyn Error>>
{
    let value = serde_json::to_value(document(vec![theme("paper", 1)]))?;
    let mut invalid = value.clone();
    invalid["themes"][0]["palette"]["foreground"] = serde_json::json!([256, 0, 0]);
    assert!(ThemeDocument::parse(&serde_json::to_vec(&invalid)?).is_err());
    invalid["themes"][0]["palette"]["foreground"] = serde_json::json!([1.5, 0, 0]);
    assert!(ThemeDocument::parse(&serde_json::to_vec(&invalid)?).is_err());
    for name in [
        "dark",
        "light",
        "default",
        "Dark",
        "../escape",
        " spaced",
        "",
        "a/b",
        "A",
    ] {
        assert!(
            ThemeDocument::parse(&serde_json::to_vec(&document(vec![theme(name, 1)]))?).is_err()
        );
    }
    invalid = value.clone();
    invalid["themes"][0]["palette"]["ansi"] = serde_json::json!([[0, 0, 0]]);
    assert!(ThemeDocument::parse(&serde_json::to_vec(&invalid)?).is_err());
    invalid = value.clone();
    invalid["themes"][0]["command"] = serde_json::json!("SECRET_SENTINEL");
    let error = ThemeDocument::parse(&serde_json::to_vec(&invalid)?)
        .err()
        .ok_or("invalid field")?;
    assert!(!error.to_string().contains("SECRET_SENTINEL"));
    assert!(!format!("{error:?}").contains("SECRET_SENTINEL"));
    invalid = value;
    invalid["version"] = serde_json::json!(2);
    assert!(matches!(
        ThemeDocument::parse(&serde_json::to_vec(&invalid)?),
        Err(ThemeTransferError::UnsupportedFormat)
    ));
    assert!(
        ThemeDocument::parse(&serde_json::to_vec(&document(vec![
            theme("paper", 1),
            theme("paper", 2)
        ]))?)
        .is_err()
    );
    let many = (0..=MAX_THEMES)
        .map(|index| theme(&format!("theme-{index}"), 1))
        .collect();
    assert!(ThemeDocument::parse(&serde_json::to_vec(&document(many))?).is_err());
    assert!(matches!(
        ThemeDocument::parse(&vec![b' '; MAX_THEME_BYTES + 1]),
        Err(ThemeTransferError::TooLarge)
    ));
    Ok(())
}

#[test]
fn theme_conflicts_preview_actual_retained_palette_and_roundtrip() -> Result<(), Box<dyn Error>> {
    let original = theme("paper", 1);
    let replacement = theme("paper", 20);
    let mut catalog = ProfileCatalog::from_snapshot(CatalogSnapshot {
        themes: vec![original.clone()],
        ..Default::default()
    })?;
    let input = document(vec![replacement.clone(), theme("terminal-green", 30)]);
    let (_, fail) = plan(&catalog, &input, ImportConflictPolicy::Fail)?;
    assert!(!fail.can_commit);
    let (changes, skip) = plan(&catalog, &input, ImportConflictPolicy::Skip)?;
    assert_eq!(skip.change_count, 1);
    assert_eq!(skip.items[0].theme, Some(original.clone()));
    catalog.apply_batch(0, &changes)?;
    let (changes, replace) = plan(&catalog, &input, ImportConflictPolicy::Replace)?;
    assert_eq!(replace.items[0].theme, Some(replacement));
    catalog.apply_batch(1, &changes)?;
    let bytes = export_themes(&catalog.snapshot())?;
    let exported = ThemeDocument::parse(&bytes)?;
    let mut destination = ProfileCatalog::from_snapshot(CatalogSnapshot::default())?;
    let (changes, _) = plan(&destination, &exported, ImportConflictPolicy::Fail)?;
    destination.apply_batch(0, &changes)?;
    assert_eq!(destination.snapshot().themes, catalog.snapshot().themes);
    Ok(())
}

#[test]
fn merged_catalog_limit_rejects_entire_theme_batch() -> Result<(), Box<dyn Error>> {
    let catalog = ProfileCatalog::from_snapshot(CatalogSnapshot {
        themes: (0..MAX_THEMES)
            .map(|i| theme(&format!("theme-{i}"), 1))
            .collect(),
        ..Default::default()
    })?;
    assert!(
        plan(
            &catalog,
            &document(vec![theme("overflow", 1)]),
            ImportConflictPolicy::Fail
        )
        .is_err()
    );
    assert_eq!(catalog.snapshot().revision, 0);
    Ok(())
}

#[test]
fn published_sample_is_importable_and_exports_the_same_palette() -> Result<(), Box<dyn Error>> {
    let sample = ThemeDocument::parse(include_bytes!(
        "../../../../docs/examples/terminal-theme-paper.json"
    ))?;
    let mut catalog = ProfileCatalog::from_snapshot(CatalogSnapshot::default())?;
    let (changes, preview) = plan(&catalog, &sample, ImportConflictPolicy::Fail)?;
    assert!(preview.can_commit);
    catalog.apply_batch(0, &changes)?;
    assert_eq!(
        ThemeDocument::parse(&export_themes(&catalog.snapshot())?)?,
        sample
    );
    Ok(())
}
