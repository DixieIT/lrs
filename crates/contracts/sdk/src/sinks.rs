//! Handles the core passes to components so they can produce effects without
//! knowing the host infrastructure.

use crate::{Error, Result};
use protocol::{Event, WidgetRow};
use std::path::Path;

pub trait EventSink: Send + Sync {
    fn emit(&self, event: Event);
}

pub trait WidgetSink: Send + Sync {
    fn upsert(&self, widget: WidgetRow);
    fn remove(&self, session_id: &str, widget_id: &str);
}

pub trait SecretVault: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<String>>;
    fn set(&self, key: &str, value: &str) -> Result<()>;
}

pub trait ConfigStore: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<String>>;
    fn set(&self, key: &str, value: &str) -> Result<()>;
}

/// Abstract filesystem access exposed through `ToolContext`. Implemented by the
/// runtime (or by a dedicated fs component registered as a port), so no tool
/// needs to import a sibling component to read a file.
pub trait FsPort: Send + Sync {
    fn read(&self, _path: &Path) -> Result<String> {
        Err(Error::PermissionDenied("read not implemented".into()))
    }
    fn write(&self, _path: &Path, _content: &str) -> Result<()> {
        Err(Error::PermissionDenied("write not implemented".into()))
    }
    /// Entries of a directory, sorted; directories end with `/`.
    fn list(&self, _path: &Path) -> Result<Vec<String>> {
        Err(Error::PermissionDenied("list not implemented".into()))
    }
}
