use super::*;
/// None in existing definitions means exclusive no-overwrite publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilePublicationPolicy {
    SkipIdentical,
    BackupReplace,
}
pub fn definition_uses_file_publication(bytes: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .is_some_and(|value| {
            value
                .get("publication_policy")
                .is_some_and(|p| !p.is_null())
                || value
                    .get("steps")
                    .and_then(|steps| steps.as_array())
                    .is_some_and(|steps| {
                        steps.iter().any(|step| {
                            matches!(
                                step.get("kind").and_then(|kind| kind.as_str()),
                                Some("upload" | "download")
                            ) && step
                                .get("definition")
                                .and_then(|d| d.get("publication_policy"))
                                .is_some_and(|p| !p.is_null())
                        })
                    })
        })
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilePublicationReceipt {
    pub previous: Option<UploadContent>,
    pub skipped_identical: bool,
    pub backup_verified: bool,
}
impl FilePublicationReceipt {
    pub fn planned(previous: Option<UploadContent>) -> Self {
        Self {
            previous,
            skipped_identical: false,
            backup_verified: false,
        }
    }
    pub fn validate(&self) -> Result<(), TaskError> {
        if self
            .previous
            .is_some_and(|content| content.size > MAX_UPLOAD_FILE_BYTES)
            || (self.skipped_identical || self.backup_verified) && self.previous.is_none()
            || self.skipped_identical && self.backup_verified
        {
            return Err(TaskError::Invalid);
        }
        Ok(())
    }
    pub fn validate_next(&self, next: &Self) -> Result<(), TaskError> {
        self.validate()?;
        next.validate()?;
        if self.previous != next.previous
            || self.skipped_identical && !next.skipped_identical
            || self.backup_verified && !next.backup_verified
        {
            return Err(TaskError::Transition);
        }
        Ok(())
    }
    pub fn validate_commit(
        &self,
        policy: FilePublicationPolicy,
        expected: UploadContent,
    ) -> Result<(), TaskError> {
        self.validate()?;
        if self.skipped_identical && self.previous != Some(expected)
            || policy == FilePublicationPolicy::SkipIdentical && self.backup_verified
            || policy == FilePublicationPolicy::SkipIdentical
                && self.previous.is_some()
                && !self.skipped_identical
            || policy == FilePublicationPolicy::BackupReplace
                && self.previous.is_some()
                && !self.skipped_identical
                && !self.backup_verified
        {
            return Err(TaskError::Invalid);
        }
        Ok(())
    }
}
