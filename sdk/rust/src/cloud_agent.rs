//! Cloud managed agent sessions, backed by the smolfleet `/v1/agents` API.

use smol_cloud::blocking::{AgentEvents, Client};
use smol_cloud::types::{Agent, AgentPage, CreateAgent, SendAgentTurn};

use crate::{ConnectOptions, Error, ErrorKind, Machine, Result, Target};

/// A handle to one cloud managed agent session.
#[derive(Clone)]
pub struct CloudAgentSession {
    client: Client,
    name: String,
}

impl CloudAgentSession {
    /// Create a session. Harness setup continues asynchronously on the cloud.
    pub fn create(request: &CreateAgent, connect: &ConnectOptions) -> Result<Self> {
        let client = cloud_client(connect)?;
        let info = client.create_agent(request)?;
        Ok(Self {
            client,
            name: info.name,
        })
    }

    /// Attach to an existing session by name, checking that it exists.
    pub fn connect(name: &str, connect: &ConnectOptions) -> Result<Self> {
        let client = cloud_client(connect)?;
        client.agent(name)?;
        Ok(Self {
            client,
            name: name.to_string(),
        })
    }

    /// List a page of sessions in this account.
    pub fn list(
        connect: &ConnectOptions,
        after: Option<&str>,
        limit: Option<u32>,
    ) -> Result<AgentPage> {
        Ok(cloud_client(connect)?.agents(after, limit)?)
    }

    /// Session name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Fetch the latest state and turn history.
    pub fn info(&self) -> Result<Agent> {
        Ok(self.client.agent(&self.name)?)
    }

    /// Attach to the current machine to stage files or inspect the workspace.
    pub fn machine(&self) -> Result<Machine> {
        let info = self.info()?;
        let id = info.machine_id.ok_or_else(|| {
            Error::new(ErrorKind::InvalidState, "agent session has no machine yet")
        })?;
        let connect = ConnectOptions::with_api_key(self.client.credentials().api_key())
            .base_url(self.client.credentials().base_url());
        Machine::connect_with(&id, &connect)
    }

    /// Submit one turn. A repeated idempotency key with the same input returns the same index.
    pub fn send(&self, request: &SendAgentTurn, idempotency_key: Option<&str>) -> Result<u64> {
        Ok(self
            .client
            .send_agent_turn(&self.name, request, idempotency_key)?)
    }

    /// Replay and follow a turn. Pass the last event id as `after` on reconnect.
    pub fn events(&self, turn: u64, after: Option<u64>) -> Result<AgentEvents> {
        Ok(self.client.agent_events(&self.name, turn, after)?)
    }

    /// Stop a running turn's machine and mark the turn cancelled.
    pub fn cancel(&self, turn: u64) -> Result<()> {
        Ok(self.client.cancel_agent_turn(&self.name, turn)?)
    }

    /// Restore the session to the state after a checkpointed turn.
    pub fn rewind(&self, turn: u64) -> Result<Agent> {
        Ok(self.client.rewind_agent(&self.name, turn)?)
    }

    /// Branch a new session from a checkpointed turn.
    pub fn branch(&self, turn: u64, name: &str) -> Result<Self> {
        let info = self.client.branch_agent(&self.name, turn, name)?;
        Ok(Self {
            client: self.client.clone(),
            name: info.name,
        })
    }

    /// Compatibility alias for [`Self::branch`].
    pub fn fork(&self, turn: u64, name: &str) -> Result<Self> {
        self.branch(turn, name)
    }

    /// Pause an idle session's machine.
    pub fn pause(&self) -> Result<()> {
        Ok(self.client.pause_agent(&self.name)?)
    }

    /// Resume an idle session's machine.
    pub fn resume(&self) -> Result<()> {
        Ok(self.client.resume_agent(&self.name)?)
    }

    /// Delete the session and its unshared resources.
    pub fn delete(self) -> Result<()> {
        Ok(self.client.delete_agent(&self.name)?)
    }
}

fn cloud_client(connect: &ConnectOptions) -> Result<Client> {
    if connect.target() != Target::Cloud {
        return Err(Error::new(
            ErrorKind::NotSupported,
            "managed agent sessions require ConnectOptions::cloud()",
        ));
    }
    connect.client()
}
