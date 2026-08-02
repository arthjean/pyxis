use std::time::Duration;

use agent_core::auxiliary::files::{FileUploadRequest, UploadedFile};
use agent_core::auxiliary::{AuxiliaryError, AuxiliaryOperation, AuxiliaryPhase};
use reqwest::Method;
use serde::Deserialize;
use serde_json::json;

use super::ConfiguredOpenAiProvider;
use super::http::{http_error, read_auxiliary_response};
use super::validation::nonempty;

const FINALIZE_TIMEOUT: Duration = Duration::from_secs(30);
const FINALIZE_RETRY_DELAY: Duration = Duration::from_millis(250);
const OPENAI_FILE_URI_PREFIX: &str = "sediment://";
const OPENAI_FILE_UPLOAD_LIMIT_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Deserialize)]
struct CreateFileResponse {
    file_id: String,
    upload_url: String,
}

#[derive(Deserialize)]
struct DownloadLinkResponse {
    status: String,
    download_url: Option<String>,
    file_name: Option<String>,
    mime_type: Option<String>,
    error_message: Option<String>,
}

pub(super) async fn upload(
    provider: &ConfiguredOpenAiProvider,
    request: FileUploadRequest,
) -> Result<UploadedFile, AuxiliaryError> {
    let operation = AuxiliaryOperation::FileUpload;
    super::ensure_supported(provider, operation)?;
    let mut cancellation = provider.cancellation_snapshot();
    nonempty(operation, "file_name", &request.file_name, 1024)?;
    let actual_bytes = u64::try_from(request.contents.len()).unwrap_or(u64::MAX);
    if request.file_size_bytes > OPENAI_FILE_UPLOAD_LIMIT_BYTES
        || actual_bytes > OPENAI_FILE_UPLOAD_LIMIT_BYTES
    {
        return Err(AuxiliaryError::FileTooLarge {
            file_name: request.file_name,
            size_bytes: request.file_size_bytes.max(actual_bytes),
            limit_bytes: OPENAI_FILE_UPLOAD_LIMIT_BYTES,
        });
    }
    if request.file_size_bytes != actual_bytes {
        return Err(AuxiliaryError::FileSizeMismatch {
            declared_bytes: request.file_size_bytes,
            actual_bytes,
        });
    }

    let create = provider
        .auxiliary_json_scoped(
            operation,
            AuxiliaryPhase::Request,
            "files",
            &json!({
                "file_name": request.file_name,
                "file_size": request.file_size_bytes,
                "use_case": "codex",
            }),
            &mut cancellation,
        )
        .await?;
    let create: CreateFileResponse = serde_json::from_slice(&create.body)
        .map_err(|_| AuxiliaryError::decode(operation, "invalid create response"))?;
    validate_file_id(&create.file_id)?;
    let upload_url = url::Url::parse(&create.upload_url)
        .map_err(|_| AuxiliaryError::decode(operation, "invalid upload URL"))?;
    if !allowed_resource_url(&upload_url) {
        return Err(AuxiliaryError::decode(
            operation,
            "upload URL is not an allowed HTTP endpoint",
        ));
    }

    // This request is intentionally built from the bare client. Provider
    // default and auth headers are never available at the upload origin.
    let azure_client_request_id = format!("pyxis-{}", hex::encode(rand::random::<[u8; 16]>()));
    let upload = provider
        .send_auxiliary(
            operation,
            AuxiliaryPhase::BlobUpload,
            provider
                .http
                .put(upload_url)
                .header("x-ms-blob-type", "BlockBlob")
                .header("x-ms-client-request-id", &azure_client_request_id)
                .header(reqwest::header::CONTENT_LENGTH, request.file_size_bytes)
                .body(request.contents),
            &cancellation,
        )
        .await
        .map_err(|error| match error {
            AuxiliaryError::Timeout { .. } => AuxiliaryError::BlobUploadTransport {
                kind: "timeout",
                azure_client_request_id: azure_client_request_id.clone(),
            },
            AuxiliaryError::Transport { kind, .. } => AuxiliaryError::BlobUploadTransport {
                kind,
                azure_client_request_id: azure_client_request_id.clone(),
            },
            error => error,
        })?;
    if !upload.status().is_success() {
        let mut error = http_error(
            operation,
            AuxiliaryPhase::BlobUpload,
            upload.status().as_u16(),
            upload.headers(),
        );
        if let AuxiliaryError::Http {
            azure_client_request_id: request_id,
            ..
        } = &mut error
            && request_id.is_none()
        {
            *request_id = Some(azure_client_request_id);
        }
        return Err(error);
    }
    read_auxiliary_response(
        operation,
        AuxiliaryPhase::BlobUpload,
        upload,
        provider.config.transport.idle_timeout(),
        cancellation.clone(),
    )
    .await?;

    let finalize_path = format!("files/{}/uploaded", create.file_id);
    let deadline = tokio::time::Instant::now()
        + FINALIZE_TIMEOUT.min(provider.config.transport.idle_timeout());
    loop {
        let response = provider
            .auxiliary_request_scoped(
                operation,
                AuxiliaryPhase::Finalize,
                Method::POST,
                &finalize_path,
                Some(("application/json", b"{}".to_vec())),
                &mut cancellation,
            )
            .await?;
        let response: DownloadLinkResponse = serde_json::from_slice(&response.body)
            .map_err(|_| AuxiliaryError::decode(operation, "invalid finalize response"))?;
        match response.status.as_str() {
            "success" => {
                let download_url =
                    response
                        .download_url
                        .ok_or_else(|| AuxiliaryError::UploadFailed {
                            file_id: create.file_id.clone(),
                            reason: "missing download_url".into(),
                        })?;
                let parsed_download_url =
                    url::Url::parse(&download_url).map_err(|_| AuxiliaryError::UploadFailed {
                        file_id: create.file_id.clone(),
                        reason: "invalid download_url".into(),
                    })?;
                if !allowed_resource_url(&parsed_download_url) {
                    return Err(AuxiliaryError::UploadFailed {
                        file_id: create.file_id,
                        reason: "download_url is not an allowed HTTP endpoint".into(),
                    });
                }
                return Ok(UploadedFile {
                    uri: format!("{OPENAI_FILE_URI_PREFIX}{}", create.file_id),
                    file_id: create.file_id,
                    download_url,
                    file_name: response.file_name.unwrap_or(request.file_name),
                    file_size_bytes: request.file_size_bytes,
                    mime_type: response.mime_type.or(request.mime_type),
                });
            }
            "retry" if tokio::time::Instant::now() < deadline => {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => {
                        return Err(AuxiliaryError::Cancelled {
                            operation,
                            phase: AuxiliaryPhase::Finalize,
                        });
                    }
                    _ = tokio::time::sleep(FINALIZE_RETRY_DELAY) => {}
                }
            }
            "retry" => {
                return Err(AuxiliaryError::UploadNotReady {
                    file_id: create.file_id,
                });
            }
            _ => {
                return Err(AuxiliaryError::UploadFailed {
                    file_id: create.file_id,
                    reason: response
                        .error_message
                        .filter(|message| message.len() <= 1024)
                        .map(|message| agent_core::redaction::redact_text(&message))
                        .unwrap_or_else(|| "provider reported upload failure".into()),
                });
            }
        }
    }
}

fn allowed_resource_url(url: &url::Url) -> bool {
    if url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    match url.scheme() {
        "https" => true,
        "http" => url.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        }),
        _ => false,
    }
}

fn validate_file_id(file_id: &str) -> Result<(), AuxiliaryError> {
    if file_id.is_empty()
        || file_id.len() > 256
        || !file_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        return Err(AuxiliaryError::decode(
            AuxiliaryOperation::FileUpload,
            "invalid file id",
        ));
    }
    Ok(())
}
