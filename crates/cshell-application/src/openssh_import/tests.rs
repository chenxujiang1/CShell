use super::*;

fn options(home: &Path) -> OpenSshImportOptions<'_> {
    OpenSshImportOptions {
        default_username: "fallback",
        home_directory: Some(home),
    }
}

#[test]
fn openssh_first_values_wildcard_defaults_negation_and_aliases()
-> Result<(), Box<dyn std::error::Error>> {
    let document = import_openssh_config(
        br#"
Host prod dev
    HostName=%h.example.test
    User operator
Host p* !prod
    Port 2200
Host dev
    Port 2222
Host * !prod
    User ignored
    Port 2223
Host *
    User last
    Port 22
"#,
        OpenSshImportOptions::default(),
    )?;
    assert_eq!(
        document
            .profiles
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>(),
        ["prod", "dev"]
    );
    assert_eq!(document.ssh_connections[0].host, "prod.example.test");
    assert_eq!(document.ssh_connections[0].port, 22);
    assert_eq!(document.ssh_connections[1].port, 2222);
    assert_eq!(document.ssh_connections[1].username, "operator");
    assert_eq!(
        document.ssh_connections[1].auth_method,
        SshAuthMethod::Agent
    );
    ProfileImportDocument::parse(&serde_json::to_vec(&document)?)?;
    let global_first = import_openssh_config(
        b"User early\nHost prod\nUser later\n",
        OpenSshImportOptions::default(),
    )?;
    assert_eq!(global_first.ssh_connections[0].username, "early");
    Ok(())
}

#[test]
fn openssh_quoted_paths_comments_tokens_certificates_and_windows_escapes()
-> Result<(), Box<dyn std::error::Error>> {
    let home = std::env::current_dir()?.join("home with spaces");
    let input = "\u{feff}Host srv\r\nUser alice\r\nPort = 2201\r\nIdentityFile \"~/.ssh/密钥 %n-%h-%r-%p-%%\" # comment\r\nCertificateFile '%d/.ssh/cert #1.pub'\r\nStrictHostKeyChecking yes\r\nIdentitiesOnly yes\r\nPreferredAuthentications publickey\r\n";
    let document = import_openssh_config(input.as_bytes(), options(&home))?;
    let target = &document.ssh_connections[0];
    assert_eq!(target.auth_method, SshAuthMethod::Certificate);
    assert_eq!(
        target.private_key_path.as_deref(),
        home.join(".ssh/密钥 srv-srv-alice-2201-%").to_str()
    );
    assert_eq!(
        target.certificate_path.as_deref().map(Path::new),
        Some(home.join(".ssh/cert #1.pub").as_path())
    );
    assert_eq!(
        split_values(r#""C:\Users\alice\id key" 'a\'b'"#, 1)?,
        [r"C:\Users\alice\id key", "a'b"]
    );
    Ok(())
}

#[test]
fn openssh_jump_identity_is_internal_and_invalid_aliases_are_refused()
-> Result<(), Box<dyn std::error::Error>> {
    let document = import_openssh_config(b"Host jump\nHostName jump.test\nHost app\nHostName app.test\nProxyJump jump\nHost *\nUser ops\n", OpenSshImportOptions::default())?;
    assert_eq!(
        document.ssh_connections[1].route,
        SshRoute::Jump {
            profile_id: document.profiles[0].id
        }
    );
    for input in [
        "Host a\nProxyJump missing\nUser ops\n",
        "Host a\nProxyJump a\nUser ops\n",
        "Host a b\nProxyJump a\nUser ops\n",
        "Host a\nProxyJump ops@b:2222\nHost b\nUser ops\n",
        "Host a\nProxyJump b,c\nUser ops\n",
    ] {
        assert_eq!(
            import_openssh_config(
                input.as_bytes(),
                OpenSshImportOptions {
                    default_username: "ops",
                    home_directory: None
                }
            ),
            Err(OpenSshImportError::Jump)
        );
    }
    Ok(())
}

#[test]
fn openssh_unsupported_directives_never_execute_or_disclose_values() {
    for directive in [
        "Include",
        "Match",
        "ProxyCommand",
        "RemoteCommand",
        "LocalCommand",
        "SetEnv",
        "UserKnownHostsFile",
        "ServerAliveInterval",
    ] {
        let marker = "private-diagnostic-sentinel";
        let input = format!("Host srv\n{directive} {marker}\nUser ops\n");
        let error = import_openssh_config(input.as_bytes(), OpenSshImportOptions::default()).err();
        assert_eq!(
            error,
            Some(OpenSshImportError::UnsupportedDirective { line: 2 })
        );
        assert!(!format!("{error:?}").contains(marker));
        if let Some(error) = error {
            assert!(!error.to_string().contains(marker));
        }
    }
}

#[test]
fn openssh_auth_and_trust_policy_cannot_be_silently_weakened()
-> Result<(), Box<dyn std::error::Error>> {
    let home = std::env::current_dir()?;
    for directive in [
        "StrictHostKeyChecking no",
        "StrictHostKeyChecking accept-new",
        "IdentitiesOnly no\nIdentityFile ~/.ssh/key",
        "PreferredAuthentications publickey,password",
        "Port 0",
        "Port 65536",
        "HostName %z",
        "User ${USER}",
        "IdentityFile ~/.ssh/a\nIdentityFile ~/.ssh/b",
    ] {
        let input = format!("Host srv\nUser ops\n{directive}\n");
        // User's first value must be considered; put the unsupported User before the fallback.
        let input = if directive.starts_with("User") {
            format!("Host srv\n{directive}\n")
        } else {
            input
        };
        assert!(
            import_openssh_config(input.as_bytes(), options(&home)).is_err(),
            "{directive}"
        );
    }
    let password = import_openssh_config(
        b"Host srv\nPreferredAuthentications password\n",
        options(&home),
    )?;
    assert_eq!(
        password.ssh_connections[0].auth_method,
        SshAuthMethod::Password
    );
    for path in ["relative/key", "~someone/.ssh/key", "${HOME}/key", "%z/key"] {
        let input = format!("Host srv\nIdentityFile {path}\n");
        assert_eq!(
            import_openssh_config(input.as_bytes(), options(&home)),
            Err(OpenSshImportError::KeyPath)
        );
    }
    Ok(())
}

#[test]
fn openssh_limits_invalid_encoding_syntax_and_missing_context() {
    assert_eq!(
        wildcard_match(b"*aaaaab", b"aaaaaaaaaaa", &mut 8),
        Err(OpenSshImportError::Limit)
    );
    assert_eq!(
        import_openssh_config(
            &vec![b'x'; MAX_PROFILE_IMPORT_BYTES + 1],
            OpenSshImportOptions::default()
        ),
        Err(OpenSshImportError::Limit)
    );
    for input in [&[0xff][..], b"Host srv\0\n", b"Host srv\x1b\n"] {
        assert_eq!(
            import_openssh_config(input, OpenSshImportOptions::default()),
            Err(OpenSshImportError::Encoding)
        );
    }
    assert_eq!(
        import_openssh_config(b"Host srv\n", OpenSshImportOptions::default()),
        Err(OpenSshImportError::MissingUser)
    );
    assert_eq!(
        import_openssh_config(b"Host * !x\nUser ops\n", OpenSshImportOptions::default()),
        Err(OpenSshImportError::NoHosts)
    );
    assert_eq!(
        import_openssh_config(b"Host Srv srv\n", OpenSshImportOptions::default()),
        Err(OpenSshImportError::DuplicateAlias)
    );
    for input in [
        "Host srv\nUser \"unterminated\n",
        "Host srv\nPort==22\n",
        "Host\n",
        "Host !\n",
    ] {
        assert!(import_openssh_config(input.as_bytes(), OpenSshImportOptions::default()).is_err());
    }
    let input = (0..=MAX_HOSTS)
        .map(|i| format!("Host srv{i}\n"))
        .collect::<String>();
    assert_eq!(
        import_openssh_config(input.as_bytes(), OpenSshImportOptions::default()),
        Err(OpenSshImportError::Limit)
    );
}
