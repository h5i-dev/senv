//! The h5i-app type bindings for Echo.
//!
//! Sessions execute on [`h5i_app::MemoryEngine`]. `EchoStore` exists because
//! [`h5i_app::http::H5iApp`] is generic over a [`h5i_app::pg::Store`]. Its
//! PostgreSQL methods are refused: this server does not open a database.

use axum::http::HeaderMap;
use h5i_app::http::{AuthError, Authenticator};
use h5i_app::pg::{DbError, Store, Tx};
use h5i_app::{Kernel, TenantId};

use crate::kernel::{self, Command, EchoError, Principal, Reply, Snapshot};

pub struct EchoStore;

impl Kernel for kernel::EchoKernel {
    type Principal = Principal;
    type Snapshot = Snapshot;
    type Command = Command;
    type WriteSet = Snapshot;
    type Reply = Reply;
    type Error = EchoError;

    fn tenant(actor: &Principal) -> TenantId {
        TenantId(actor.tenant)
    }

    fn transition(
        actor: &Principal,
        snap: &Snapshot,
        cmd: &Command,
    ) -> Result<(Snapshot, Reply), EchoError> {
        kernel::transition(actor, snap, cmd)
    }

    fn apply(snap: &Snapshot, ws: &Snapshot) -> Snapshot {
        kernel::apply(snap, ws)
    }

    fn read_only(cmd: &Command) -> bool {
        matches!(cmd, Command::State)
    }
}

impl Store<kernel::EchoKernel> for EchoStore {
    fn ddl() -> Vec<String> {
        Vec::new()
    }

    fn tables() -> Vec<&'static str> {
        Vec::new()
    }

    async fn load(_tx: &Tx<'_>, _tenant: TenantId) -> Result<Snapshot, DbError> {
        Err(DbError::Decode(
            "echo sessions execute on MemoryEngine; there is no PostgreSQL snapshot".into(),
        ))
    }

    async fn write(_tx: &Tx<'_>, _tenant: TenantId, _ws: &Snapshot) -> Result<(), DbError> {
        Err(DbError::Decode(
            "echo sessions execute on MemoryEngine; there is no PostgreSQL write".into(),
        ))
    }
}

/// OpenEnv clients do not send h5i bearer tokens. The session id, not this
/// principal, is the isolation key, and each session has its own engine.
pub struct AnonAuth;

impl Authenticator<kernel::EchoKernel> for AnonAuth {
    fn authenticate(&self, _headers: &HeaderMap) -> Result<Principal, AuthError> {
        Ok(Principal {
            tenant: kernel::SESSION_TENANT,
        })
    }
}
