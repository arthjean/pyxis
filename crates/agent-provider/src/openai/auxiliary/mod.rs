mod compact;
mod files;
mod http;
mod images;
mod json;
mod realtime;
mod search;
mod validation;

use agent_core::auxiliary::search as search_contract;
use agent_core::auxiliary::{
    AuxiliaryError, AuxiliaryOperation, AuxiliaryProvider, RealtimeSession,
};
use agent_core::auxiliary::{compact as compact_contract, files as file_contract};
use agent_core::auxiliary::{images as image_contract, realtime as realtime_contract};
use agent_core::provider::AuxiliaryCapabilities;
use async_trait::async_trait;

use super::ConfiguredOpenAiProvider;

#[async_trait]
impl AuxiliaryProvider for ConfiguredOpenAiProvider {
    async fn compact_remote(
        &self,
        request: &compact_contract::CompactRequest,
    ) -> Result<compact_contract::CompactResponse, AuxiliaryError> {
        compact::remote(self, request).await
    }

    async fn summarize_memories(
        &self,
        request: &compact_contract::MemorySummarizeInput,
    ) -> Result<Vec<compact_contract::MemorySummarizeOutput>, AuxiliaryError> {
        compact::summarize_memories(self, request).await
    }

    async fn generate_image(
        &self,
        request: &image_contract::ImageGenerationRequest,
    ) -> Result<image_contract::ImageResponse, AuxiliaryError> {
        images::generate(self, request).await
    }

    async fn edit_image(
        &self,
        request: &image_contract::ImageEditRequest,
    ) -> Result<image_contract::ImageResponse, AuxiliaryError> {
        images::edit(self, request).await
    }

    async fn search(
        &self,
        request: &search_contract::SearchRequest,
    ) -> Result<search_contract::SearchResponse, AuxiliaryError> {
        search::execute(self, request).await
    }

    async fn upload_file(
        &self,
        request: file_contract::FileUploadRequest,
    ) -> Result<file_contract::UploadedFile, AuxiliaryError> {
        files::upload(self, request).await
    }

    async fn create_realtime_call(
        &self,
        request: &realtime_contract::RealtimeCallRequest,
    ) -> Result<realtime_contract::RealtimeCallResponse, AuxiliaryError> {
        realtime::create_call(self, request).await
    }

    async fn connect_realtime(
        &self,
        config: realtime_contract::RealtimeSessionConfig,
    ) -> Result<Box<dyn RealtimeSession>, AuxiliaryError> {
        realtime::connect(self, config, None).await
    }

    async fn connect_realtime_sideband(
        &self,
        config: realtime_contract::RealtimeSessionConfig,
        call_id: &str,
    ) -> Result<Box<dyn RealtimeSession>, AuxiliaryError> {
        realtime::connect(self, config, Some(call_id)).await
    }
}

fn ensure_supported(
    provider: &ConfiguredOpenAiProvider,
    operation: AuxiliaryOperation,
) -> Result<(), AuxiliaryError> {
    let capabilities: &AuxiliaryCapabilities = &provider.capabilities.auxiliary;
    let supported = match operation {
        AuxiliaryOperation::RemoteCompact => capabilities.remote_compact,
        AuxiliaryOperation::Memories => capabilities.memories,
        AuxiliaryOperation::ImageGeneration | AuxiliaryOperation::ImageEdit => capabilities.images,
        AuxiliaryOperation::Search => capabilities.search,
        AuxiliaryOperation::FileUpload => capabilities.files,
        AuxiliaryOperation::RealtimeCall | AuxiliaryOperation::RealtimeWebSocket => {
            capabilities.realtime
        }
    };
    supported
        .then_some(())
        .ok_or(AuxiliaryError::Unsupported { operation })
}
