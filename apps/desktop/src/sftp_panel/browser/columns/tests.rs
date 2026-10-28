use super::*;

fn entry(
    name: &str,
    directory: bool,
    size: Option<u64>,
    modified: Option<u32>,
) -> SftpDirectoryEntry {
    SftpDirectoryEntry {
        name: name.into(),
        path: format!("/{name}"),
        kind: if directory { 2 } else { 1 },
        size,
        modified_unix_seconds: modified,
        ..Default::default()
    }
}
fn names<'a>(sort: SortState, entries: &'a [SftpDirectoryEntry]) -> Vec<&'a str> {
    sort.sorted(entries)
        .into_iter()
        .map(|entry| entry.name.as_str())
        .collect()
}

#[test]
fn local_defaults_to_newest_first_and_remote_defaults_to_folders_first() {
    let entries = [
        entry("old-dir", true, None, Some(1)),
        entry("z-new.txt", false, Some(10), Some(50)),
        entry("a-old.txt", false, Some(2), Some(2)),
        entry("unknown", false, None, None),
    ];
    assert_eq!(
        names(SortState::initial(true), &entries),
        ["z-new.txt", "a-old.txt", "old-dir", "unknown"]
    );
    assert_eq!(
        names(SortState::initial(false), &entries),
        ["old-dir", "a-old.txt", "unknown", "z-new.txt"]
    );
    let mut sort = SortState::initial(false);
    sort.select(Column::Size);
    assert_eq!(
        names(sort, &entries),
        ["old-dir", "a-old.txt", "z-new.txt", "unknown"]
    );
    sort.select(Column::Size);
    assert_eq!(
        names(sort, &entries),
        ["old-dir", "z-new.txt", "a-old.txt", "unknown"]
    );
    sort.folders_first = false;
    assert_eq!(
        names(sort, &entries),
        ["z-new.txt", "a-old.txt", "old-dir", "unknown"]
    );
}

#[test]
fn every_column_can_reverse_and_numeric_fields_use_raw_values_with_missing_last() {
    let mut entries = [
        entry("a.bin", false, Some(10), Some(20)),
        entry("z.txt", false, Some(2), Some(10)),
        entry("missing", false, None, None),
    ];
    entries[0].uid = Some(10);
    entries[1].uid = Some(2);
    entries[0].permissions = Some(0o100755);
    entries[1].permissions = Some(0o100644);
    for column in columns(false) {
        let mut sort = SortState {
            column: *column,
            ascending: true,
            folders_first: false,
        };
        let ascending = names(sort, &entries[..2]);
        sort.select(*column);
        let descending = names(sort, &entries[..2]);
        assert_eq!(
            ascending.into_iter().rev().collect::<Vec<_>>(),
            descending,
            "{column:?}"
        );
    }
    for column in [
        Column::Size,
        Column::Modified,
        Column::Permissions,
        Column::Owner,
    ] {
        for ascending in [true, false] {
            let sort = SortState {
                column,
                ascending,
                folders_first: false,
            };
            assert_eq!(names(sort, &entries)[2], "missing");
            assert_eq!(
                names(sort, &entries)[0],
                if ascending { "z.txt" } else { "a.bin" }
            );
        }
    }
}

#[test]
fn metadata_formatting_handles_special_modes_types_and_local_time() {
    let mut file = entry("archive.gz", false, Some(123), Some(0));
    assert_eq!(kind(&file), "GZ 文件");
    file.name = ".bashrc".into();
    assert_eq!(kind(&file), "文件");
    file.permissions = Some(0o104755);
    assert_eq!(permissions(&file), "-rwsr-xr-x");
    file.permissions = Some(0o104644);
    assert_eq!(permissions(&file), "-rwSr--r--");
    file.kind = 2;
    file.permissions = Some(0o041777);
    assert_eq!(permissions(&file), "drwxrwxrwt");
    assert_eq!(Column::Size.value(&file), "—");
    let text = modified(Some(0));
    assert_eq!(text.len(), 19);
    assert_eq!(&text[4..5], "-");
    assert_eq!(&text[10..11], " ");
    assert_eq!(modified(None), "—");
    assert_eq!(
        local_components(1).unwrap()[5],
        (local_components(0).unwrap()[5] + 1) % 60
    );
}
