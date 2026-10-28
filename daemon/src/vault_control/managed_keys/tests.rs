#![allow(clippy::unwrap_used)]
use super::*;

fn operate(
    controller: &VaultController,
    op: Op,
    request: ManagedKeyRequest,
    phrase: &str,
) -> ResultKey<&'static str> {
    let response = controller.response(None);
    controller.managed_key_operation(
        op,
        &request,
        &Secret::new(phrase.as_bytes().to_vec()),
        response.revision,
        response.lock_generation,
    )
}

#[test]
fn rsa4096_size_is_accepted_while_legacy_exponent_has_a_precise_error() {
    const NORMAL: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../crates/cshell-ssh/tests/fixtures/legacy_rsa4096_des3"
    ));
    const LEGACY: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../crates/cshell-ssh/tests/fixtures/legacy_rsa4096_e35_des3"
    ));
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("id_rsa_4096");
    let controller = VaultController::new(root.path().join("library.vault"));
    controller
        .create(&Secret::new(b"rsa4096-test-vault-master".to_vec()), 0)
        .unwrap();
    let request = ManagedKeyRequest {
        name: "rsa4096".into(),
        private_path: source.to_string_lossy().into_owned(),
        ..Default::default()
    };
    std::fs::write(&source, LEGACY).unwrap();
    let revision = controller.response(None).revision;
    let error = operate(
        &controller,
        Op::ImportManagedKey,
        request.clone(),
        "cshell-fixture-only",
    )
    .unwrap_err();
    assert!(error.1.contains("公有指数"));
    assert!(error.1.contains("4096 位长度本身受支持"));
    assert_eq!(controller.response(None).revision, revision);
    assert!(controller.response(None).managed_keys.is_empty());
    std::fs::write(&source, NORMAL).unwrap();
    operate(
        &controller,
        Op::ImportManagedKey,
        request,
        "cshell-fixture-only",
    )
    .unwrap();
    let response = controller.response(None);
    assert_eq!(response.managed_keys.len(), 1);
    assert_eq!(response.managed_keys[0].algorithm, "ssh-rsa");
    assert!(response.managed_keys[0].encrypted);
    assert_eq!(std::fs::read_to_string(source).unwrap(), NORMAL);
}

#[test]
fn extensionless_legacy_des3_import_reports_errors_and_exports_modern_openssh() {
    const PRIVATE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../crates/cshell-ssh/tests/fixtures/legacy_rsa_des3"
    ));
    const PUBLIC: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../crates/cshell-ssh/tests/fixtures/legacy_rsa_des3.pub"
    ));
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("测试密钥目录");
    std::fs::create_dir(&input).unwrap();
    let source = input.join("id_rsa");
    std::fs::write(&source, PRIVATE).unwrap();
    std::fs::write(input.join("id_rsa.pub"), PUBLIC).unwrap();
    let controller = VaultController::new(root.path().join("library.vault"));
    controller
        .create(&Secret::new(b"test-vault-master".to_vec()), 0)
        .unwrap();
    let request = ManagedKeyRequest {
        name: "legacy-rsa".into(),
        private_path: source.to_string_lossy().into_owned(),
        public_path: input.join("id_rsa.pub").to_string_lossy().into_owned(),
        ..Default::default()
    };
    let revision = controller.response(None).revision;
    let missing = operate(&controller, Op::ImportManagedKey, request.clone(), "").unwrap_err();
    assert!(missing.1.contains("请填写私钥口令"));
    let wrong = operate(&controller, Op::ImportManagedKey, request.clone(), "wrong").unwrap_err();
    assert!(wrong.1.contains("解密失败"));
    assert_ne!(missing.1, wrong.1);
    assert_eq!(controller.response(None).revision, revision);
    assert!(controller.response(None).managed_keys.is_empty());
    operate(
        &controller,
        Op::ImportManagedKey,
        request,
        "cshell-fixture-only",
    )
    .unwrap();
    let response = controller.response(None);
    assert_eq!(response.managed_keys.len(), 1);
    let key = &response.managed_keys[0];
    assert_eq!(key.algorithm, "ssh-rsa");
    assert!(key.encrypted);
    let destination = root.path().join("exported");
    operate(
        &controller,
        Op::ExportManagedKey,
        ManagedKeyRequest {
            id: key.id.clone(),
            export_directory: destination.to_string_lossy().into_owned(),
            ..Default::default()
        },
        "",
    )
    .unwrap();
    let exported = Zeroizing::new(std::fs::read_to_string(destination.join("id_key")).unwrap());
    assert!(exported.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----"));
    assert!(cshell_ssh::SshPrivateKey::decode_private_key(&exported, None).is_err());
    let decoded = cshell_ssh::SshPrivateKey::decode_managed_private_key(
        &exported,
        Some("cshell-fixture-only"),
    )
    .unwrap();
    assert!(decoded.matches_public_openssh(PUBLIC));
    assert_eq!(std::fs::read_to_string(source).unwrap(), PRIVATE);
}

#[test]
fn managed_key_library_import_export_and_encryption_survive_restart_without_clobber() {
    let root = tempfile::tempdir().unwrap();
    let vault_path = root.path().join("library.vault");
    let controller = VaultController::new(vault_path.clone());
    let master = Secret::new(b"managed-key-test-master".to_vec());
    controller.create(&master, 0).unwrap();
    operate(
        &controller,
        Op::GenerateManagedKey,
        ManagedKeyRequest {
            name: "my-key".into(),
            algorithm: "ed25519".into(),
            ..Default::default()
        },
        "key-test-passphrase",
    )
    .unwrap();
    let response = controller.response(None);
    assert_eq!(response.managed_keys.len(), 1);
    let key = response.managed_keys[0].clone();
    assert!(key.encrypted);
    assert!(key.public_key.starts_with("ssh-ed25519 "));
    let bytes = std::fs::read(&vault_path).unwrap();
    for private in ["my-key", "key-test-passphrase", "OPENSSH PRIVATE KEY"] {
        assert!(
            !bytes
                .windows(private.len())
                .any(|w| w == private.as_bytes())
        );
    }
    let exported = root.path().join("exported-key");
    let export_request = ManagedKeyRequest {
        id: key.id.clone(),
        export_directory: exported.to_string_lossy().into_owned(),
        ..Default::default()
    };
    operate(
        &controller,
        Op::ExportManagedKey,
        export_request.clone(),
        "",
    )
    .unwrap();
    let private = Zeroizing::new(std::fs::read_to_string(exported.join("id_key")).unwrap());
    assert!(cshell_ssh::SshPrivateKey::decode_private_key(&private, None).is_err());
    let decoded =
        cshell_ssh::SshPrivateKey::decode_private_key(&private, Some("key-test-passphrase"))
            .unwrap();
    assert!(
        decoded
            .matches_public_openssh(&std::fs::read_to_string(exported.join("id_key.pub")).unwrap())
    );
    assert!(operate(&controller, Op::ExportManagedKey, export_request, "").is_err());
    assert_eq!(
        *private,
        std::fs::read_to_string(exported.join("id_key")).unwrap()
    );
    let import_request = ManagedKeyRequest {
        name: "imported-key".into(),
        private_path: exported.join("id_key").to_string_lossy().into_owned(),
        public_path: exported.join("id_key.pub").to_string_lossy().into_owned(),
        ..Default::default()
    };
    assert!(
        operate(
            &controller,
            Op::ImportManagedKey,
            import_request.clone(),
            "wrong"
        )
        .is_err()
    );
    let other = cshell_ssh::SshPrivateKey::generate_ed25519().unwrap();
    std::fs::write(
        root.path().join("other.pub"),
        other.public_openssh().unwrap(),
    )
    .unwrap();
    let mut mismatch = import_request.clone();
    mismatch.public_path = root.path().join("other.pub").to_string_lossy().into_owned();
    assert!(
        operate(
            &controller,
            Op::ImportManagedKey,
            mismatch,
            "key-test-passphrase"
        )
        .is_err()
    );
    assert_eq!(controller.response(None).managed_keys.len(), 1);
    operate(
        &controller,
        Op::ImportManagedKey,
        import_request,
        "key-test-passphrase",
    )
    .unwrap();
    assert_eq!(controller.response(None).managed_keys.len(), 2);
    drop(controller);
    let controller = VaultController::new(vault_path);
    assert!(controller.response(None).managed_keys.is_empty());
    controller
        .unlock(&master, controller.response(None).lock_generation)
        .unwrap();
    assert_eq!(controller.response(None).managed_keys.len(), 2);
    operate(
        &controller,
        Op::DeleteManagedKey,
        ManagedKeyRequest {
            id: key.id,
            ..Default::default()
        },
        "",
    )
    .unwrap();
    assert_eq!(controller.response(None).managed_keys.len(), 1);
}

#[test]
fn locked_or_stale_requests_cannot_create_export_or_delete_keys() {
    let root = tempfile::tempdir().unwrap();
    let controller = VaultController::new(root.path().join("vault"));
    controller
        .create(&Secret::new(b"test-master-password".to_vec()), 0)
        .unwrap();
    let stale = controller.response(None);
    let request = ManagedKeyRequest {
        name: "plain".into(),
        algorithm: "ed25519".into(),
        ..Default::default()
    };
    operate(&controller, Op::GenerateManagedKey, request.clone(), "").unwrap();
    assert!(
        controller
            .managed_key_operation(
                Op::GenerateManagedKey,
                &request,
                &Secret::new(vec![]),
                stale.revision,
                stale.lock_generation
            )
            .is_err()
    );
    let current = controller.response(None);
    let key = current.managed_keys[0].clone();
    assert!(!key.encrypted);
    let destination = root.path().join("plain-export");
    let export = ManagedKeyRequest {
        id: key.id.clone(),
        export_directory: destination.to_string_lossy().into_owned(),
        ..Default::default()
    };
    operate(&controller, Op::ExportManagedKey, export.clone(), "").unwrap();
    let raw = Zeroizing::new(std::fs::read_to_string(destination.join("id_key")).unwrap());
    assert!(cshell_ssh::SshPrivateKey::decode_private_key(&raw, None).is_ok());
    controller.lock();
    let locked_export = root.path().join("must-not-exist");
    let mut export = export;
    export.export_directory = locked_export.to_string_lossy().into_owned();
    assert!(
        controller
            .managed_key_operation(
                Op::ExportManagedKey,
                &export,
                &Secret::new(vec![]),
                current.revision,
                current.lock_generation
            )
            .is_err()
    );
    assert!(!locked_export.exists());
    assert!(operate(&controller, Op::GenerateManagedKey, request, "").is_err());
    assert!(
        operate(
            &controller,
            Op::DeleteManagedKey,
            ManagedKeyRequest {
                id: key.id,
                ..Default::default()
            },
            ""
        )
        .is_err()
    );
    assert!(controller.response(None).managed_keys.is_empty());
}
