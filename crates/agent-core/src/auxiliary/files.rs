#[derive(Clone, PartialEq, Eq)]
pub struct FileUploadRequest {
    pub file_name: String,
    pub file_size_bytes: u64,
    pub mime_type: Option<String>,
    pub contents: Vec<u8>,
}

impl std::fmt::Debug for FileUploadRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FileUploadRequest")
            .field("file_name", &self.file_name)
            .field("file_size_bytes", &self.file_size_bytes)
            .field("mime_type", &self.mime_type)
            .field("contents", &"[REDACTED_BINARY]")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct UploadedFile {
    pub file_id: String,
    pub uri: String,
    pub download_url: String,
    pub file_name: String,
    pub file_size_bytes: u64,
    pub mime_type: Option<String>,
}

impl std::fmt::Debug for UploadedFile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UploadedFile")
            .field("file_id", &self.file_id)
            .field("uri", &self.uri)
            .field("download_url", &"[REDACTED_SIGNED_URL]")
            .field("file_name", &self.file_name)
            .field("file_size_bytes", &self.file_size_bytes)
            .field("mime_type", &self.mime_type)
            .finish()
    }
}
