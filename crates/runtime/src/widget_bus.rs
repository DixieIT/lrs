//! Per-session widget state; implements the SDK `WidgetSink`.

use protocol::WidgetRow;
use sdk::WidgetSink;
use std::collections::HashMap;
use std::sync::RwLock;

#[derive(Default)]
pub struct WidgetBus {
    rows: RwLock<HashMap<String, HashMap<String, WidgetRow>>>,
}

impl WidgetBus {
    pub fn for_session(&self, session_id: &str) -> Vec<WidgetRow> {
        self.rows
            .read()
            .unwrap()
            .get(session_id)
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default()
    }
}

impl WidgetSink for WidgetBus {
    fn upsert(&self, widget: WidgetRow) {
        let mut rows = self.rows.write().unwrap();
        rows.entry(widget.session_id.clone())
            .or_default()
            .insert(widget.id.clone(), widget);
    }

    fn remove(&self, session_id: &str, widget_id: &str) {
        if let Some(m) = self.rows.write().unwrap().get_mut(session_id) {
            m.remove(widget_id);
        }
    }
}
