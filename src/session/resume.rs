//! Session resume functionality.

use crate::events::{Event, Message};
use crate::session::header::{SessionError, SessionHeader};
use crate::session::log::SessionManager;
use crate::tool::todo::{TodoItem, TodoList};

/// Restored session state for resume.
#[derive(Debug, Clone)]
pub struct RestoredSession {
    /// The session header.
    pub header: SessionHeader,
    /// Restored conversation messages.
    pub messages: Vec<Message>,
    /// Restored todo list.
    pub todos: TodoList,
    /// Any warnings from loading (corrupted lines, etc.).
    pub warnings: Vec<String>,
}

/// Restore a session from its log.
///
/// Replays events to reconstruct conversation history and todo state.
pub fn restore_session(
    manager: &SessionManager,
    session_id: &str,
) -> Result<RestoredSession, SessionError> {
    let (header, events, warnings) = manager.load_session(session_id)?;

    // Replay events to restore state using the proper replay function
    let (messages, todos) = replay_events(&events);

    Ok(RestoredSession {
        header,
        messages,
        todos,
        warnings,
    })
}

/// Replay events into a conversation and todo list.
///
/// This is the proper way to restore state for resume.
pub fn replay_events(events: &[Event]) -> (Vec<Message>, TodoList) {
    let mut messages = Vec::new();
    let todos = TodoList::new();

    for event in events {
        match event {
            Event::Message(msg) => {
                messages.push(msg.clone());
            }
            Event::ToolResult(result) => {
                // Handle todo tool results to rebuild todo list
                if result.name == "todo"
                    && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&result.output)
                {
                    // Check the action from the tool args to determine how to handle
                    let action = result
                        .args
                        .get("action")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");

                    if let Some(arr) = parsed.as_array() {
                        // This is likely a `list` result - replace entire todo list
                        // Clear existing and add all items from the list
                        let items: Vec<TodoItem> = arr
                            .iter()
                            .filter_map(|v| serde_json::from_value::<TodoItem>(v.clone()).ok())
                            .collect();
                        // Rebuild todo list from scratch using the list result
                        rebuild_todo_list(&todos, &items);
                    } else if let Ok(item) = serde_json::from_value::<TodoItem>(parsed) {
                        // Single item result - could be `add` or `complete`
                        if action == "complete" || item.done {
                            let _ = todos.complete(&item.id);
                        } else if action == "add" {
                            // Only add if not already present (by ID)
                            if todos.get(&item.id).is_none() {
                                todos.add(item.title);
                            }
                        } else {
                            // Default: add if not present
                            if todos.get(&item.id).is_none() {
                                todos.add(item.title);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    (messages, todos)
}

/// Rebuild the entire todo list from a list of items.
fn rebuild_todo_list(todos: &TodoList, items: &[TodoItem]) {
    // Get current items
    let current = todos.list();

    // Complete items that are done in the new list but not in current
    for item in items {
        if item.done {
            // If item exists in current but not done, complete it
            if let Some(existing) = current.iter().find(|e| e.id == item.id) {
                if !existing.done {
                    let _ = todos.complete(&item.id);
                }
            } else {
                // Item doesn't exist but is marked done - add then complete
                // This shouldn't happen normally, but handle gracefully
                let _ = todos.add(item.title.clone());
                let _ = todos.complete(&item.id);
            }
        } else {
            // Item is not done - add if not present
            if current.iter().all(|e| e.id != item.id) {
                todos.add(item.title.clone());
            }
        }
    }

    // Note: We don't remove items that are in current but not in the new list
    // because the list result should be authoritative. In practice, the list
    // result should contain all items.
}
