//! A Skill is a discovered resource, not a live extension.
//!
//! `discover` returns descriptors; `load` turns one into a `Capability`, which
//! is the only thing that carries instructions and temporary tools.

use crate::{Capability, Result};

#[derive(Debug, Clone, PartialEq)]
pub struct SkillDescriptor {
    pub name: String,
    pub description: String,
    /// Where the resource was found. Not shown to the model.
    pub source: String,
}

pub trait SkillProvider: Send + Sync {
    fn id(&self) -> &str;
    fn discover(&self, cwd: &str) -> Result<Vec<SkillDescriptor>>;
    fn load(&self, descriptor: &SkillDescriptor) -> Result<Box<dyn Capability>>;
}
