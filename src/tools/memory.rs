use crate::error::GremlinError;
use crate::memory::Memory;
use serde_json::json;
use std::sync::Arc;

/// Register all memory-related tools in the tool registry
pub fn register_memory_tools(
    registry: &mut crate::tools::ToolRegistry,
    memory: Arc<Memory>,
) {
    // memory_fact — store or retrieve a fact
    {
        let mem = memory.clone();
        registry.register(
            "memory_fact",
            "Store or retrieve a learned fact about the user/project. Use for things like 'user prefers fish shell', 'project uses Rust 2021 edition', etc.",
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["get", "set", "list"], "description": "Operation: get (retrieve), set (store), or list (all facts)"},
                    "key": {"type": "string", "description": "Fact key (e.g. 'shell', 'editor', 'project_type')"},
                    "value": {"type": "string", "description": "Fact value (required for 'set')"},
                    "project": {"type": "string", "description": "Optional project scope (defaults to global)"},
                    "confidence": {"type": "number", "description": "Confidence 0.0-1.0 (default 0.8)"},
                    "source": {"type": "string", "description": "Source: 'conversation', 'explicit', 'observed' (default 'conversation')"}
                },
                "required": ["action"]
            }),
            Box::new(move |args| {
                let action = args["action"].as_str().ok_or_else(|| GremlinError::Tool("missing 'action'".into()))?;
                let project = args["project"].as_str();

                match action {
                    "get" => {
                        let key = args["key"].as_str().ok_or_else(|| GremlinError::Tool("missing 'key' for get".into()))?;
                        let fact = mem.get_fact(key, project)?;
                        match fact {
                            Some(f) => Ok(format!("Fact '{}': {} (confidence: {:.0}%, source: {})", f.key, f.value, f.confidence * 100.0, f.source)),
                            None => Ok(format!("No fact found for key '{}'", key)),
                        }
                    }
                    "set" => {
                        let key = args["key"].as_str().ok_or_else(|| GremlinError::Tool("missing 'key' for set".into()))?;
                        let value = args["value"].as_str().ok_or_else(|| GremlinError::Tool("missing 'value' for set".into()))?;
                        let confidence = args["confidence"].as_f64().unwrap_or(0.8) as f32;
                        let source = args["source"].as_str().unwrap_or("conversation");
                        let id = mem.upsert_fact(key, value, project, confidence, source)?;
                        Ok(format!("Stored fact '{}' = '{}' (id={})", key, value, id))
                    }
                    "list" => {
                        let facts = mem.list_facts(project)?;
                        if facts.is_empty() {
                            Ok("No facts stored.".into())
                        } else {
                            let lines: Vec<String> = facts.iter().map(|f| {
                                let proj = f.project.as_deref().unwrap_or("global");
                                format!("  [{}] {} = {} ({:.0}%, {})", proj, f.key, f.value, f.confidence * 100.0, f.source)
                            }).collect();
                            Ok(format!("Facts:\n{}", lines.join("\n")))
                        }
                    }
                    _ => Err(GremlinError::Tool(format!("unknown action '{}'", action))),
                }
            }),
        );
    }

    // memory_preference — store or retrieve a user preference
    {
        let mem = memory.clone();
        registry.register(
            "memory_preference",
            "Store or retrieve a user preference. Use for persistent settings like 'theme=dark', 'editor=vim', 'verbose=false'.",
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["get", "set", "list"], "description": "Operation: get, set, or list"},
                    "key": {"type": "string", "description": "Preference key"},
                    "value": {"type": "string", "description": "Preference value (required for 'set')"},
                    "project": {"type": "string", "description": "Optional project scope"},
                    "confidence": {"type": "number", "description": "Confidence 0.0-1.0 (default 0.9)"},
                    "source": {"type": "string", "description": "Source: 'explicit', 'inferred', 'observed' (default 'explicit')"}
                },
                "required": ["action"]
            }),
            Box::new(move |args| {
                let action = args["action"].as_str().ok_or_else(|| GremlinError::Tool("missing 'action'".into()))?;
                let project = args["project"].as_str();

                match action {
                    "get" => {
                        let key = args["key"].as_str().ok_or_else(|| GremlinError::Tool("missing 'key' for get".into()))?;
                        let pref = mem.get_preference(key, project)?;
                        match pref {
                            Some(p) => Ok(format!("Preference '{}': {} (confidence: {:.0}%, source: {})", p.key, p.value, p.confidence * 100.0, p.source)),
                            None => Ok(format!("No preference found for key '{}'", key)),
                        }
                    }
                    "set" => {
                        let key = args["key"].as_str().ok_or_else(|| GremlinError::Tool("missing 'key' for set".into()))?;
                        let value = args["value"].as_str().ok_or_else(|| GremlinError::Tool("missing 'value' for set".into()))?;
                        let confidence = args["confidence"].as_f64().unwrap_or(0.9) as f32;
                        let source = args["source"].as_str().unwrap_or("explicit");
                        let id = mem.upsert_preference(key, value, project, confidence, source)?;
                        Ok(format!("Stored preference '{}' = '{}' (id={})", key, value, id))
                    }
                    "list" => {
                        let prefs = mem.list_preferences(project)?;
                        if prefs.is_empty() {
                            Ok("No preferences stored.".into())
                        } else {
                            let lines: Vec<String> = prefs.iter().map(|p| {
                                let proj = p.project.as_deref().unwrap_or("global");
                                format!("  [{}] {} = {} ({:.0}%, {})", proj, p.key, p.value, p.confidence * 100.0, p.source)
                            }).collect();
                            Ok(format!("Preferences:\n{}", lines.join("\n")))
                        }
                    }
                    _ => Err(GremlinError::Tool(format!("unknown action '{}'", action))),
                }
            }),
        );
    }

    // memory_search — full-text search over conversation history
    {
        let mem = memory.clone();
        registry.register(
            "memory_search",
            "Search conversation history using full-text search (FTS5). Returns relevant past messages.",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search query (supports FTS5 syntax: 'term', 'term1 term2', '\"exact phrase\"', 'prefix*')"},
                    "project": {"type": "string", "description": "Optional project filter"},
                    "limit": {"type": "integer", "description": "Max results (default 10, max 50)"}
                },
                "required": ["query"]
            }),
            Box::new(move |args| {
                let query = args["query"].as_str().ok_or_else(|| GremlinError::Tool("missing 'query'".into()))?;
                let project = args["project"].as_str();
                let limit = args["limit"].as_u64().unwrap_or(10).min(50) as usize;
                let results = mem.search(query, project, limit)?;
                if results.is_empty() {
                    Ok(format!("No conversations found matching '{}'", query))
                } else {
                    let lines: Vec<String> = results.iter().map(|m| {
                        let proj = m.project.as_deref().unwrap_or("?");
                        format!("  [{}] {}: {}...", proj, m.role, m.content.chars().take(120).collect::<String>())
                    }).collect();
                    Ok(format!("Found {} result(s) for '{}':\n{}", results.len(), query, lines.join("\n")))
                }
            }),
        );
    }

    // memory_recommendation — add or list recommendations
    {
        let mem = memory.clone();
        registry.register(
            "memory_recommendation",
            "Add a recommendation for future reference, or list pending actions. Use for 'try this tool', 'refactor that module', etc.",
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["add", "list", "dismiss"], "description": "Operation"},
                    "kind": {"type": "string", "description": "Kind: 'command', 'tool', 'workflow', 'code_change' (required for 'add')"},
                    "title": {"type": "string", "description": "Short title (required for 'add')"},
                    "description": {"type": "string", "description": "Detailed description (required for 'add')"},
                    "confidence": {"type": "number", "description": "Confidence 0.0-1.0 (default 0.7)"},
                    "context": {"type": "string", "description": "JSON context that triggered this (optional)"},
                    "action_json": {"type": "string", "description": "Tool call JSON if actionable (optional)"},
                    "id": {"type": "integer", "description": "Recommendation ID (required for 'dismiss')"}
                },
                "required": ["action"]
            }),
            Box::new(move |args| {
                let action = args["action"].as_str().ok_or_else(|| GremlinError::Tool("missing 'action'".into()))?;

                match action {
                    "add" => {
                        let kind = args["kind"].as_str().ok_or_else(|| GremlinError::Tool("missing 'kind'".into()))?;
                        let title = args["title"].as_str().ok_or_else(|| GremlinError::Tool("missing 'title'".into()))?;
                        let description = args["description"].as_str().ok_or_else(|| GremlinError::Tool("missing 'description'".into()))?;
                        let confidence = args["confidence"].as_f64().unwrap_or(0.7) as f32;
                        let context = args["context"].as_str().unwrap_or("{}").to_string();
                        let action_json = args["action_json"].as_str().map(|s| s.to_string());

                        let rec = crate::memory::Recommendation {
                            id: 0,
                            kind: kind.to_string(),
                            title: title.to_string(),
                            description: description.to_string(),
                            confidence,
                            context,
                            action: action_json,
                            created_at: chrono::Utc::now().to_rfc3339(),
                            dismissed: false,
                        };
                        let id = mem.add_recommendation(&rec)?;
                        Ok(format!("Added recommendation #{} ({})", id, title))
                    }
                    "list" => {
                        let recs = mem.get_recommendations(false, 20)?;
                        if recs.is_empty() {
                            Ok("No pending recommendations.".into())
                        } else {
                            let lines: Vec<String> = recs.iter().map(|r| {
                                format!("  #{} [{}] {} — {} ({:.0}%)", r.id, r.kind, r.title, r.description, r.confidence * 100.0)
                            }).collect();
                            Ok(format!("Recommendations:\n{}", lines.join("\n")))
                        }
                    }
                    "dismiss" => {
                        let id = args["id"].as_i64().ok_or_else(|| GremlinError::Tool("missing 'id' for dismiss".into()))?;
                        mem.dismiss_recommendation(id)?;
                        Ok(format!("Dismissed recommendation #{}", id))
                    }
                    _ => Err(GremlinError::Tool(format!("unknown action '{}'", action))),
                }
            }),
        );
    }

    // memory_stats — debug stats
    {
        let mem = memory.clone();
        registry.register(
            "memory_stats",
            "Show memory system statistics (DB size, conversation count, facts, preferences, recommendations).",
            json!({"type": "object", "properties": {}}),
            Box::new(move |_args| {
                let stats = mem.stats()?;
                Ok(format!(
                    "Memory Stats:\n  Conversations: {}\n  Facts: {}\n  Preferences: {}\n  Recommendations (active): {}\n  Proposed modifications: {}\n  DB size: {:.1} KB",
                    stats.conversations, stats.facts, stats.preferences,
                    stats.recommendations, stats.proposed_modifications,
                    stats.db_size_bytes as f64 / 1024.0
                ))
            }),
        );
    }
}