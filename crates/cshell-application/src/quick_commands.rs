//! Bounded command template catalog. Validation and edits are independent of GUI.
use async_trait::async_trait;
use cshell_domain::*;
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

pub const MAX_QUICK_COMMAND_CATALOG_BYTES: usize = 1024 * 1024;
pub const MAX_QUICK_COMMAND_BODY_BYTES: usize = 256 * 1024;
pub const MAX_QUICK_COMMANDS: usize = 256;
pub const MAX_QUICK_COMMAND_FOLDERS: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuickCommandSnapshot {
    pub revision: u64,
    pub catalog: QuickCommandCatalog,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QuickCommandChange {
    Upsert(QuickCommandRecord),
    Remove(QuickCommandId),
    UpsertFolder(QuickCommandFolder),
    RemoveFolder(QuickCommandFolderId),
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum QuickCommandError {
    #[error("unsupported quick command catalog format or version")]
    Unsupported,
    #[error("quick command catalog exceeds its limits")]
    Limit,
    #[error("invalid quick command metadata or folder references")]
    Invalid,
}
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum QuickCommandRepositoryError {
    #[error("quick command catalog revision changed: expected {expected}, actual {actual}")]
    Conflict { expected: u64, actual: u64 },
    #[error("quick command storage is unavailable")]
    Unavailable,
    #[error("stored quick command catalog is corrupt")]
    Corrupt,
}
#[async_trait]
pub trait QuickCommandRepository: std::fmt::Debug + Send + Sync {
    async fn load_quick_commands(
        &self,
    ) -> Result<Option<QuickCommandSnapshot>, QuickCommandRepositoryError>;
    async fn save_quick_commands(
        &self,
        expected_revision: u64,
        catalog: &QuickCommandCatalog,
    ) -> Result<QuickCommandSnapshot, QuickCommandRepositoryError>;
}

fn label(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}
pub fn validate_quick_commands(catalog: &QuickCommandCatalog) -> Result<(), QuickCommandError> {
    if catalog.format != "cshell.quick-commands" || !matches!(catalog.version, 1 | 2) {
        return Err(QuickCommandError::Unsupported);
    }
    if catalog.commands.len() > MAX_QUICK_COMMANDS
        || catalog.folders.len() > MAX_QUICK_COMMAND_FOLDERS
    {
        return Err(QuickCommandError::Limit);
    }
    let mut folders = BTreeMap::new();
    for folder in &catalog.folders {
        if !label(&folder.name) || folders.insert(folder.id, folder).is_some() {
            return Err(QuickCommandError::Invalid);
        }
    }
    for folder in &catalog.folders {
        let mut next = Some(folder.id);
        let mut visited = BTreeSet::new();
        while let Some(id) = next {
            if !visited.insert(id) {
                return Err(QuickCommandError::Invalid);
            }
            next = folders
                .get(&id)
                .ok_or(QuickCommandError::Invalid)?
                .parent_id;
        }
    }
    let mut ids = BTreeSet::new();
    for command in &catalog.commands {
        if !ids.insert(command.id)
            || !label(&command.name)
            || command
                .folder_id
                .is_some_and(|id| !folders.contains_key(&id))
            || command.tags.len() > 32
            || command.tags.iter().any(|tag| !label(tag))
        {
            return Err(QuickCommandError::Invalid);
        }
        let (text, empty_allowed) = match &command.action {
            QuickCommandAction::SendText { text, .. } => (text, false),
            QuickCommandAction::OpenCompose { text, .. } => (text, true),
            QuickCommandAction::RunTask {
                definition,
                selection,
            }
            | QuickCommandAction::RunScript {
                definition,
                selection,
            } => {
                if catalog.version != 2 {
                    return Err(QuickCommandError::Unsupported);
                }
                validate_quick_task_template(definition, selection)?;
                if matches!(command.action, QuickCommandAction::RunScript { .. })
                    && (definition.steps.len() != 1
                        || !matches!(definition.steps[0].command, TaskCommand::Script { .. }))
                {
                    return Err(QuickCommandError::Invalid);
                }
                continue;
            }
            _ => return Err(QuickCommandError::Unsupported),
        };
        if text.len() > MAX_QUICK_COMMAND_BODY_BYTES {
            return Err(QuickCommandError::Limit);
        }
        if !empty_allowed && text.is_empty() {
            return Err(QuickCommandError::Invalid);
        }
    }
    if serde_json::to_vec(catalog)
        .map_err(|_| QuickCommandError::Invalid)?
        .len()
        > MAX_QUICK_COMMAND_CATALOG_BYTES
    {
        return Err(QuickCommandError::Limit);
    }
    Ok(())
}
pub fn validate_quick_task_template(
    definition: &TaskDefinition,
    selection: &HostSelection,
) -> Result<(), QuickCommandError> {
    crate::validate_task_definition(definition).map_err(|_| QuickCommandError::Invalid)?;
    crate::encode_host_selection(selection).map_err(|_| QuickCommandError::Invalid)?;
    if matches!(selection, HostSelection::Explicit { hosts } if hosts.is_empty()) {
        return Err(QuickCommandError::Invalid);
    }
    if serde_json::to_vec(definition)
        .map_err(|_| QuickCommandError::Invalid)?
        .len()
        > MAX_QUICK_COMMAND_BODY_BYTES
    {
        return Err(QuickCommandError::Limit);
    }
    Ok(())
}
pub fn encode_quick_commands(catalog: &QuickCommandCatalog) -> Result<Vec<u8>, QuickCommandError> {
    validate_quick_commands(catalog)?;
    serde_json::to_vec(catalog).map_err(|_| QuickCommandError::Invalid)
}
pub fn decode_quick_commands(bytes: &[u8]) -> Result<QuickCommandCatalog, QuickCommandError> {
    if bytes.len() > MAX_QUICK_COMMAND_CATALOG_BYTES {
        return Err(QuickCommandError::Limit);
    }
    let catalog = serde_json::from_slice(bytes).map_err(|_| QuickCommandError::Invalid)?;
    validate_quick_commands(&catalog)?;
    Ok(catalog)
}
pub fn apply_quick_command_change(
    catalog: &QuickCommandCatalog,
    change: QuickCommandChange,
) -> Result<QuickCommandCatalog, QuickCommandError> {
    validate_quick_commands(catalog)?;
    let mut next = catalog.clone();
    match change {
        QuickCommandChange::Upsert(record) => {
            next.commands.retain(|command| command.id != record.id);
            next.commands.push(record);
        }
        QuickCommandChange::Remove(id) => next.commands.retain(|command| command.id != id),
        QuickCommandChange::UpsertFolder(folder) => {
            next.folders.retain(|item| item.id != folder.id);
            next.folders.push(folder);
        }
        QuickCommandChange::RemoveFolder(id) => next.folders.retain(|folder| folder.id != id),
    }
    if next.commands.iter().any(|command| {
        matches!(
            command.action,
            QuickCommandAction::RunTask { .. } | QuickCommandAction::RunScript { .. }
        )
    }) {
        next.version = 2;
    }
    validate_quick_commands(&next)?;
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record() -> QuickCommandRecord {
        QuickCommandRecord {
            id: QuickCommandId::new(),
            name: "checks".into(),
            folder_id: None,
            tags: BTreeSet::from(["operations".into()]),
            systems: BTreeSet::from([QuickCommandSystem::Linux]),
            action: QuickCommandAction::SendText {
                text: "echo PRIVATE_COMMAND_BODY".into(),
                append_enter: true,
                target: QuickCommandTarget::Current,
            },
        }
    }
    #[test]
    fn command_catalog_roundtrip_limits_and_unknown_actions_are_checked()
    -> Result<(), Box<dyn std::error::Error>> {
        let catalog = QuickCommandCatalog {
            commands: vec![record()],
            ..Default::default()
        };
        assert_eq!(
            decode_quick_commands(&encode_quick_commands(&catalog)?)?,
            catalog
        );
        assert!(!format!("{catalog:?}").contains("PRIVATE_COMMAND_BODY"));
        let mut next = catalog.clone();
        next.commands.push(next.commands[0].clone());
        assert_eq!(
            validate_quick_commands(&next),
            Err(QuickCommandError::Invalid)
        );
        next.commands = vec![record()];
        next.commands[0].action = QuickCommandAction::SendText {
            text: "x".repeat(MAX_QUICK_COMMAND_BODY_BYTES + 1),
            append_enter: false,
            target: QuickCommandTarget::Current,
        };
        assert_eq!(
            validate_quick_commands(&next),
            Err(QuickCommandError::Limit)
        );
        next.commands[0].action = QuickCommandAction::OpenCompose {
            text: String::new(),
            append_enter: false,
            multiline: true,
            target: QuickCommandTarget::Armed,
        };
        validate_quick_commands(&next)?;
        assert_eq!(
            decode_quick_commands(&vec![0; MAX_QUICK_COMMAND_CATALOG_BYTES + 1]),
            Err(QuickCommandError::Limit)
        );
        let encoded = String::from_utf8(encode_quick_commands(&catalog)?)?;
        assert_eq!(
            decode_quick_commands(encoded.replace("send_text", "run_unknown").as_bytes()),
            Err(QuickCommandError::Invalid)
        );
        assert_eq!(
            decode_quick_commands(
                encoded
                    .replace(
                        "\"append_enter\":true",
                        "\"append_enter\":true,\"secret_extra\":1"
                    )
                    .as_bytes()
            ),
            Err(QuickCommandError::Invalid)
        );
        Ok(())
    }
    #[test]
    fn command_folder_cycles_and_referenced_deletes_leave_original_unchanged()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = QuickCommandFolder {
            id: QuickCommandFolderId::new(),
            name: "root".into(),
            parent_id: None,
        };
        let child = QuickCommandFolder {
            id: QuickCommandFolderId::new(),
            name: "child".into(),
            parent_id: Some(root.id),
        };
        let mut command = record();
        command.folder_id = Some(child.id);
        let catalog = QuickCommandCatalog {
            folders: vec![root.clone(), child.clone()],
            commands: vec![command.clone()],
            ..Default::default()
        };
        validate_quick_commands(&catalog)?;
        assert_eq!(
            apply_quick_command_change(&catalog, QuickCommandChange::RemoveFolder(child.id)),
            Err(QuickCommandError::Invalid)
        );
        let cycle = QuickCommandFolder {
            parent_id: Some(child.id),
            ..root.clone()
        };
        assert_eq!(
            apply_quick_command_change(&catalog, QuickCommandChange::UpsertFolder(cycle)),
            Err(QuickCommandError::Invalid)
        );
        let changed = apply_quick_command_change(&catalog, QuickCommandChange::Remove(command.id))?;
        assert!(changed.commands.is_empty());
        assert_eq!(catalog.commands, vec![command]);
        Ok(())
    }
}
