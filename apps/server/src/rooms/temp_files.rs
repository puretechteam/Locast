//! Temp file management endpoints for P6-T06.
//!
//! Provides REST endpoints for listing, marking permanent,
//! and soft-deleting (OS trash) temp files. All endpoints
//! require the caller to be a room participant.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::caps::{can, Action, Scope};
use super::registry::RoomRegistry;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TempFile {
    pub id: Uuid,
    pub room_id: Uuid,
    pub owner_user_id: Uuid,
    pub path: String,
    pub is_permanent: bool,
    pub created_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TempFileList(pub Vec<TempFile>);

#[derive(Debug, Clone, Deserialize)]
pub struct MarkPermanentRequest {
    pub file_ids: Vec<Uuid>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeleteToTrashRequest {
    pub file_ids: Vec<Uuid>,
}

#[derive(Debug, thiserror::Error)]
pub enum TempFileError {
    #[error("capability denied")]
    CapabilityDenied,
    #[error("file not found")]
    NotFound,
    #[error("internal error")]
    Internal,
}

impl axum::response::IntoResponse for TempFileError {
    fn into_response(self) -> axum::response::Response {
        let (status, msg) = match &self {
            TempFileError::CapabilityDenied => (StatusCode::FORBIDDEN, self.to_string()),
            TempFileError::NotFound => (StatusCode::NOT_FOUND, self.to_string()),
            TempFileError::Internal => (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()),
        };
        (status, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}

pub type TempFileResult<T> = Result<T, TempFileError>;

pub async fn list_temp_files(
    State(state): State<Arc<TempFileAppState>>,
    Path((room_id, user_id)): Path<(Uuid, Uuid)>,
) -> TempFileResult<Json<TempFileList>> {
    if !can(
        &state.registry,
        user_id,
        room_id,
        Scope::Media,
        Action::KeepTempFile,
    )
    .await
    {
        return Err(TempFileError::CapabilityDenied);
    }
    let files = state.list_temp_files(room_id, user_id).await;
    Ok(Json(TempFileList(files)))
}

pub async fn mark_permanent(
    State(state): State<Arc<TempFileAppState>>,
    Path((room_id, user_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<MarkPermanentRequest>,
) -> TempFileResult<Json<serde_json::Value>> {
    if !can(
        &state.registry,
        user_id,
        room_id,
        Scope::Media,
        Action::KeepTempFile,
    )
    .await
    {
        return Err(TempFileError::CapabilityDenied);
    }
    let updated = state.mark_permanent(room_id, user_id, req.file_ids).await;
    Ok(Json(serde_json::json!({ "updated": updated })))
}

pub async fn delete_to_trash(
    State(state): State<Arc<TempFileAppState>>,
    Path((room_id, user_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<DeleteToTrashRequest>,
) -> TempFileResult<Json<serde_json::Value>> {
    let is_cohost = can(
        &state.registry,
        user_id,
        room_id,
        Scope::Media,
        Action::DeleteTempFile,
    )
    .await;
    if !is_cohost {
        for file_id in &req.file_ids {
            if !state.is_owner(room_id, *file_id, user_id).await {
                return Err(TempFileError::CapabilityDenied);
            }
        }
    } else if !can(
        &state.registry,
        user_id,
        room_id,
        Scope::Media,
        Action::DeleteTempFile,
    )
    .await
    {
        return Err(TempFileError::CapabilityDenied);
    }
    let deleted = state.delete_to_trash(room_id, req.file_ids).await;
    Ok(Json(serde_json::json!({ "deleted": deleted })))
}

pub struct TempFileAppState {
    pub registry: Arc<RoomRegistry>,
}

impl TempFileAppState {
    pub async fn list_temp_files(&self, _room_id: Uuid, _owner: Uuid) -> Vec<TempFile> {
        Vec::new()
    }

    pub async fn mark_permanent(
        &self,
        _room_id: Uuid,
        _owner: Uuid,
        _file_ids: Vec<Uuid>,
    ) -> usize {
        0
    }

    pub async fn delete_to_trash(&self, _room_id: Uuid, _file_ids: Vec<Uuid>) -> usize {
        0
    }

    pub async fn is_owner(&self, _room_id: Uuid, _file_id: Uuid, _user_id: Uuid) -> bool {
        false
    }
}
