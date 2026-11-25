//! Plugin topology, resolver manifest and registration-as-effect.
//!
//! A package is ONE of two mutually exclusive forms:
//! - `Service`: a single default-exported service class (`impl Service`).
//! - `FunctionPlugin`: named exports `NAME`, `inject`, `Config`, `apply`.
//! Mixing the forms makes the Loader discard the function plugin's namespace.

use crate::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Resolver manifest — the single source of truth for what a package exposes.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RegistrationDecl {
    Service { name: String },
    Tool { name: String },
    Command { name: String },
    Mcp { name: String },
    Skill { name: String },
    Widget { id: String },
    Capability { name: String },
    Hook { event: String },
    Gateway { name: String },
    Runner { name: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolverManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub form: PluginForm,
    /// Every contribution this package may make. Undeclared effects are rejected.
    pub registrations: Vec<RegistrationDecl>,
    pub config_schema: Option<Value>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginForm {
    Service,
    Function,
}

pub trait PluginManifest {
    fn manifest() -> ResolverManifest;
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Config(pub Value);

impl Config {
    pub fn new(value: Value) -> Self { Self(value) }
    pub fn get(&self, key: &str) -> Option<&Value> { self.0.get(key) }
}

// ---------------------------------------------------------------------------
// Ctx: effects, hooks, declared injections, global lookup.
// ---------------------------------------------------------------------------

pub struct RegistrationHandle {
    undo: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl RegistrationHandle {
    pub fn new(undo: impl FnOnce() + Send + Sync + 'static) -> Self {
        Self { undo: Some(Box::new(undo)) }
    }
}

impl Drop for RegistrationHandle {
    fn drop(&mut self) {
        if let Some(undo) = self.undo.take() { undo(); }
    }
}

#[derive(Default)]
pub struct Disposer {
    handles: Vec<RegistrationHandle>,
}

impl Disposer {
    pub fn push(&mut self, handle: RegistrationHandle) { self.handles.push(handle); }
    pub fn add(&mut self, handles: impl IntoIterator<Item = RegistrationHandle>) {
        self.handles.extend(handles);
    }
}

/// Contributions are effects, never direct struct mutation.
pub enum Contribution {
    Service { name: String, service: Arc<dyn Any + Send + Sync> },
    Tool { tool: Box<dyn crate::Tool> },
    Command { handler: Box<dyn crate::SlashCommandHandler> },
    Widget { widget: protocol::WidgetRow },
    Capability { capability: Box<dyn crate::Capability> },
    Mcp { provider: Box<dyn crate::McpProvider> },
    Skill { provider: Box<dyn crate::SkillProvider> },
    Hook { event: &'static str, handler: Box<dyn Fn(&dyn Any) + Send + Sync> },
    Gateway { gateway: Box<dyn crate::ModelGateway> },
    Runner { runner: Box<dyn crate::AgentRunner> },
}

pub trait EffectStore: Send + Sync {
    fn apply(&self, plugin_id: &str, contribution: Contribution) -> Result<RegistrationHandle>;
    fn subscribe(
        &self,
        plugin_id: &str,
        event: &'static str,
        handler: Box<dyn Fn(&dyn Any) + Send + Sync>,
    ) -> Result<RegistrationHandle>;
}

pub trait Ctx: Send + Sync {
    fn effect(&mut self, contribution: Contribution) -> Result<RegistrationHandle>;
    fn on(&mut self, event: &'static str, handler: Box<dyn Fn(&dyn Any) + Send + Sync>)
        -> Result<RegistrationHandle>;
    fn get(&self, name: &str) -> Option<Arc<dyn Any + Send + Sync>>;
    fn declared(&self, name: &str) -> Arc<dyn Any + Send + Sync>;
}

#[derive(Default)]
pub struct InjectCtx<'a> {
    injections: Vec<&'a str>,
    config: HashMap<&'a str, Value>,
}

impl<'a> InjectCtx<'a> {
    pub fn inject(&mut self, name: &'a str) -> Arc<dyn Any + Send + Sync> {
        self.injections.push(name);
        Arc::new(())
    }
    pub fn declare(&mut self, name: &'a str, value: Value) { self.config.insert(name, value); }
    pub fn injections(&self) -> &[&'a str] { &self.injections }
}

// ---------------------------------------------------------------------------
// The two legal package forms.
// ---------------------------------------------------------------------------

pub trait Service: PluginManifest + Send + Sync {
    fn build(config: &Config, ctx: &InjectCtx) -> Result<Self> where Self: Sized;
}

pub trait FunctionPlugin: Send + Sync {
    fn name(&self) -> &str;
    fn registrations(&self) -> Vec<RegistrationDecl> { Vec::new() }
    fn apply(&self, config: &Config, ctx: &mut dyn Ctx) -> Result<Disposer>;
}
