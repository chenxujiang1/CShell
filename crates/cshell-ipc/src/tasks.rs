use prost::{Enumeration, Message};
use zeroize::Zeroize;

#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct TaskRequest {
    #[prost(enumeration = "TaskOperation", tag = "1")]
    pub operation: i32,
    #[prost(bytes = "vec", tag = "2")]
    pub run_id: Vec<u8>,
    #[prost(uint64, tag = "3")]
    pub expected_revision: u64,
    #[prost(uint64, tag = "4")]
    pub host_revision: u64,
    #[prost(uint64, tag = "5")]
    pub profile_revision: u64,
    #[prost(bytes = "vec", tag = "6")]
    pub definition_json: Vec<u8>,
    #[prost(bytes = "vec", tag = "7")]
    pub selection_json: Vec<u8>,
    #[prost(bytes = "vec", tag = "8")]
    pub gate_id: Vec<u8>,
    #[prost(bool, tag = "9")]
    pub confirmed: bool,
    #[prost(uint32, tag = "10")]
    pub limit: u32,
    #[prost(bytes = "vec", tag = "11")]
    pub host_id: Vec<u8>,
    #[prost(uint32, tag = "12")]
    pub step: u32,
    #[prost(bool, tag = "13")]
    pub stderr: bool,
    #[prost(uint64, tag = "14")]
    pub offset: u64,
    #[prost(bytes = "vec", repeated, tag = "15")]
    pub retry_hosts: Vec<Vec<u8>>,
    #[prost(bool, tag = "16")]
    pub confirm_unknown: bool,
    #[prost(bytes = "vec", tag = "17")]
    pub compare_host_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "18")]
    pub expected_targets_json: Vec<u8>,
    #[prost(bytes = "vec", tag = "19")]
    pub result_id: Vec<u8>,
}
#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct TaskResponse {
    #[prost(enumeration = "TaskStatus", tag = "1")]
    pub status: i32,
    #[prost(bytes = "vec", tag = "2")]
    pub run_id: Vec<u8>,
    #[prost(uint64, tag = "3")]
    pub revision: u64,
    #[prost(bytes = "vec", tag = "4")]
    pub document_json: Vec<u8>,
    #[prost(bytes = "vec", tag = "5")]
    pub output: Vec<u8>,
    #[prost(uint64, tag = "6")]
    pub output_length: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Enumeration)]
#[repr(i32)]
pub enum TaskOperation {
    Create = 0,
    Get = 1,
    List = 2,
    Start = 3,
    Cancel = 4,
    Approve = 5,
    Output = 6,
    Review = 7,
    Retry = 8,
    Results = 9,
    Compare = 10,
    UploadCreate = 11,
    UploadGet = 12,
    UploadStart = 13,
    UploadCancel = 14,
    UploadResume = 15,
    UploadProbe = 16,
    UploadCleanup = 17,
    UploadAdopt = 18,
    UploadList = 19,
    DownloadCreate = 20,
    DownloadGet = 21,
    DownloadList = 22,
    DownloadStart = 23,
    DownloadCancel = 24,
    DownloadResume = 25,
    DownloadProbe = 26,
    DownloadCleanup = 27,
    DownloadAdopt = 28,
    MixedCreate = 29,
    MixedGet = 30,
    MixedList = 31,
    MixedReview = 32,
    MixedStart = 33,
    MixedCancel = 34,
    MixedOutput = 35,
    MixedFileProbe = 36,
    MixedFileResume = 37,
    MixedFileCleanup = 38,
    MixedFileAdopt = 39,
    MixedFileStatus = 40,
    MixedResults = 41,
    MixedResultPage = 42,
    OutputAnalyze = 43,
    OutputAnalysisGet = 44,
    OutputAnalysisCancel = 45,
    OutputSearch = 46,
    OutputSearchGet = 47,
    OutputSearchCancel = 48,
    OutputMerge = 49,
    OutputArchive = 50,
    OutputArchiveGet = 51,
    OutputArchiveCancel = 52,
    OutputArchiveRead = 53,
    MixedRetry = 54,
    MixedRolloutGet = 55,
    MixedRolloutApprove = 56,
    OutputWorkspace = 57,
    OutputWorkspaceGet = 58,
    OutputWorkspaceCancel = 59,
    OutputWorkspacePage = 60,
    OutputWorkspaceRead = 61,
    OutputRetentionReceipt = 62,
    OutputIndexRead = 63,
}
impl TaskOperation {
    pub const fn is_output_workspace(self) -> bool {
        matches!(
            self,
            Self::OutputWorkspace
                | Self::OutputWorkspaceGet
                | Self::OutputWorkspaceCancel
                | Self::OutputWorkspacePage
                | Self::OutputWorkspaceRead
                | Self::OutputRetentionReceipt
                | Self::OutputIndexRead
        )
    }
    #[must_use]
    pub const fn is_output_archive(self) -> bool {
        matches!(
            self,
            Self::OutputArchive
                | Self::OutputArchiveGet
                | Self::OutputArchiveCancel
                | Self::OutputArchiveRead
        )
    }
    #[must_use]
    pub const fn is_output_merge(self) -> bool {
        matches!(self, Self::OutputMerge)
    }
    #[must_use]
    pub const fn is_output_search(self) -> bool {
        matches!(
            self,
            Self::OutputSearch | Self::OutputSearchGet | Self::OutputSearchCancel
        )
    }
    #[must_use]
    pub const fn is_output_analysis(self) -> bool {
        matches!(
            self,
            Self::OutputAnalyze | Self::OutputAnalysisGet | Self::OutputAnalysisCancel
        )
    }
    #[must_use]
    pub const fn is_mixed_results(self) -> bool {
        matches!(self, Self::MixedResults | Self::MixedResultPage)
    }
    #[must_use]
    pub const fn is_mixed_repair(self) -> bool {
        matches!(
            self,
            Self::MixedFileProbe
                | Self::MixedFileResume
                | Self::MixedFileCleanup
                | Self::MixedFileAdopt
                | Self::MixedFileStatus
        )
    }
    #[must_use]
    pub const fn is_mixed(self) -> bool {
        matches!(
            self,
            Self::MixedCreate
                | Self::MixedRolloutGet
                | Self::MixedRolloutApprove
                | Self::MixedRetry
                | Self::MixedResults
                | Self::MixedResultPage
                | Self::MixedFileProbe
                | Self::MixedFileResume
                | Self::MixedFileCleanup
                | Self::MixedFileAdopt
                | Self::MixedFileStatus
                | Self::MixedGet
                | Self::MixedList
                | Self::MixedReview
                | Self::MixedStart
                | Self::MixedCancel
                | Self::MixedOutput
        )
    }
    #[must_use]
    pub const fn is_download(self) -> bool {
        matches!(
            self,
            Self::DownloadCreate
                | Self::DownloadGet
                | Self::DownloadList
                | Self::DownloadStart
                | Self::DownloadCancel
                | Self::DownloadResume
                | Self::DownloadProbe
                | Self::DownloadCleanup
                | Self::DownloadAdopt
        )
    }
    #[must_use]
    pub const fn is_upload(self) -> bool {
        matches!(
            self,
            Self::UploadCreate
                | Self::UploadGet
                | Self::UploadStart
                | Self::UploadCancel
                | Self::UploadResume
                | Self::UploadProbe
                | Self::UploadCleanup
                | Self::UploadAdopt
                | Self::UploadList
        )
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Enumeration)]
#[repr(i32)]
pub enum TaskStatus {
    Ok = 0,
    Invalid = 1,
    Conflict = 2,
    Unavailable = 3,
    Unsupported = 4,
    Missing = 5,
    Busy = 6,
    Limit = 7,
    Corrupt = 8,
}
impl std::fmt::Debug for TaskRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskRequest")
            .field("operation", &self.operation)
            .field("revision", &self.expected_revision)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for TaskResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskResponse")
            .field("status", &self.status)
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}
impl Drop for TaskRequest {
    fn drop(&mut self) {
        self.expected_targets_json.zeroize();
        self.result_id.zeroize();
        self.definition_json.zeroize();
        self.selection_json.zeroize();
    }
}
impl Drop for TaskResponse {
    fn drop(&mut self) {
        self.document_json.zeroize();
        self.output.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn task_wire_roundtrip_does_not_log_bodies() -> Result<(), Box<dyn std::error::Error>> {
        let mut request = TaskRequest::default();
        request.definition_json = b"SECRET_COMMAND".to_vec();
        request.selection_json = b"SECRET_HOST".to_vec();
        let mut response = TaskResponse::default();
        response.output = b"SECRET_OUTPUT".to_vec();
        response.document_json = b"SECRET_COMMAND".to_vec();
        assert!(!format!("{request:?} {response:?}").contains("SECRET"));
        assert_eq!(
            TaskRequest::decode(request.encode_to_vec().as_slice())?,
            request
        );
        assert_eq!(
            TaskResponse::decode(response.encode_to_vec().as_slice())?,
            response
        );
        Ok(())
    }
}
