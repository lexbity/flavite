//! Command specification and registry (§4.5, D16).
//!
//! This module provides the typed command system for the API surface.

use crate::capability::Capability;
use crate::types::{Origin, Preconditions, RequestId};
use flavite_vault::session::{Session, SessionResult};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::RwLock;

/// An erased, statically typed command handler registered with its schema.
pub type CommandHandler = fn(&Session, &Invocation) -> Result<SessionResult, String>;

/// A command specification defining what a command does.
#[derive(Debug, Clone)]
pub struct CommandSpec {
    /// Unique identifier for this command.
    pub id: &'static str,
    /// Human-readable title.
    pub title: &'static str,
    /// Argument schema.
    pub args: ArgSchema,
    /// Optional predicate for when this command is available.
    pub when: Option<CommandAvailability>,
    /// Optional keybindings.
    pub keys: &'static [Chord],
    /// Whether this command mutates vault or user data.
    pub requires_write: bool,
    /// Capability required before this handler may execute.
    pub required_capability: Option<Capability>,
}

impl CommandSpec {
    /// Create a new command specification.
    #[must_use]
    pub const fn new(id: &'static str, title: &'static str, args: ArgSchema) -> Self {
        Self {
            id,
            title,
            args,
            when: None,
            keys: &[],
            requires_write: false,
            required_capability: None,
        }
    }

    /// Create a new command specification with availability and keys.
    #[must_use]
    pub const fn with_availability(
        id: &'static str,
        title: &'static str,
        args: ArgSchema,
        when: Option<CommandAvailability>,
        keys: &'static [Chord],
    ) -> Self {
        Self {
            id,
            title,
            args,
            when,
            keys,
            requires_write: false,
            required_capability: None,
        }
    }

    /// Mark the command as mutating so read-only state is enforced by the
    /// registered contract rather than by a caller supplied prefix.
    #[must_use]
    pub const fn write(mut self) -> Self {
        self.requires_write = true;
        self
    }

    /// Require a declared capability at dispatch.
    #[must_use]
    pub fn requires(mut self, capability: Capability) -> Self {
        self.required_capability = Some(capability);
        self
    }
}

/// Argument schema for a command.
#[derive(Clone)]
pub struct ArgSchema {
    validate: fn(&serde_json::Value) -> Result<(), String>,
}

impl std::fmt::Debug for ArgSchema {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ArgSchema(..)")
    }
}

impl ArgSchema {
    /// Schema that validates arguments by deserializing them into a typed DTO.
    #[must_use]
    pub fn typed<T: DeserializeOwned>() -> Self {
        Self {
            validate: |value| {
                serde_json::from_value::<T>(value.clone())
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            },
        }
    }

    /// Validate an invocation's JSON arguments at the API boundary.
    pub fn validate(&self, value: &serde_json::Value) -> Result<(), String> {
        (self.validate)(value)
    }
}

/// A keyboard chord (combination of modifier and key).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Chord {
    /// Modifier keys (Ctrl, Shift, Alt, etc.).
    pub modifiers: u8,
    /// The key.
    pub key: String,
}

impl Chord {
    /// Create a new chord.
    #[must_use]
    pub fn new(modifiers: u8, key: &str) -> Self {
        Self {
            modifiers,
            key: key.to_string(),
        }
    }
}

/// Command availability predicate.
#[derive(Debug, Clone)]
pub enum CommandAvailability {
    /// Command is always available.
    Always,
    /// Command is available when a note is open.
    NoteOpen,
    /// Command is available when a buffer is dirty.
    BufferDirty,
    /// Command is available when a buffer is in conflict.
    BufferConflict,
    /// Command is available in read-only mode.
    ReadOnly,
}

/// A keyboard modifier mask.
pub mod modifiers {
    /// No modifier.
    pub const NONE: u8 = 0b0000;
    /// Shift key.
    pub const SHIFT: u8 = 0b0001;
    /// Control key (Ctrl on Windows/Linux, Command on macOS).
    pub const CTRL: u8 = 0b0010;
    /// Alt key (Option on macOS).
    pub const ALT: u8 = 0b0100;
    /// Super key (Windows key on Windows, Command on macOS).
    pub const SUPER: u8 = 0b1000;
}

/// An invocation of a command.
#[derive(Debug, Clone)]
pub struct Invocation {
    /// Unique request ID.
    pub request_id: RequestId,
    /// The command ID.
    pub command_id: &'static str,
    /// The arguments.
    pub args: serde_json::Value,
    /// The origin.
    pub origin: Origin,
    /// Preconditions.
    pub preconditions: Preconditions,
    /// Capabilities granted to this caller. GUI/CLI/internal callers receive
    /// the built-in host grants; plugin callers must provide explicit grants.
    pub capabilities: Vec<Capability>,
}

impl Invocation {
    /// Create a new invocation.
    #[must_use]
    pub fn new(
        request_id: RequestId,
        command_id: &'static str,
        args: serde_json::Value,
        origin: Origin,
    ) -> Self {
        let capabilities = match origin {
            Origin::Plugin => Vec::new(),
            _ => vec![
                Capability::ReadNotes,
                Capability::WriteNotes {
                    scope: crate::capability::WriteScope::Vault,
                },
            ],
        };
        Self {
            request_id,
            command_id,
            args,
            origin,
            preconditions: Preconditions::new(),
            capabilities,
        }
    }

    /// Create a new invocation with preconditions.
    #[must_use]
    pub fn with_preconditions(
        request_id: RequestId,
        command_id: &'static str,
        args: serde_json::Value,
        origin: Origin,
        preconditions: Preconditions,
    ) -> Self {
        let capabilities = match origin {
            Origin::Plugin => Vec::new(),
            _ => vec![
                Capability::ReadNotes,
                Capability::WriteNotes {
                    scope: crate::capability::WriteScope::Vault,
                },
            ],
        };
        Self {
            request_id,
            command_id,
            args,
            origin,
            preconditions,
            capabilities,
        }
    }

    /// Set the grants explicitly, as a future plugin host will do.
    #[must_use]
    pub fn with_capabilities(mut self, capabilities: Vec<Capability>) -> Self {
        self.capabilities = capabilities;
        self
    }
}

/// The command registry.
#[derive(Debug, Default)]
pub struct CommandRegistry {
    /// Specifications and handlers are one registration unit.
    commands: RwLock<HashMap<&'static str, RegisteredCommand>>,
    /// Next request ID counter.
    next_request_id: std::sync::atomic::AtomicU64,
}

#[derive(Debug)]
struct RegisteredCommand {
    spec: CommandSpec,
    handler: CommandHandler,
}

impl CommandRegistry {
    /// Create a new command registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            commands: RwLock::new(HashMap::new()),
            next_request_id: std::sync::atomic::AtomicU64::new(1),
        }
    }

    /// Generate a new request ID.
    #[must_use]
    pub fn next_request_id(&self) -> RequestId {
        self.next_request_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    }

    /// Register a command.
    pub fn register(
        &self,
        spec: CommandSpec,
        handler: CommandHandler,
    ) -> Result<(), CommandRegistrationError> {
        let id = spec.id;
        let mut commands = self.commands.write().unwrap();
        if commands.contains_key(id) {
            return Err(CommandRegistrationError::DuplicateId(id));
        }
        commands.insert(id, RegisteredCommand { spec, handler });
        Ok(())
    }

    /// Get a command specification.
    #[must_use]
    pub fn get_spec(&self, id: &str) -> Option<CommandSpec> {
        let commands = self.commands.read().unwrap();
        commands.get(id).map(|registered| registered.spec.clone())
    }

    /// Validate arguments against the registered command schema.
    pub fn validate(
        &self,
        id: &str,
        args: &serde_json::Value,
    ) -> Result<(), CommandValidationError> {
        let commands = self.commands.read().unwrap();
        let registered = commands
            .get(id)
            .ok_or_else(|| CommandValidationError::Unavailable(id.to_owned()))?;
        registered
            .spec
            .args
            .validate(args)
            .map_err(CommandValidationError::InvalidArgs)
    }

    /// Validate arguments and dispatch through the handler registered with the
    /// command specification. Validation finishes before the handler can run.
    pub fn dispatch(
        &self,
        session: &Session,
        invocation: &Invocation,
    ) -> Result<SessionResult, CommandDispatchError> {
        let commands = self.commands.read().unwrap();
        let registered = commands
            .get(invocation.command_id)
            .ok_or_else(|| CommandDispatchError::Unavailable(invocation.command_id.to_owned()))?;
        registered
            .spec
            .args
            .validate(&invocation.args)
            .map_err(CommandDispatchError::InvalidArgs)?;
        (registered.handler)(session, invocation).map_err(CommandDispatchError::Handler)
    }

    /// List all command IDs.
    #[must_use]
    pub fn command_ids(&self) -> Vec<&'static str> {
        let commands = self.commands.read().unwrap();
        let mut ids = commands.keys().copied().collect::<Vec<_>>();
        ids.sort_unstable();
        ids
    }

    /// Return the registered command specifications in stable display order.
    #[must_use]
    pub fn specs(&self) -> Vec<CommandSpec> {
        let commands = self.commands.read().unwrap();
        let mut values = commands
            .values()
            .map(|registered| registered.spec.clone())
            .collect::<Vec<_>>();
        values.sort_by_key(|spec| spec.id);
        values
    }
}

/// A command rejected before its handler can produce effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandValidationError {
    /// No handler/specification is registered for this ID.
    Unavailable(String),
    /// Arguments do not match the registered typed schema.
    InvalidArgs(String),
}

/// Failure to dispatch a registered command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandDispatchError {
    /// No command/handler was registered.
    Unavailable(String),
    /// Typed argument validation failed.
    InvalidArgs(String),
    /// Handler refused an otherwise valid request before side effects.
    Handler(String),
}

/// Error for command registration.
#[derive(Debug)]
pub enum CommandRegistrationError {
    /// Command ID already registered.
    DuplicateId(&'static str),
}

/// Result of command execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CommandResult {
    /// Success with data.
    Success(CommandSuccess),
    /// Command is pending.
    Pending { request_id: RequestId },
    /// Command failed.
    Error { message: String },
    /// Buffer conflict.
    Conflict { path: String },
    /// Action required.
    ActionRequired {
        code: String,
        data: Option<serde_json::Value>,
    },
}

/// Success data for a command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommandSuccess {
    /// Request ID.
    pub request_id: RequestId,
    /// Command ID.
    pub command_id: String,
    /// Result data.
    pub data: serde_json::Value,
}

impl CommandSuccess {
    /// Create a new success result.
    #[must_use]
    pub fn new(request_id: RequestId, command_id: &'static str, data: serde_json::Value) -> Self {
        Self {
            request_id,
            command_id: command_id.to_string(),
            data,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_basic() {
        let registry = CommandRegistry::new();
        let spec = CommandSpec::new("test.cmd", "Test", ArgSchema::typed::<serde_json::Value>());

        registry.register(spec, test_handler).unwrap();
        assert!(registry.command_ids().contains(&"test.cmd"));
    }

    #[test]
    fn duplicate_id_fails() {
        let registry = CommandRegistry::new();
        let spec = CommandSpec::new("test.cmd", "Test", ArgSchema::typed::<serde_json::Value>());

        registry.register(spec, test_handler).unwrap();
        let result = registry.register(
            CommandSpec::new("test.cmd", "Test2", ArgSchema::typed::<serde_json::Value>()),
            test_handler,
        );
        assert!(matches!(
            result,
            Err(CommandRegistrationError::DuplicateId(_))
        ));
    }

    fn test_handler(
        _session: &flavite_vault::session::Session,
        _invocation: &Invocation,
    ) -> Result<SessionResult, String> {
        Err("test handler".into())
    }
}
