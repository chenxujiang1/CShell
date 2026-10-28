use super::*;
use std::error::Error;

fn profiles() -> CatalogSnapshot {
    let id = ProfileId::new();
    CatalogSnapshot {
        revision: 7,
        profiles: vec![ProfileRecord {
            id,
            name: "SSH template".into(),
            kind: ProfileKind::Ssh,
            folder_id: None,
            tags: BTreeSet::new(),
            favorite: false,
            terminal: TerminalOverrides::default(),
        }],
        ssh_connections: vec![SshConnectionRecord {
            options: Default::default(),
            profile_id: id,
            host: "template.test".into(),
            port: 22,
            username: "operator".into(),
            auth_method: SshAuthMethod::Password,
            private_key_path: None,
            certificate_path: None,
            agent_backend: SshAgentBackend::OpenSsh,
            agent_identity: None,
            route: SshRoute::Direct,
        }],
        ..Default::default()
    }
}
fn fixture(count: usize) -> (HostCatalogSnapshot, CatalogSnapshot) {
    let profiles = profiles();
    let id = profiles.profiles[0].id;
    let hosts = (0..count)
        .map(|index| HostRecord {
            id: HostId::new(),
            name: format!("host-{index}"),
            address: format!("host-{index}.test"),
            port: 22,
            profile_id: id,
            tags: BTreeSet::from([if index % 2 == 0 { "web" } else { "db" }.into()]),
            labels: BTreeMap::from([(
                "env".into(),
                if index % 3 == 0 { "prod" } else { "stage" }.into(),
            )]),
            platform_hint: HostPlatformHint::Linux,
        })
        .collect();
    (
        HostCatalogSnapshot {
            revision: 13,
            catalog: HostCatalog {
                hosts,
                ..Default::default()
            },
        },
        profiles,
    )
}
fn selection(query: &str) -> HostSelection {
    HostSelection::Selector {
        expression: query.into(),
    }
}

#[test]
fn selector_precedence_quotes_unicode_labels_and_exact_ids_are_deterministic()
-> Result<(), Box<dyn Error>> {
    let (mut hosts, _) = fixture(6);
    hosts.catalog.hosts[0].name = "\u{6570}\u{636e} \"primary\"".into();
    let first = hosts.catalog.hosts[0].id;
    let ids: BTreeSet<_> = select_hosts(
        &hosts.catalog,
        &selection("tag:web OR tag:db AND env:prod"),
        10,
    )?
    .iter()
    .map(|host| host.id)
    .collect();
    let expected = hosts
        .catalog
        .hosts
        .iter()
        .filter(|host| {
            host.tags.contains("web")
                || host.tags.contains("db") && host.labels.get("env") == Some(&"prod".into())
        })
        .map(|host| host.id)
        .collect();
    assert_eq!(ids, expected);
    assert_eq!(
        select_hosts(
            &hosts.catalog,
            &selection("(tag:web || tag:db) && NOT env:stage"),
            10
        )?
        .len(),
        2
    );
    assert_eq!(
        select_hosts(
            &hosts.catalog,
            &selection("name:\"\u{6570}\u{636e} \\\"primary\\\"\""),
            10
        )?[0]
            .id,
        first
    );
    assert_eq!(
        select_hosts(
            &hosts.catalog,
            &selection(&format!(
                "host:{first} AND label:env=prod AND platform:linux"
            )),
            10
        )?[0]
            .id,
        first
    );
    assert_eq!(
        select_hosts(&hosts.catalog, &selection("address:host-1.test"), 10)?.len(),
        1
    );
    assert!(select_hosts(&hosts.catalog, &selection("none"), 10)?.is_empty());
    let key = "k".repeat(64);
    let value = "v".repeat(256);
    hosts.catalog.hosts[0]
        .labels
        .insert(key.clone(), value.clone());
    assert_eq!(
        select_hosts(
            &hosts.catalog,
            &selection(&format!("label:{key}={value}")),
            10
        )?[0]
            .id,
        first
    );
    assert_eq!(
        select_hosts(&hosts.catalog, &selection("all"), 1),
        Err(HostCatalogError::TooManyTargets)
    );
    Ok(())
}

#[test]
fn hostile_or_unsupported_selectors_fail_without_expanding_targets_or_echoing_query()
-> Result<(), Box<dyn Error>> {
    for query in [
        "",
        "tag:",
        "tag:web db",
        "tag:web & all",
        "all OR",
        "()",
        "platform:bsd",
        "host:broken",
        "unknown:PRIVATE_QUERY",
        "tag:\"unterminated",
        "tag:\"bad\\nquote\"",
    ] {
        assert!(
            HostSelector::parse(query).is_err(),
            "unexpected valid selector"
        );
    }
    let failure = HostSelector::parse("unknown:PRIVATE_QUERY")
        .err()
        .ok_or("expected selector error")?;
    assert_eq!(failure.kind, HostSelectorErrorKind::UnknownField);
    assert!(!failure.to_string().contains("PRIVATE_QUERY"));
    let parsed = HostSelector::parse("tag:PRIVATE_QUERY")?;
    assert!(!format!("{parsed:?}").contains("PRIVATE_QUERY"));
    assert!(HostSelector::parse(&"a".repeat(MAX_HOST_SELECTOR_BYTES + 1)).is_err());
    assert!(HostSelector::parse(&format!("{}all{}", "(".repeat(34), ")".repeat(34))).is_err());
    assert!(HostSelector::parse(&vec!["all"; 100].join(" OR ")).is_err());
    let (hosts, _) = fixture(1);
    assert_eq!(
        select_hosts(
            &hosts.catalog,
            &selection(&format!("all OR group:{}", HostGroupId::new())),
            10
        ),
        Err(HostCatalogError::UnknownGroup)
    );
    Ok(())
}

#[test]
fn static_dynamic_groups_deduplicate_and_parent_organization_does_not_expand_membership()
-> Result<(), Box<dyn Error>> {
    let (mut hosts, _) = fixture(4);
    let parent = HostGroupId::new();
    let child = HostGroupId::new();
    let dynamic = HostGroupId::new();
    hosts.catalog.groups = vec![
        HostGroupRecord {
            id: parent,
            name: "parent".into(),
            parent_id: None,
            membership: HostGroupMembership::Static {
                hosts: BTreeSet::from([hosts.catalog.hosts[0].id]),
            },
        },
        HostGroupRecord {
            id: child,
            name: "child".into(),
            parent_id: Some(parent),
            membership: HostGroupMembership::Static {
                hosts: BTreeSet::from([hosts.catalog.hosts[1].id]),
            },
        },
        HostGroupRecord {
            id: dynamic,
            name: "combined".into(),
            parent_id: None,
            membership: HostGroupMembership::Dynamic {
                selector: format!("group:{parent} OR group:{child} OR group:{parent}"),
            },
        },
    ];
    assert_eq!(
        select_hosts(&hosts.catalog, &HostSelection::Group { id: parent }, 10)?.len(),
        1
    );
    assert_eq!(
        select_hosts(&hosts.catalog, &HostSelection::Group { id: dynamic }, 10)?.len(),
        2
    );
    assert_eq!(
        apply_host_change(&hosts.catalog, HostCatalogChange::RemoveGroup(parent)),
        Err(HostCatalogError::UnknownGroup)
    );
    assert_eq!(
        apply_host_change(
            &hosts.catalog,
            HostCatalogChange::RemoveHost(hosts.catalog.hosts[0].id)
        ),
        Err(HostCatalogError::UnknownHost)
    );
    assert_eq!(hosts.catalog.groups.len(), 3);
    let cycle = HostGroupRecord {
        membership: HostGroupMembership::Dynamic {
            selector: format!("group:{dynamic}"),
        },
        ..hosts.catalog.groups[0].clone()
    };
    assert_eq!(
        apply_host_change(&hosts.catalog, HostCatalogChange::UpsertGroup(cycle)),
        Err(HostCatalogError::Cycle)
    );
    let ancestry = HostGroupRecord {
        parent_id: Some(child),
        ..hosts.catalog.groups[0].clone()
    };
    assert_eq!(
        apply_host_change(&hosts.catalog, HostCatalogChange::UpsertGroup(ancestry)),
        Err(HostCatalogError::Cycle)
    );
    Ok(())
}

#[test]
fn group_dependency_depth_is_checked_even_when_leaf_groups_are_visited_first() {
    let (mut hosts, _) = fixture(1);
    let mut dependency = None;
    for index in 0..35 {
        let id = HostGroupId::new();
        hosts.catalog.groups.push(HostGroupRecord {
            id,
            name: format!("group-{index}"),
            parent_id: None,
            membership: HostGroupMembership::Dynamic {
                selector: dependency
                    .map_or_else(|| "all".into(), |parent| format!("group:{parent}")),
            },
        });
        dependency = Some(id);
    }
    assert_eq!(validate_hosts(&hosts.catalog), Err(HostCatalogError::Limit));
}

#[test]
fn thousand_target_snapshot_survives_metadata_profile_edits_and_roundtrip()
-> Result<(), Box<dyn Error>> {
    let (mut hosts, mut profiles) = fixture(1_000);
    let frozen = freeze_hosts(&hosts, &profiles, selection("all"), MAX_HOST_TARGETS)?;
    assert_eq!(frozen.targets().len(), 1_000);
    assert_eq!(frozen.connections().len(), 1);
    assert_eq!(frozen.host_revision(), 13);
    assert_eq!(frozen.profile_revision(), 7);
    let id = frozen.targets()[0].id;
    let old_host = frozen.targets()[0].clone();
    hosts.catalog.hosts.clear();
    profiles.ssh_connections[0].username = "changed".into();
    assert_eq!(frozen.targets()[0], old_host);
    assert_eq!(
        frozen
            .resolved_connection(id)
            .ok_or("missing frozen connection")?
            .username,
        "operator"
    );
    assert_eq!(
        frozen
            .resolved_connection(id)
            .ok_or("missing frozen endpoint")?
            .host,
        old_host.address
    );
    assert_eq!(decode_frozen_hosts(&encode_frozen_hosts(&frozen)?)?, frozen);
    let (hosts, profiles) = fixture(1_001);
    assert_eq!(
        freeze_hosts(&hosts, &profiles, selection("all"), MAX_HOST_TARGETS),
        Err(HostCatalogError::TooManyTargets)
    );
    assert_eq!(
        freeze_hosts(&hosts, &profiles, selection("none"), MAX_HOST_TARGETS),
        Err(HostCatalogError::EmptyTargets)
    );
    Ok(())
}

#[test]
fn freeze_includes_original_credential_template_and_jump_dependency_but_no_other_profiles()
-> Result<(), Box<dyn Error>> {
    let (hosts, mut catalog) = fixture(1);
    let mut jump = profiles();
    let mut unrelated = profiles();
    jump.profiles[0].name = "jump".into();
    unrelated.profiles[0].name = "unrelated".into();
    catalog.ssh_connections[0].route = SshRoute::Jump {
        profile_id: jump.profiles[0].id,
    };
    catalog.profiles.extend(jump.profiles);
    catalog.ssh_connections.extend(jump.ssh_connections);
    catalog.profiles.extend(unrelated.profiles);
    catalog.ssh_connections.extend(unrelated.ssh_connections);
    let frozen = freeze_hosts(&hosts, &catalog, selection("all"), 10)?;
    assert_eq!(frozen.connections().len(), 2);
    let source = frozen
        .connections()
        .iter()
        .find(|connection| connection.profile_id == hosts.catalog.hosts[0].profile_id)
        .ok_or("missing template")?;
    assert_eq!(source.host, "template.test");
    assert_eq!(
        frozen.credential_template(hosts.catalog.hosts[0].id),
        Some(source)
    );
    assert_eq!(
        frozen
            .resolved_connection(hosts.catalog.hosts[0].id)
            .ok_or("missing resolved endpoint")?
            .host,
        hosts.catalog.hosts[0].address
    );
    let mut data: serde_json::Value = serde_json::from_slice(&encode_frozen_hosts(&frozen)?)?;
    data["connections"] = serde_json::json!([]);
    assert!(decode_frozen_hosts(&serde_json::to_vec(&data)?).is_err());
    Ok(())
}

#[test]
fn invalid_catalog_limits_duplicate_endpoints_and_dangling_profiles_are_rejected()
-> Result<(), Box<dyn Error>> {
    let (mut hosts, profiles) = fixture(1);
    assert_eq!(decode_hosts(&encode_hosts(&hosts.catalog)?)?, hosts.catalog);
    let mut duplicate = hosts.catalog.hosts[0].clone();
    duplicate.id = HostId::new();
    hosts.catalog.hosts.push(duplicate);
    assert_eq!(
        validate_hosts(&hosts.catalog),
        Err(HostCatalogError::Invalid)
    );
    hosts.catalog.hosts.pop();
    hosts.catalog.hosts[0]
        .labels
        .insert("unqueryable=key".into(), "value".into());
    assert_eq!(
        validate_hosts(&hosts.catalog),
        Err(HostCatalogError::Invalid)
    );
    hosts.catalog.hosts[0].labels.remove("unqueryable=key");
    hosts.catalog.hosts[0].profile_id = ProfileId::new();
    assert_eq!(
        validate_host_profiles(&hosts.catalog, &profiles),
        Err(HostCatalogError::MissingProfile)
    );
    assert_eq!(
        freeze_hosts(&hosts, &profiles, selection("all"), 10),
        Err(HostCatalogError::MissingProfile)
    );
    assert_eq!(
        decode_hosts(&vec![0; MAX_HOST_DOCUMENT_BYTES + 1]),
        Err(HostCatalogError::Limit)
    );
    let (mut large, _) = fixture(1_000);
    for host in &mut large.catalog.hosts {
        host.labels = (0..64)
            .map(|index| (format!("key-{index}"), "\\".repeat(256)))
            .collect();
    }
    assert_eq!(validate_hosts(&large.catalog), Err(HostCatalogError::Limit));
    Ok(())
}
