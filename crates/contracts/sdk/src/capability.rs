//! The single extension unit: instructions + temporary tools + hooks.
//!
//! There is no "tool that contains instructions". A `Tool` is executable only.
//! A `Capability` is what a package contributes when it wants to change the
//! model's task context: it carries model-facing instructions, temporary tools
//! active only while it is enabled, and lifecycle hooks.

use crate::{ConfigStore, EventSink, Result, Tool, WidgetSink};

pub trait Capability: Send + Sync {
    /// Stable, model-visible name. No UI, transport or implementation vocabulary.
    fn name(&self) -> &str;

    /// Task-relevant system instructions injected while this capability is active.
    /// Text is pinned verbatim in snapshots.
    fn instructions(&self) -> &[String] {
        &[]
    }

    /// Tools active only while the capability is in use.
    fn tools(&self) -> Vec<Box<dyn Tool>> {
        Vec::new()
    }

    fn on_session_start(&self, _session_id: &str, _events: &dyn EventSink) -> Result<()> {
        Ok(())
    }
    fn on_before_model(&self, _session_id: &str, _events: &dyn EventSink) -> Result<()> {
        Ok(())
    }
    fn on_after_tool(&self, _session_id: &str, _tool: &str, _events: &dyn EventSink) -> Result<()> {
        Ok(())
    }
    fn on_turn_end(&self, _session_id: &str, _events: &dyn EventSink) -> Result<()> {
        Ok(())
    }

    fn widgets(&self) -> Option<&dyn WidgetSink> {
        None
    }
    fn config(&self) -> Option<&dyn ConfigStore> {
        None
    }
}
