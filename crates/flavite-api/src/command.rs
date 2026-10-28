//! Command specification and registry (§4.5, D16).
//!
//! This module provides the typed command system for the API surface.

use crate::types::{Origin, Preconditions, RequestId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::RwLock;

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
        }
    }
}

/// Argument schema for a command.
#[derive(Debug, Clone)]
pub struct ArgSchema;

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
        Self {
            request_id,
            command_id,
            args,
            origin,
            preconditions: Preconditions::new(),
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
        Self {
            request_id,
            command_id,
            args,
            origin,
            preconditions,
        }
    }
}

/// The command registry.
#[derive(Debug, Default)]
pub struct CommandRegistry {
    /// Map from command ID to specification.
    specs: RwLock<HashMap<&'static str, CommandSpec>>,
    /// Next request ID counter.
    next_request_id: std::sync::atomic::AtomicU64,
}

impl CommandRegistry {
    /// Create a new command registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            specs: RwLock::new(HashMap::new()),
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
    pub fn register(&self, spec: CommandSpec) -> Result<(), CommandRegistrationError> {
        let id = spec.id;

        {
            let specs = self.specs.read().unwrap();
            if specs.contains_key(id) {
                return Err(CommandRegistrationError::DuplicateId(id));
            }
        }

        {
            let mut specs = self.specs.write().unwrap();
            specs.insert(id, spec);
        }

        Ok(())
    }

    /// Get a command specification.
    #[must_use]
    pub fn get_spec(&self, id: &str) -> Option<CommandSpec> {
        let specs = self.specs.read().unwrap();
        specs.get(id).cloned()
    }

    /// List all command IDs.
    #[must_use]
    pub fn command_ids(&self) -> Vec<&'static str> {
        let specs = self.specs.read().unwrap();
        let mut ids = specs.keys().copied().collect::<Vec<_>>();
        ids.sort_unstable();
        ids
    }

    /// Return the registered command specifications in stable display order.
    #[must_use]
    pub fn specs(&self) -> Vec<CommandSpec> {
        let specs = self.specs.read().unwrap();
        let mut values = specs.values().cloned().collect::<Vec<_>>();
        values.sort_by_key(|spec| spec.id);
        values
    }
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
        let spec = CommandSpec::new("test.cmd", "Test", ArgSchema);

        registry.register(spec).unwrap();
        assert!(registry.command_ids().contains(&"test.cmd"));
    }

    #[test]
    fn duplicate_id_fails() {
        let registry = CommandRegistry::new();
        let spec = CommandSpec::new("test.cmd", "Test", ArgSchema);

        registry.register(spec).unwrap();
        let result = registry.register(CommandSpec::new("test.cmd", "Test2", ArgSchema));
        assert!(matches!(
            result,
            Err(CommandRegistrationError::DuplicateId(_))
        ));
    }
}
