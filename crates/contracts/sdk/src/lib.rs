//! The only place where the core declares what it expects from external
//! packages. Everything in `packages/` depends on this crate and on
//! `protocol` alone — never on another package.

mod capability;
mod command;
mod ports;
mod error;
mod gateway;
mod mcp;
mod plugin;
mod runner;
mod session;
mod sinks;
mod skill;
mod tool;

pub use capability::Capability;
pub use command::{CommandContext, SlashCommandHandler};
pub use ports::{
    DefaultModel, PermissionCheck, Scope, SystemPrompt, ToolExecutor, ToolPresentation, Tools,
};
pub use error::Error;
pub use gateway::{Chunk, ModelGateway, ModelInfo, ProviderInfo, Stream};
pub use mcp::McpProvider;
pub use plugin::{
    Config, Contribution, Ctx, Disposer, EffectStore, FunctionPlugin, InjectCtx,
    PluginForm, PluginManifest, RegistrationDecl, RegistrationHandle, ResolverManifest,
    Service,
};
pub use runner::{AgentRunner, TurnContext, TurnControl};
pub use session::SessionStore;
pub use sinks::{ConfigStore, EventSink, FsPort, SecretVault, WidgetSink};
pub use skill::{SkillDescriptor, SkillProvider};
pub use tool::{Tool, ToolContext, ToolOutput};

/// Canonical service keys. Runtime and packages must use these instead of
/// inline strings when resolving a registered service.
pub mod service_keys {
    pub const TOOL_PRESENTATION: &str = "agent-tool-presentation";
    pub const DEFAULT_MODEL: &str = "agent-default-model";
    pub const SYSTEM_PROMPT: &str = "system-prompt";
    pub const SCOPE: &str = "scope";
    pub const SESSION: &str = "session";
    pub const NATIVE_EXECUTOR: &str = "native";
    pub const PTC_EXECUTOR: &str = "ptc";
    pub const TOOLS: &str = "tools";
    pub const TOOL_PERMISSION: &str = "tool-permission";
    pub const REACT_RUNNER: &str = "react";
    pub const SECRET_VAULT: &str = "secrets";
    pub const CONFIG_STORE: &str = "config";
}

pub use service_keys::{
    DEFAULT_MODEL, NATIVE_EXECUTOR, PTC_EXECUTOR, REACT_RUNNER, SCOPE, SESSION,
    CONFIG_STORE, SECRET_VAULT, SYSTEM_PROMPT, TOOLS, TOOL_PERMISSION, TOOL_PRESENTATION,
};

pub type Result<T> = std::result::Result<T, Error>;
