//! Preserves captured floor, table and server configuration with pure validation.
//!
//! Table and server IDs occupy separate fields but remain strings, matching this
//! example's server identity. They do not provide the original typed-ID distinction.
//! Parsing, startup wiring and inactivity policy remain outside this module.
//! Validation preserves input order and returns the first failure without mutation.

use std::{collections::HashSet, fmt, time::Duration};

/// Retains table groups without flattening their ownership or metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FloorConfig {
    /// Lists tables in validation order; an empty floor is allowed.
    pub tables: Vec<TableConfig>,
}

impl FloorConfig {
    /// Validates table contents and floor-wide identity uniqueness.
    ///
    /// # Errors
    /// Returns a duplicate table ID before validating that table. Each table is
    /// fully validated before checking its server IDs against preceding tables.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut table_ids = HashSet::new();
        let mut server_ids = HashSet::new();

        for table in &self.tables {
            if !table_ids.insert(&table.id) {
                return Err(ConfigError::DuplicateTableId(table.id.clone()));
            }
            table.validate()?;
            for server in &table.servers {
                if !server_ids.insert(&server.id) {
                    return Err(ConfigError::DuplicateServerId(server.id.clone()));
                }
            }
        }

        Ok(())
    }
}

/// Retains servers and their table-level inactivity setting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableConfig {
    /// Identifies this table independently of server IDs.
    pub id: String,
    /// Lists servers in validation order.
    pub servers: Vec<ServerConfig>,
    /// Retains the inactivity duration; this module does not enforce shutdown.
    pub inactivity_timeout: Duration,
}

impl TableConfig {
    /// Validates table shape, server identities and each server's ports.
    ///
    /// # Errors
    /// Rejects an empty table, then a zero timeout, then each duplicate server
    /// ID or invalid server configuration in input order.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.servers.is_empty() {
            return Err(ConfigError::EmptyTable(self.id.clone()));
        }
        if self.inactivity_timeout.is_zero() {
            return Err(ConfigError::ZeroInactivityTimeout(self.id.clone()));
        }

        let mut ids = HashSet::new();
        for server in &self.servers {
            if !ids.insert(&server.id) {
                return Err(ConfigError::DuplicateServerIdInTable {
                    table_id: self.id.clone(),
                    server_id: server.id.clone(),
                });
            }
            server.validate(&self.id)?;
        }

        Ok(())
    }
}

/// Retains one server's proxy binding configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerConfig {
    /// Identifies the server using this example's string identity.
    pub id: String,
    /// Retains the proxy bind host without parsing or network validation.
    pub proxy_host: String,
    /// Lists Java ports; an empty list is allowed.
    pub java_ports: Vec<u16>,
    /// Lists Bedrock ports; an empty list is allowed.
    pub bedrock_ports: Vec<u16>,
}

impl ServerConfig {
    /// Validates this server's ports with table context for diagnostics.
    ///
    /// # Errors
    /// Rejects zero or repeated ports, scanning Java before Bedrock. Repeated
    /// ports across distinct servers remain allowed, as in the captured source.
    pub fn validate(&self, table_id: &str) -> Result<(), ConfigError> {
        let mut seen = HashSet::new();
        for &port in self.java_ports.iter().chain(&self.bedrock_ports) {
            if port == 0 {
                return Err(ConfigError::InvalidPort {
                    table_id: table_id.into(),
                    server_id: self.id.clone(),
                    port,
                });
            }
            if !seen.insert(port) {
                return Err(ConfigError::DuplicatePort {
                    table_id: table_id.into(),
                    server_id: self.id.clone(),
                    port,
                });
            }
        }

        Ok(())
    }
}

/// Identifies a captured configuration validation failure and its context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigError {
    /// A floor contains the same table ID twice.
    DuplicateTableId(String),
    /// A table contains no servers.
    EmptyTable(String),
    /// A table has a zero inactivity duration.
    ZeroInactivityTimeout(String),
    /// Different tables contain the same server ID.
    DuplicateServerId(String),
    /// One table contains the same server ID twice.
    DuplicateServerIdInTable {
        /// Identifies the containing table.
        table_id: String,
        /// Identifies the repeated server.
        server_id: String,
    },
    /// A server contains port zero.
    InvalidPort {
        /// Identifies the containing table.
        table_id: String,
        /// Identifies the server.
        server_id: String,
        /// Records the rejected port.
        port: u16,
    },
    /// A server repeats a port within or across protocol lists.
    DuplicatePort {
        /// Identifies the containing table.
        table_id: String,
        /// Identifies the server.
        server_id: String,
        /// Records the repeated port.
        port: u16,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateTableId(id) => write!(f, "duplicate table id: {id}"),
            Self::EmptyTable(id) => write!(f, "table {id} has no servers"),
            Self::ZeroInactivityTimeout(id) => write!(f, "table {id} has inactivity_timeout=0"),
            Self::DuplicateServerId(id) => write!(f, "duplicate server id: {id}"),
            Self::DuplicateServerIdInTable {
                table_id,
                server_id,
            } => write!(
                f,
                "table {table_id} contains duplicate server id: {server_id}"
            ),
            Self::InvalidPort {
                table_id,
                server_id,
                port,
            } => write!(
                f,
                "table {table_id} server {server_id} has invalid port: {port}"
            ),
            Self::DuplicatePort {
                table_id,
                server_id,
                port,
            } => write!(
                f,
                "table {table_id} server {server_id} repeats port: {port}"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}
