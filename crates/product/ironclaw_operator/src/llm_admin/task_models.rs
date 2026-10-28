//! Task models: the operator-wide model each background task runs on, kept
//! in `$IRONCLAW_REBORN_HOME/task_models.json` beside `providers.json`, and
//! the one-shot completion a task makes on it (no tools, no agent run).

use std::path::{Path, PathBuf};
use std::time::Duration;

use ironclaw_config::LlmSlotSelection;
use ironclaw_llm::{ChatMessage, CompletionRequest};
use ironclaw_product_contracts::operator_llm::LlmConfigServiceError;
use ironclaw_product_contracts::task_models::{
    TaskCompletionRequest, TaskCompletionResult, TaskModels,
};

use crate::llm_admin::LlmKeyStore;
use crate::llm_admin::llm_catalog::{
    apply_stored_api_key, resolve_llm_selection_allow_missing_key,
};

const FILE_NAME: &str = "task_models.json";
/// A title must never hold anything up for long.
const COMPLETION_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_TASK_TOKENS: u32 = 512;

pub(crate) fn file_path(home: &Path) -> PathBuf {
    home.join(FILE_NAME)
}

pub(crate) async fn load(path: &Path) -> Result<TaskModels, LlmConfigServiceError> {
    match tokio::fs::read(path).await {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
            tracing::warn!(%error, "task_models.json is not valid; ignoring it");
            LlmConfigServiceError::Internal
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(TaskModels::default()),
        Err(error) => {
            tracing::warn!(%error, "task_models.json could not be read");
            Err(LlmConfigServiceError::Unavailable)
        }
    }
}

/// Write through a temp file and rename, so a crash never leaves half a file.
pub(crate) async fn save(path: &Path, models: &TaskModels) -> Result<(), LlmConfigServiceError> {
    for choice in [&models.thread_titles].into_iter().flatten() {
        if choice.provider_id.trim().is_empty() || choice.model.trim().is_empty() {
            return Err(LlmConfigServiceError::InvalidRequest {
                field: Some("model".into()),
                reason: "a task model needs a provider and a model".into(),
            });
        }
    }
    let bytes = serde_json::to_vec_pretty(models).map_err(|error| {
        tracing::warn!(%error, "task models could not be serialized");
        LlmConfigServiceError::Internal
    })?;
    // A temp file of its own per save, then an atomic rename: two saves at
    // once cannot write into the same temp file.
    let path = path.to_path_buf();
    let write = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        use std::io::Write;
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
        tmp.write_all(&bytes)?;
        tmp.persist(&path).map_err(|error| error.error)?;
        Ok(())
    });
    match write.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => {
            tracing::warn!(%error, "task_models.json could not be written");
            Err(LlmConfigServiceError::Unavailable)
        }
        Err(error) => {
            tracing::warn!(%error, "task_models.json write task failed");
            Err(LlmConfigServiceError::Unavailable)
        }
    }
}

pub(crate) async fn complete(
    home: &Path,
    providers_path: &Path,
    keys: &LlmKeyStore,
    request: TaskCompletionRequest,
) -> Result<TaskCompletionResult, LlmConfigServiceError> {
    let models = load(&file_path(home)).await?;
    let Some(choice) = models.get(request.task) else {
        return Ok(TaskCompletionResult::default());
    };
    let selection = LlmSlotSelection {
        provider_id: Some(choice.provider_id.clone()),
        model: Some(choice.model.clone()),
        ..Default::default()
    };
    let mut config = resolve_llm_selection_allow_missing_key(&selection, Some(providers_path))
        .map_err(|error| LlmConfigServiceError::InvalidRequest {
            field: Some("provider_id".into()),
            reason: error.to_string(),
        })?;
    let key = keys.read(&choice.provider_id).await.map_err(|error| {
        tracing::warn!(%error, provider_id = %choice.provider_id, "task model key unreadable");
        LlmConfigServiceError::Unavailable
    })?;
    if let Some(key) = key {
        apply_stored_api_key(&mut config, key);
    }
    let session = ironclaw_llm::create_session_manager(config.session.clone()).await;
    let provider = ironclaw_llm::build_static_provider_chain(&config, session)
        .await
        .map_err(|error| {
            tracing::warn!(%error, provider_id = %choice.provider_id, "task model unavailable");
            LlmConfigServiceError::Unavailable
        })?;
    let mut call = CompletionRequest::new(vec![
        ChatMessage::system(request.system),
        ChatMessage::user(request.prompt),
    ])
    .with_model(choice.model.clone());
    call.max_tokens = Some(request.max_tokens.clamp(16, MAX_TASK_TOKENS));
    let response = tokio::time::timeout(COMPLETION_TIMEOUT, provider.complete(call))
        .await
        .map_err(|_elapsed| {
            tracing::warn!(provider_id = %choice.provider_id, "task completion timed out");
            LlmConfigServiceError::Unavailable
        })?
        .map_err(|error| {
            tracing::warn!(%error, provider_id = %choice.provider_id, "task completion failed");
            LlmConfigServiceError::Unavailable
        })?;
    Ok(TaskCompletionResult {
        text: Some(response.content),
    })
}

#[cfg(test)]
mod tests {
    use ironclaw_product_contracts::task_models::TaskModelChoice;

    use super::*;

    #[tokio::test]
    async fn task_models_survive_a_reload_and_reject_blanks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file_path(dir.path());
        assert_eq!(load(&path).await.expect("missing"), TaskModels::default());
        let models = TaskModels {
            thread_titles: Some(TaskModelChoice {
                provider_id: "glm".into(),
                model: "glm-4.6".into(),
            }),
        };
        save(&path, &models).await.expect("save");
        assert_eq!(load(&path).await.expect("load"), models);
        let blank = TaskModels {
            thread_titles: Some(TaskModelChoice {
                provider_id: "glm".into(),
                model: " ".into(),
            }),
        };
        assert!(save(&path, &blank).await.is_err());
        assert_eq!(load(&path).await.expect("unchanged"), models);

        // Saves at once each use their own temp file: all succeed and the
        // file holds one whole set.
        let sets: Vec<TaskModels> = (0..8)
            .map(|i| TaskModels {
                thread_titles: Some(TaskModelChoice {
                    provider_id: "glm".into(),
                    model: format!("glm-{i}"),
                }),
            })
            .collect();
        let mut saves = tokio::task::JoinSet::new();
        for m in sets.clone() {
            let path = path.clone();
            saves.spawn(async move { save(&path, &m).await });
        }
        while let Some(result) = saves.join_next().await {
            result.expect("joined").expect("concurrent save");
        }
        assert!(sets.contains(&load(&path).await.expect("whole")));
    }
}
