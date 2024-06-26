use async_trait::async_trait;
use snafu::ResultExt;
use spi::query::execution::{Output, QueryStateMachineRef};
use spi::query::logical_planner::CopyVnode;
use spi::{CoordinatorSnafu, QueryResult};

use super::DDLDefinitionTask;

pub struct CopyVnodeTask {
    stmt: CopyVnode,
}

impl CopyVnodeTask {
    #[inline(always)]
    pub fn new(stmt: CopyVnode) -> Self {
        Self { stmt }
    }
}

#[async_trait]
impl DDLDefinitionTask for CopyVnodeTask {
    async fn execute(&self, query_state_machine: QueryStateMachineRef) -> QueryResult<Output> {
        let (vnode_id, node_id) = (self.stmt.vnode_id, self.stmt.node_id);
        let tenant = query_state_machine.session.tenant();

        let meta = query_state_machine.meta.clone();
        let coord = query_state_machine.coord.clone();
        let vnode_all_info = coordinator::get_vnode_all_info(meta, tenant, vnode_id)
            .await
            .context(CoordinatorSnafu)?;

        let replica_id = vnode_all_info.repl_set_id;
        let cmd_type = coordinator::ReplicationCmdType::AddRaftFollower(replica_id, node_id);
        coord
            .replication_manager(tenant, cmd_type)
            .await
            .context(CoordinatorSnafu)?;

        Ok(Output::Nil(()))
    }
}
