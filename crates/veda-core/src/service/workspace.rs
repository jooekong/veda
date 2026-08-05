//! Workspace / tenant provisioning shared by the `vk_` control plane
//! (`POST /v1/workspaces`) and the platform gateway surface
//! (`POST /v1/workspace/{workspace}/projects`).
//!
//! This is the one place where control-plane rows (MySQL) and the physical
//! Milvus collection of a db-kind workspace are created together — the
//! cross-store rollback ordering lives here, not in any HTTP handler.

use std::sync::Arc;

use chrono::Utc;
use tracing::warn;
use uuid::Uuid;
use veda_types::api::CreateWorkspaceRequest;
use veda_types::{
    validate, Account, AccountStatus, Dataset, DatasetStatus, Result, VedaError, Workspace,
    WorkspaceKind, WorkspaceStatus,
};

use crate::milvus::vector_collection_name;
use crate::store::{AuthStore, VectorWorkspaceStore};

pub struct WorkspaceService {
    auth: Arc<dyn AuthStore>,
    vector_store: Arc<dyn VectorWorkspaceStore>,
    /// Embedding dim from server config. Stamped into the Milvus vector field
    /// on db workspace collection creation.
    embedding_dim: u32,
}

impl WorkspaceService {
    pub fn new(
        auth: Arc<dyn AuthStore>,
        vector_store: Arc<dyn VectorWorkspaceStore>,
        embedding_dim: u32,
    ) -> Self {
        Self {
            auth,
            vector_store,
            embedding_dim,
        }
    }

    /// Create a workspace under `account_id`. For `kind=db`, commits the
    /// workspace + bootstrap `default` dataset in one tx, then provisions the
    /// Milvus collection with rollback on failure. `req.app_id` is the
    /// workspace's governance label; the platform surface sets it to the path
    /// workspace code.
    pub async fn create_workspace(
        &self,
        account_id: String,
        req: CreateWorkspaceRequest,
    ) -> Result<Workspace> {
        let now = Utc::now();
        let ws = Workspace {
            id: Uuid::new_v4().to_string(),
            account_id,
            name: req.name,
            status: WorkspaceStatus::Active,
            kind: req.kind,
            app_id: req.app_id,
            description: req.description,
            created_at: now,
            updated_at: now,
        };
        if ws.kind == WorkspaceKind::Db {
            // workspace + bootstrap dataset commit together in one tx (no
            // orphan-workspace window), then provision the Milvus collection
            // with rollback on failure.
            let default_dataset = Dataset {
                id: Uuid::new_v4().to_string(),
                workspace_id: ws.id.clone(),
                name: validate::DEFAULT_DATASET.to_string(),
                status: DatasetStatus::Active,
                description: None,
                created_at: ws.created_at,
                updated_at: ws.updated_at,
            };
            self.auth.create_db_workspace(&ws, &default_dataset).await?;
            self.provision_db_collection(&ws).await?;
        } else {
            self.auth.create_workspace(&ws).await?;
        }

        Ok(ws)
    }

    /// Create the Milvus collection for an already-persisted db workspace (its
    /// workspace + default dataset rows were committed together by
    /// `create_db_workspace`). On failure, roll back the DB metadata FIRST, then
    /// drop the partial collection. Order matters: if we crash mid-rollback,
    /// dropping the control-plane rows first means the user sees a clean "no such
    /// workspace" rather than a zombie workspace they can list but can't use
    /// (collection gone); the leftover orphan collection is pure storage waste
    /// that the archived-resource GC (todo H1) reclaims. All steps are idempotent
    /// (drop swallows not-exists), so partial rollback failures don't compound.
    async fn provision_db_collection(&self, ws: &Workspace) -> Result<()> {
        if let Err(e) = self
            .vector_store
            .create_vector_collection(&ws.id, self.embedding_dim)
            .await
        {
            if let Err(rb) = self.auth.hard_delete_datasets_for_workspace(&ws.id).await {
                warn!(
                    workspace_id = %ws.id,
                    provision_err = %e,
                    rollback_err = %rb,
                    "rollback hard_delete_datasets failed",
                );
            }
            if let Err(rb) = self.auth.hard_delete_workspace(&ws.id).await {
                warn!(
                    workspace_id = %ws.id,
                    provision_err = %e,
                    rollback_err = %rb,
                    "rollback hard_delete_workspace failed",
                );
            }
            let collection_name = vector_collection_name(&ws.id);
            if let Err(rb) = self.vector_store.drop_collection(&collection_name).await {
                warn!(
                    workspace_id = %ws.id,
                    collection_name = %collection_name,
                    provision_err = %e,
                    rollback_err = %rb,
                    "rollback drop_collection failed after milvus create error; \
                     orphan collection may remain (reclaimed by archived-resource GC)",
                );
            }
            return Err(e);
        }

        Ok(())
    }

    /// Look up the account for a platform `workspace` code (`app_id`), treating
    /// a **suspended** account as unavailable — mirrors the `vk_` / `wk_` auth
    /// paths, which only match active accounts (so ops can lock a tenant out of
    /// the control plane too). Returns `Ok(None)` when the code is simply unknown.
    pub async fn lookup_active_account(&self, app_id: &str) -> Result<Option<Account>> {
        match self.auth.get_account_by_app_id(app_id).await? {
            Some(acc) if acc.status == AccountStatus::Active => Ok(Some(acc)),
            Some(_) => Err(VedaError::Unauthorized("account suspended".into())),
            None => Ok(None),
        }
    }

    /// Resolve the account for a platform `workspace` code, creating it
    /// (auto-provisioning the tenant) when absent. Race-safe: a concurrent
    /// create that loses the UNIQUE(app_id) race surfaces as `AlreadyExists`,
    /// which we resolve by re-reading the winner. Only the account row is
    /// created — no `vk_` is minted.
    pub async fn ensure_account(&self, app_id: &str) -> Result<Account> {
        if let Some(acc) = self.lookup_active_account(app_id).await? {
            return Ok(acc);
        }
        let now = Utc::now();
        let account = Account {
            id: Uuid::new_v4().to_string(),
            name: format!("app-{app_id}"),
            email: None,
            password_hash: None,
            app_id: Some(app_id.to_string()),
            status: AccountStatus::Active,
            created_at: now,
            updated_at: now,
        };
        match self.auth.create_account(&account).await {
            Ok(()) => Ok(account),
            // Lost the race against a concurrent first-touch of the same
            // workspace; the winner's row now exists — read it back.
            Err(VedaError::AlreadyExists(_)) => {
                self.lookup_active_account(app_id).await?.ok_or_else(|| {
                    VedaError::Internal("app_id account vanished after duplicate".into())
                })
            }
            Err(e) => Err(e),
        }
    }
}
