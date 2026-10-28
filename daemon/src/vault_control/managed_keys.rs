use super::*;
use cshell_ipc::{ManagedKeyInfo, ManagedKeyRequest, VaultOperation as Op};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    path::Path,
};
use zeroize::{Zeroize, Zeroizing};

const BINDING: &str = "managed-ssh-key-v1";
const MAX_KEYS: usize = 32;
const MAX_FILE: u64 = 64 * 1024;

#[derive(Serialize, Deserialize)]
struct Record {
    name: String,
    algorithm: String,
    fingerprint: String,
    public_key: String,
    private_key: String,
    passphrase: String,
}
impl Drop for Record {
    fn drop(&mut self) {
        self.private_key.zeroize();
        self.passphrase.zeroize();
    }
}
impl Record {
    fn info(&self, id: [u8; 16]) -> ManagedKeyInfo {
        ManagedKeyInfo {
            id: id.to_vec(),
            name: self.name.clone(),
            algorithm: self.algorithm.clone(),
            fingerprint: self.fingerprint.clone(),
            public_key: self.public_key.clone(),
            encrypted: !self.passphrase.is_empty(),
        }
    }
}
fn key_ref(id: [u8; 16]) -> VaultCredentialRef {
    VaultCredentialRef {
        profile_id: id,
        kind: Kind::ManagedKey,
    }
}
fn read(vault: &PortableVault, id: [u8; 16]) -> Result<Record, VaultError> {
    let secret = vault.read(key_ref(id), BINDING)?;
    serde_json::from_slice(secret.expose()).map_err(|_| VaultError::Corrupt)
}
pub(super) fn list(vault: &PortableVault) -> Result<Vec<ManagedKeyInfo>, VaultError> {
    let mut keys = vault
        .references(Kind::ManagedKey)?
        .into_iter()
        .map(|reference| Ok(read(vault, reference.profile_id)?.info(reference.profile_id)))
        .collect::<Result<Vec<_>, VaultError>>()?;
    keys.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
    Ok(keys)
}

#[derive(Debug)]
pub(crate) struct ManagedKeyError(pub VaultError, pub &'static str);
type ResultKey<T> = Result<T, ManagedKeyError>;
fn bad(message: &'static str) -> ManagedKeyError {
    ManagedKeyError(VaultError::InvalidInput, message)
}
fn state_error(error: VaultError) -> ManagedKeyError {
    ManagedKeyError(
        error,
        match error {
            VaultError::Capacity => "凭据库容量已满，请导出并整理不再使用的凭据后重试",
            VaultError::Locked => "凭据库已锁定，请解锁后重试",
            VaultError::Missing => "所选密钥已不存在，请刷新后重新选择",
            _ => "凭据库状态已变化，请刷新或解锁后再操作",
        },
    )
}
fn path(value: &str) -> ResultKey<&Path> {
    let path = Path::new(value);
    if value.len() > 4096 || value.contains('\0') || !path.is_absolute() {
        return Err(bad("请选择有效的绝对路径"));
    }
    Ok(path)
}
fn read_file(value: &str) -> ResultKey<Zeroizing<String>> {
    let file = std::fs::File::open(path(value)?).map_err(|_| bad("无法读取所选密钥文件"))?;
    if !file
        .metadata()
        .is_ok_and(|m| m.is_file() && m.len() <= MAX_FILE)
    {
        return Err(bad("密钥文件必须是小于 64 KiB 的普通文件"));
    }
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(MAX_FILE + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| bad("读取密钥文件失败"))?;
    if bytes.len() > MAX_FILE as usize {
        return Err(bad("密钥文件过大"));
    }
    Ok(Zeroizing::new(
        std::str::from_utf8(&bytes)
            .map_err(|_| bad("密钥文件编码无效"))?
            .to_owned(),
    ))
}
fn export_pair(record: &Record, destination: &Path) -> ResultKey<()> {
    // A new dedicated directory prevents accidental replacement of existing SSH keys.
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(destination).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            bad("导出目录已存在，请使用新的目录名；已有文件不会被覆盖")
        } else {
            bad("无法创建导出目录，请检查父目录和写入权限")
        }
    })?;
    cshell_ipc::RuntimePaths::prepare(destination.to_owned())
        .map_err(|_| bad("无法保护导出目录权限，未写入密钥"))?;
    for (name, bytes) in [
        ("id_key.pub", record.public_key.as_bytes()),
        ("id_key", record.private_key.as_bytes()),
    ] {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(destination.join(name))
            .map_err(|_| bad("导出失败，请检查目标目录；已有文件不会被覆盖"))?;
        file.write_all(bytes)
            .and_then(|_| {
                if bytes.ends_with(b"\n") {
                    Ok(())
                } else {
                    file.write_all(b"\n")
                }
            })
            .and_then(|_| file.sync_all())
            .map_err(|_| bad("密钥文件写入未完成，请检查磁盘空间并使用新目录重试"))?;
    }
    Ok(())
}

impl VaultController {
    pub(crate) fn managed_key_operation(
        &self,
        operation: Op,
        request: &ManagedKeyRequest,
        secret: &Secret,
        revision: u64,
        generation: u64,
    ) -> ResultKey<&'static str> {
        self.ensure_owned().map_err(state_error)?;
        // Check before any expensive decoding, and fence the actual write/export again below.
        self.commit_auth(generation, || {
            let mut state = self.guard();
            let vault = portable(&mut state)?;
            check_revision(vault, revision)?;
            vault.references(Kind::ManagedKey).map(|_| ())
        })
        .map_err(state_error)?
        .map_err(state_error)?;
        match operation {
            Op::GenerateManagedKey | Op::ImportManagedKey => {
                let name = request.name.trim();
                if name.is_empty()
                    || name.len() > 128
                    || name.chars().any(char::is_control)
                    || !request.id.is_empty()
                    || !request.export_directory.is_empty()
                {
                    return Err(bad("请输入 1～128 字节的密钥名称"));
                }
                let phrase =
                    std::str::from_utf8(secret.expose()).map_err(|_| bad("私钥口令编码无效"))?;
                if phrase.len() > 1024 {
                    return Err(bad("私钥口令过长"));
                }
                let key = if operation == Op::GenerateManagedKey {
                    if request.algorithm != "ed25519"
                        || !request.private_path.is_empty()
                        || !request.public_path.is_empty()
                    {
                        return Err(bad("请选择 Ed25519 算法"));
                    }
                    cshell_ssh::SshPrivateKey::generate_ed25519()
                        .map_err(|_| bad("密钥生成失败"))?
                } else {
                    if !request.algorithm.is_empty() {
                        return Err(bad("导入时无需指定算法"));
                    }
                    let encoded = read_file(&request.private_path)?;
                    let key = cshell_ssh::SshPrivateKey::decode_managed_private_key(
                        &encoded,
                        (!phrase.is_empty()).then_some(phrase),
                    )
                    .map_err(|error| bad(error.message()))?;
                    if !request.public_path.is_empty()
                        && !key.matches_public_openssh(&read_file(&request.public_path)?)
                    {
                        return Err(bad("公钥与私钥不匹配，请选择同一密钥对"));
                    }
                    key
                };
                let record = Record {
                    name: name.to_owned(),
                    algorithm: key.algorithm_name(),
                    fingerprint: key.sha256_fingerprint(),
                    public_key: key.public_openssh().map_err(|_| bad("公钥提取失败"))?,
                    private_key: key
                        .encode_managed_openssh((!phrase.is_empty()).then_some(phrase))
                        .map_err(|_| bad("私钥编码失败"))?
                        .to_string(),
                    passphrase: phrase.to_owned(),
                };
                let bytes =
                    Secret::new(serde_json::to_vec(&record).map_err(|_| bad("密钥编码失败"))?);
                let id = *ProfileId::new().as_uuid().as_bytes();
                self.commit_auth(generation, || {
                    let mut state = self.guard();
                    let vault = portable(&mut state)?;
                    check_revision(vault, revision)?;
                    if vault.references(Kind::ManagedKey)?.len() >= MAX_KEYS {
                        return Err(VaultError::Capacity);
                    }
                    vault.write(key_ref(id), BINDING, &bytes)
                })
                .map_err(state_error)?
                .map_err(state_error)?;
                Ok(if operation == Op::GenerateManagedKey {
                    "密钥对已生成并保存，可查看公钥或导出"
                } else {
                    "密钥对已导入，公私钥已校验"
                })
            }
            Op::DeleteManagedKey | Op::ExportManagedKey => {
                let id: [u8; 16] = request
                    .id
                    .as_slice()
                    .try_into()
                    .map_err(|_| bad("请选择密钥"))?;
                if !secret.expose().is_empty()
                    || !request.name.is_empty()
                    || !request.algorithm.is_empty()
                    || !request.private_path.is_empty()
                    || !request.public_path.is_empty()
                {
                    return Err(bad("无效的密钥操作"));
                }
                if operation == Op::DeleteManagedKey && !request.export_directory.is_empty() {
                    return Err(bad("无效的密钥操作"));
                }
                self.commit_auth(generation, || -> ResultKey<()> {
                    let mut state = self.guard();
                    let vault = portable(&mut state).map_err(state_error)?;
                    check_revision(vault, revision).map_err(state_error)?;
                    if operation == Op::DeleteManagedKey {
                        read(vault, id).map_err(state_error)?;
                        vault.delete(key_ref(id)).map_err(state_error)
                    } else {
                        export_pair(
                            &read(vault, id).map_err(state_error)?,
                            path(&request.export_directory)?,
                        )
                    }
                })
                .map_err(state_error)??;
                Ok(if operation == Op::DeleteManagedKey {
                    "密钥已从凭据库移除"
                } else {
                    "密钥对已导出：id_key 为私钥，id_key.pub 为公钥；保留原私钥口令设置"
                })
            }
            _ => Err(bad("不支持的密钥操作")),
        }
    }
}

#[cfg(test)]
mod tests;
