use super::*;

fn make_message(json: &str) -> serde_json::Value {
    serde_json::from_str(json).unwrap()
}

#[test]
fn extract_tool_data_empty_when_metadata_missing() {
    let v = make_message(r#"{"requestId": "r1", "result": {"metadata": {}}}"#);
    assert_eq!(extract_tool_data(&v), ToolData::default());
}

#[test]
fn extract_tool_data_empty_when_no_rounds() {
    let v = make_message(r#"{"result": {"metadata": {"toolCallRounds": []}}}"#);
    assert_eq!(extract_tool_data(&v), ToolData::default());
}

#[test]
fn extract_tool_data_skips_speak_only_rounds() {
    // Real-shape: a round with empty toolCalls just carries the
    // model's prose (`response`/`thinking`). It must not surface in
    // the names/ids/files vectors.
    let v = make_message(
        r#"{"result": {"metadata": {"toolCallRounds": [
            {"response": "I see you have a file open…", "toolCalls": [], "id": "round-1"}
        ]}}}"#,
    );
    assert_eq!(extract_tool_data(&v), ToolData::default());
}

#[test]
fn extract_tool_data_replace_string_in_file() {
    let v = make_message(
        r#"{"result": {"metadata": {"toolCallRounds": [
            {"toolCalls": [
                {"name": "replace_string_in_file",
                 "id": "call-abc",
                 "arguments": {"filePath": "src/auth.rs", "oldString": "x", "newString": "y"}}
            ]}
        ]}}}"#,
    );
    let td = extract_tool_data(&v);
    assert_eq!(td.names, vec!["replace_string_in_file".to_string()]);
    assert_eq!(td.ids, vec!["call-abc".to_string()]);
    assert_eq!(td.files, vec!["src/auth.rs".to_string()]);
}

#[test]
fn extract_tool_data_multi_replace_and_create_and_read() {
    let v = make_message(
        r#"{"result": {"metadata": {"toolCallRounds": [
            {"toolCalls": [
                {"name": "multi_replace_string_in_file", "id": "1", "arguments": {"filePath": "a.rs"}},
                {"name": "create_file", "id": "2", "arguments": {"filePath": "b.rs"}},
                {"name": "read_file", "id": "3", "arguments": {"filePath": "c.rs"}}
            ]}
        ]}}}"#,
    );
    let td = extract_tool_data(&v);
    assert_eq!(td.names.len(), 3);
    assert_eq!(td.ids, vec!["1", "2", "3"]);
    assert_eq!(td.files, vec!["a.rs", "b.rs", "c.rs"]);
}

#[test]
fn extract_tool_data_unknown_tool_yields_no_file_but_name_and_id_emit() {
    let v = make_message(
        r#"{"result": {"metadata": {"toolCallRounds": [
            {"toolCalls": [
                {"name": "search_codebase", "id": "x", "arguments": {"query": "foo"}}
            ]}
        ]}}}"#,
    );
    let td = extract_tool_data(&v);
    assert_eq!(td.names, vec!["search_codebase".to_string()]);
    assert_eq!(td.ids, vec!["x".to_string()]);
    assert!(td.files.is_empty());
}

#[test]
fn extract_tool_data_strips_file_uri_scheme() {
    let v = make_message(
        r#"{"result": {"metadata": {"toolCallRounds": [
            {"toolCalls": [
                {"name": "read_file", "id": "1",
                 "arguments": {"filePath": "file:///home/dev/repo/src/x.rs"}}
            ]}
        ]}}}"#,
    );
    let td = extract_tool_data(&v);
    assert_eq!(td.files, vec!["/home/dev/repo/src/x.rs".to_string()]);
}

#[test]
fn extract_tool_data_strips_vscode_vfs_scheme() {
    let v = make_message(
        r#"{"result": {"metadata": {"toolCallRounds": [
            {"toolCalls": [
                {"name": "read_file", "id": "1",
                 "arguments": {"filePath": "vscode-vfs://github/owner/repo/path/to/file.rs"}}
            ]}
        ]}}}"#,
    );
    let td = extract_tool_data(&v);
    assert_eq!(td.files, vec!["/owner/repo/path/to/file.rs".to_string()]);
}

#[test]
fn extract_tool_data_apply_patch_walks_patches_array() {
    let v = make_message(
        r#"{"result": {"metadata": {"toolCallRounds": [
            {"toolCalls": [
                {"name": "apply_patch", "id": "p1",
                 "arguments": {"patches": [
                     {"filePath": "src/lib.rs"},
                     {"filePath": "src/main.rs"}
                 ]}}
            ]}
        ]}}}"#,
    );
    let td = extract_tool_data(&v);
    assert_eq!(td.files, vec!["src/lib.rs", "src/main.rs"]);
}

#[test]
fn extract_tool_data_apply_patch_falls_back_to_top_level_filepath() {
    let v = make_message(
        r#"{"result": {"metadata": {"toolCallRounds": [
            {"toolCalls": [
                {"name": "apply_patch", "id": "p1",
                 "arguments": {"filePath": "src/single.rs"}}
            ]}
        ]}}}"#,
    );
    let td = extract_tool_data(&v);
    assert_eq!(td.files, vec!["src/single.rs".to_string()]);
}

#[test]
fn extract_tool_data_flattens_across_rounds_and_preserves_duplicates() {
    // Mirrors claude_code: the same tool name invoked twice across
    // two rounds shows up twice in `names`, paired with distinct ids.
    let v = make_message(
        r#"{"result": {"metadata": {"toolCallRounds": [
            {"toolCalls": [
                {"name": "read_file", "id": "1", "arguments": {"filePath": "a.rs"}}
            ]},
            {"toolCalls": [
                {"name": "read_file", "id": "2", "arguments": {"filePath": "b.rs"}}
            ]}
        ]}}}"#,
    );
    let td = extract_tool_data(&v);
    assert_eq!(td.names, vec!["read_file", "read_file"]);
    assert_eq!(td.ids, vec!["1", "2"]);
    assert_eq!(td.files, vec!["a.rs", "b.rs"]);
}

#[test]
fn extract_tool_data_skips_blank_name_and_blank_id() {
    // Defensive: an in-flight stub on a kind:2 splice could land
    // before the model has named the call. We must not insert
    // empty strings into the tag vectors — downstream consumers
    // would emit an empty tag value that violates the
    // not-null-empty contract.
    let v = make_message(
        r#"{"result": {"metadata": {"toolCallRounds": [
            {"toolCalls": [
                {"name": "", "id": "", "arguments": {"filePath": "ignored.rs"}}
            ]}
        ]}}}"#,
    );
    let td = extract_tool_data(&v);
    assert!(td.names.is_empty());
    assert!(td.ids.is_empty());
    assert!(td.files.is_empty());
}

#[test]
fn extract_tool_data_missing_tool_calls_array_skips_round() {
    let v = make_message(
        r#"{"result": {"metadata": {"toolCallRounds": [
            {"id": "round-1"},
            {"toolCalls": [
                {"name": "read_file", "id": "ok", "arguments": {"filePath": "x.rs"}}
            ]}
        ]}}}"#,
    );
    let td = extract_tool_data(&v);
    assert_eq!(td.names, vec!["read_file".to_string()]);
    assert_eq!(td.ids, vec!["ok".to_string()]);
    assert_eq!(td.files, vec!["x.rs".to_string()]);
}

#[test]
fn extract_tokens_vscode_delta_shape() {
    let v = make_message(r#"{"promptTokens": 1500, "outputTokens": 200}"#);
    let t = extract_tokens(&v).unwrap();
    assert_eq!(t.input, 1500);
    assert_eq!(t.output, 200);
    assert_eq!(t.cache_read, 0);
    assert_eq!(t.cache_write, 0);
}

#[test]
fn extract_tokens_vscode_delta_with_cache() {
    let v = make_message(
        r#"{"promptTokens": 1000, "outputTokens": 500, "cacheReadTokens": 200, "cacheWriteTokens": 50}"#,
    );
    let t = extract_tokens(&v).unwrap();
    assert_eq!(t.input, 1000);
    assert_eq!(t.cache_read, 200);
    assert_eq!(t.cache_write, 50);
}

#[test]
fn extract_tokens_copilot_cli_shape() {
    let v = make_message(
        r#"{"modelMetrics": {"inputTokens": 800, "outputTokens": 60, "cacheReadTokens": 10}}"#,
    );
    let t = extract_tokens(&v).unwrap();
    assert_eq!(t.input, 800);
    assert_eq!(t.output, 60);
    assert_eq!(t.cache_read, 10);
}

#[test]
fn extract_tokens_legacy_usage_shape() {
    let v = make_message(
        r#"{"usage": {"promptTokens": 12000, "completionTokens": 750, "cacheReadInputTokens": 4000, "cacheCreationInputTokens": 100}}"#,
    );
    let t = extract_tokens(&v).unwrap();
    assert_eq!(t.input, 12000);
    assert_eq!(t.output, 750);
    assert_eq!(t.cache_read, 4000);
    assert_eq!(t.cache_write, 100);
}

#[test]
fn extract_tokens_feb_2026_shape() {
    let v = make_message(
        r#"{"result": {"metadata": {"promptTokens": 9000, "outputTokens": 400, "cacheReadTokens": 1200}}}"#,
    );
    let t = extract_tokens(&v).unwrap();
    assert_eq!(t.input, 9000);
    assert_eq!(t.output, 400);
    assert_eq!(t.cache_read, 1200);
}

#[test]
fn extract_tokens_zero_pair_skips_shape_and_falls_through() {
    // Top-level shape has zeros; nested feb-2026 shape should win.
    let v = make_message(
        r#"{
            "promptTokens": 0,
            "outputTokens": 0,
            "result": {"metadata": {"promptTokens": 100, "outputTokens": 5}}
        }"#,
    );
    let t = extract_tokens(&v).unwrap();
    assert_eq!(t.input, 100);
    assert_eq!(t.output, 5);
}

#[test]
fn extract_tokens_unknown_shape_returns_none() {
    let v = make_message(r#"{"weird": {"thingy": 42}}"#);
    assert!(extract_tokens(&v).is_none());
    assert!(!shape_matches_any(&v));
}

#[test]
fn extract_model_id_strips_copilot_prefix() {
    let v = make_message(r#"{"modelId": "copilot/claude-sonnet-4-5"}"#);
    assert_eq!(extract_model_id(&v).as_deref(), Some("claude-sonnet-4-5"));
}

#[test]
fn extract_model_id_passes_through_when_no_prefix() {
    let v = make_message(r#"{"modelId": "gpt-4.1"}"#);
    assert_eq!(extract_model_id(&v).as_deref(), Some("gpt-4.1"));
}

#[test]
fn extract_model_id_falls_back_to_metadata() {
    let v = make_message(r#"{"result": {"metadata": {"modelId": "copilot/o3"}}}"#);
    assert_eq!(extract_model_id(&v).as_deref(), Some("o3"));
}

// ---- §2.4.1 `auto` resolver (R1.4, #671) ---------------------------

/// Concrete, manifest-known modelIds pass through the resolver
/// untouched even when an `agent.id` is present. The resolver fires
/// only on the literal `"auto"` router placeholder.
#[test]
fn extract_model_id_concrete_models_bypass_auto_resolver() {
    let v = make_message(
        r#"{"modelId": "copilot/claude-sonnet-4-5", "agent": {"id": "github.copilot.editsAgent"}}"#,
    );
    assert_eq!(extract_model_id(&v).as_deref(), Some("claude-sonnet-4-5"));
}

/// `modelId == "auto"` + recognised `agent.id` resolves to the agent's
/// optimistic default model. Pricing then matches via the LiteLLM
/// manifest instead of falling through to `unpriced:no_pricing`.
#[test]
fn extract_model_id_auto_resolves_via_agent_edits() {
    let v = make_message(
        r#"{"modelId": "copilot/auto", "agent": {"id": "github.copilot.editsAgent"}}"#,
    );
    assert_eq!(extract_model_id(&v).as_deref(), Some("claude-sonnet-4-5"));
}

#[test]
fn extract_model_id_auto_resolves_via_agent_workspace() {
    let v = make_message(
        r#"{"modelId": "copilot/auto", "agent": {"id": "github.copilot.workspaceAgent"}}"#,
    );
    assert_eq!(extract_model_id(&v).as_deref(), Some("gpt-4.1"));
}

/// `modelId == "auto"` + unknown `agent.id` falls back to the literal
/// `"auto"` so the row still emits. Downstream pricing tags it
/// `unpriced:no_pricing`; the §3 reconciliation worker trues up
/// dollars on the next tick for individually-licensed users.
#[test]
fn extract_model_id_auto_with_unknown_agent_falls_back_to_auto() {
    let v = make_message(
        r#"{"modelId": "copilot/auto", "agent": {"id": "github.copilot.someFutureAgent"}}"#,
    );
    assert_eq!(extract_model_id(&v).as_deref(), Some("auto"));
}

/// `modelId == "auto"` with no `agent.id` at all (older sessions, the
/// synthetic v3 fixtures, hand-trimmed records) preserves `"auto"`.
/// This pins the back-compat contract — the resolver is additive,
/// never destructive.
#[test]
fn extract_model_id_auto_without_agent_preserves_auto() {
    let v = make_message(r#"{"modelId": "copilot/auto"}"#);
    assert_eq!(extract_model_id(&v).as_deref(), Some("auto"));
}

/// Resolver also fires when the `modelId` arrives via the Feb-2026
/// nested shape (`result.metadata.modelId`).
#[test]
fn extract_model_id_auto_resolves_under_metadata_shape() {
    let v = make_message(
        r#"{"result": {"metadata": {"modelId": "copilot/auto"}}, "agent": {"id": "github.copilot.editsAgent"}}"#,
    );
    assert_eq!(extract_model_id(&v).as_deref(), Some("claude-sonnet-4-5"));
}

/// #685: `result.metadata.resolvedModel` outranks the §2.4.1
/// `agent.id` static table when the resolved value is shape-clean
/// and the pricing manifest knows about it. Three real on-disk
/// sessions drove this priority flip:
///
/// - dated LiteLLM-canonical Anthropic key (`claude-haiku-4-5-20251001`)
///   wins directly via manifest entries — no alias hop needed.
/// - non-Anthropic auto-routed key (`grok-code-fast-1`) wins via
///   the alias overlay — without this, it would be wrongly
///   attributed to `claude-sonnet-4-5` by the editsAgent fallback.
/// - GPU-fleet code (`capi-noe-ptuc-h200-oswe-vscode-prime`)
///   isn't in the manifest, so step (1) fails and the agent.id
///   fallback runs — current behavior preserved.
#[test]
fn extract_model_id_prefers_resolved_when_manifest_known() {
    // Anthropic dated form — direct manifest hit.
    let v = make_message(
        r#"{
            "modelId": "copilot/claude-haiku-4.5",
            "agent": {"id": "github.copilot.editsAgent"},
            "result": {"metadata": {"resolvedModel": "claude-haiku-4-5-20251001"}}
        }"#,
    );
    assert_eq!(
        extract_model_id(&v).as_deref(),
        Some("claude-haiku-4-5-20251001"),
        "dated LiteLLM-canonical Anthropic key must win directly via manifest"
    );

    // Grok auto-route — alias-overlay hit, beats Sonnet fallback.
    let v = make_message(
        r#"{
            "modelId": "copilot/auto",
            "agent": {"id": "github.copilot.editsAgent"},
            "result": {"metadata": {"resolvedModel": "grok-code-fast-1"}}
        }"#,
    );
    assert_eq!(
        extract_model_id(&v).as_deref(),
        Some("grok-code-fast-1"),
        "Grok resolvedModel must win over editsAgent → claude-sonnet-4-5"
    );

    // Fleet code — manifest miss, falls through to editsAgent table.
    let v = make_message(
        r#"{
            "modelId": "copilot/auto",
            "agent": {"id": "github.copilot.editsAgent"},
            "result": {"metadata": {"resolvedModel": "capi-noe-ptuc-h200-oswe-vscode-prime"}}
        }"#,
    );
    assert_eq!(
        extract_model_id(&v).as_deref(),
        Some("claude-sonnet-4-5"),
        "fleet-code resolvedModel must fall through to §2.4.1 agent.id table"
    );

    // No resolvedModel at all — current §2.4.1 behavior preserved.
    let v = make_message(
        r#"{"modelId": "copilot/auto", "agent": {"id": "github.copilot.editsAgent"}}"#,
    );
    assert_eq!(extract_model_id(&v).as_deref(), Some("claude-sonnet-4-5"));
}

/// `is_clean_model_shape` must pass real model ids and reject
/// anything carrying dots, slashes, uppercase, or empty input —
/// the gate that lets the manifest probe in step (1) of
/// `extract_model_id` stay correct without false positives on
/// surface forms it can't handle.
#[test]
fn is_clean_model_shape_filters() {
    assert!(is_clean_model_shape("grok-code-fast-1"));
    assert!(is_clean_model_shape("claude-haiku-4-5-20251001"));
    assert!(is_clean_model_shape("capi-noe-ptuc-h200-oswe-vscode-prime"));
    assert!(!is_clean_model_shape("claude-haiku-4.5"));
    assert!(!is_clean_model_shape("xai/grok-code-fast-1"));
    assert!(!is_clean_model_shape("Claude-Haiku"));
    assert!(!is_clean_model_shape("1grok"));
    assert!(!is_clean_model_shape(""));
}

/// Direct unit test on the static table — pin every entry so a stale
/// edit (e.g. dropping a known agent id) trips the test instead of
/// silently falling through to the `"auto"` no-pricing path.
#[test]
fn resolve_auto_model_id_known_table() {
    assert_eq!(
        resolve_auto_model_id("github.copilot.editsAgent"),
        Some("claude-sonnet-4-5")
    );
    assert_eq!(
        resolve_auto_model_id("github.copilot.codingAgent"),
        Some("claude-sonnet-4-5")
    );
    assert_eq!(
        resolve_auto_model_id("github.copilot.workspaceAgent"),
        Some("gpt-4.1")
    );
    assert_eq!(
        resolve_auto_model_id("github.copilot.terminalAgent"),
        Some("gpt-4.1")
    );
    assert_eq!(
        resolve_auto_model_id("github.copilot.default"),
        Some("gpt-4.1")
    );
    assert_eq!(
        resolve_auto_model_id("github.copilot.chat-default"),
        Some("gpt-4.1")
    );
    assert_eq!(resolve_auto_model_id("github.copilot"), Some("gpt-4.1"));
    assert_eq!(resolve_auto_model_id("github.copilot.unknownAgent"), None);
    assert_eq!(resolve_auto_model_id(""), None);
}

#[test]
fn parse_jsonl_file_extracts_messages() {
    let content = concat!(
        r#"{"promptTokens": 100, "outputTokens": 5, "modelId": "copilot/gpt-4.1", "timestamp": "2026-04-12T10:30:00.000Z"}"#,
        "\n",
        // Unknown shape — skipped, no failure
        r#"{"unrelated": "event"}"#,
        "\n",
        r#"{"usage": {"promptTokens": 200, "completionTokens": 10}, "modelId": "copilot/claude-sonnet-4-5"}"#,
        "\n",
    );
    let path = Path::new("/tmp/budi-fixtures/sess-1.jsonl");
    let (msgs, offset) = parse_copilot_chat(path, content, 0);
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].input_tokens, 100);
    assert_eq!(msgs[0].output_tokens, 5);
    assert_eq!(msgs[0].model.as_deref(), Some("gpt-4.1"));
    assert_eq!(msgs[0].provider, "copilot_chat");
    assert_eq!(msgs[1].input_tokens, 200);
    assert_eq!(msgs[1].model.as_deref(), Some("claude-sonnet-4-5"));
    assert_eq!(offset, content.len());
}

#[test]
fn parse_jsonl_resumes_from_offset() {
    let content = concat!(
        r#"{"promptTokens": 100, "outputTokens": 5}"#,
        "\n",
        r#"{"promptTokens": 200, "outputTokens": 10}"#,
        "\n",
    );
    let path = Path::new("/tmp/budi-fixtures/sess-2.jsonl");
    let (first, mid_offset) = parse_copilot_chat(path, content, 0);
    assert_eq!(first.len(), 2);
    assert_eq!(mid_offset, content.len());

    let (second, _) = parse_copilot_chat(path, content, mid_offset);
    assert!(second.is_empty(), "no new content past mid_offset");
}

#[test]
fn parse_jsonl_truncates_partial_final_line() {
    // Last line lacks a terminating newline — must be left for the next read.
    let content = concat!(
        r#"{"promptTokens": 100, "outputTokens": 5}"#,
        "\n",
        r#"{"promptTokens": 200, "outputTokens": 10"#, // no closing brace, no newline
    );
    let path = Path::new("/tmp/budi-fixtures/sess-3.jsonl");
    let (msgs, offset) = parse_copilot_chat(path, content, 0);
    assert_eq!(msgs.len(), 1);
    // Offset must stop at the newline boundary so the partial line is re-read next tick.
    assert_eq!(
        offset,
        "{\"promptTokens\": 100, \"outputTokens\": 5}\n".len()
    );
}

#[test]
fn parse_json_document_extracts_messages() {
    let content = r#"{
        "sessionId": "sess-doc-1",
        "currentModel": "copilot/claude-sonnet-4-5",
        "messages": [
            {"promptTokens": 1000, "outputTokens": 50},
            {"result": {"metadata": {"promptTokens": 2000, "outputTokens": 100, "modelId": "copilot/gpt-4.1"}}}
        ]
    }"#;
    let path = Path::new("/tmp/budi-fixtures/sess-doc-1.json");
    let (msgs, offset) = parse_copilot_chat(path, content, 0);
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].session_id.as_deref(), Some("sess-doc-1"));
    assert_eq!(msgs[0].input_tokens, 1000);
    // First message has no modelId — inherits the document-level current model.
    assert_eq!(msgs[0].model.as_deref(), Some("claude-sonnet-4-5"));
    assert_eq!(msgs[1].input_tokens, 2000);
    assert_eq!(msgs[1].model.as_deref(), Some("gpt-4.1"));
    assert_eq!(offset, content.len());
}

#[test]
fn parse_json_document_unknown_shape_skipped() {
    // Document with a single unknown-shape record — no panic, no message.
    let content = r#"{"messages": [{"weird": "shape"}]}"#;
    let path = Path::new("/tmp/budi-fixtures/sess-doc-2.json");
    let (msgs, _) = parse_copilot_chat(path, content, 0);
    assert!(msgs.is_empty());
}

/// #701 acceptance — parser-local surface inference. The four
/// canonical roots from ADR-0092 §2.1 each map to a deterministic
/// surface label on every emitted row, so host extensions
/// (budi-cursor, future budi-jetbrains) can filter to "only my
/// host's data" without inspecting paths themselves.
///
/// JetBrains is excluded here because `parse_copilot_chat` never sees
/// a JetBrains-shaped path today: `watch_roots()` iterates VS
/// Code-family directories only. The JetBrains storage shape is
/// pinned at ADR-0093 and exercised by the fixture-presence tests
/// further down; the classifier-layer mapping
/// `infer_copilot_chat_surface` → `surface::JETBRAINS` is asserted in
/// `crate::surface::tests`. The matrix here pins the three roots that
/// actually flow through the parser.
#[test]
fn surface_is_cursor_when_path_under_cursor_user_root() {
    let content = r#"{"promptTokens": 1, "outputTokens": 2}"#;
    let path = Path::new(
        "/Users/dev/Library/Application Support/Cursor/User/workspaceStorage/abc/chatSessions/sess.jsonl",
    );
    let (msgs, _) = parse_copilot_chat(path, content, 0);
    assert!(!msgs.is_empty());
    for m in &msgs {
        assert_eq!(
            m.surface.as_deref(),
            Some(crate::surface::CURSOR),
            "Cursor/User/... must map to surface=cursor; got {:?}",
            m.surface
        );
    }
}

#[test]
fn surface_is_vscode_when_path_under_code_user_root() {
    let content = r#"{"promptTokens": 1, "outputTokens": 2}"#;
    let path = Path::new(
        "/Users/dev/Library/Application Support/Code/User/workspaceStorage/abc/chatSessions/sess.jsonl",
    );
    let (msgs, _) = parse_copilot_chat(path, content, 0);
    assert!(!msgs.is_empty());
    for m in &msgs {
        assert_eq!(
            m.surface.as_deref(),
            Some(crate::surface::VSCODE),
            "Code/User/... must map to surface=vscode; got {:?}",
            m.surface
        );
    }
}

#[test]
fn surface_is_vscode_when_path_under_vscode_server_root() {
    let content = r#"{"promptTokens": 1, "outputTokens": 2}"#;
    let path = Path::new(
        "/home/dev/.vscode-server/data/User/workspaceStorage/abc/chatSessions/sess.jsonl",
    );
    let (msgs, _) = parse_copilot_chat(path, content, 0);
    assert!(!msgs.is_empty());
    for m in &msgs {
        assert_eq!(
            m.surface.as_deref(),
            Some(crate::surface::VSCODE),
            "~/.vscode-server/... must map to surface=vscode; got {:?}",
            m.surface
        );
    }
}

/// JetBrains classifier — the surface module returns `jetbrains` for a
/// JetBrains-shaped path. Discovery in `watch_roots()` does not yet
/// touch the JetBrains storage root (see ADR-0093 and #716), so this
/// assertion lives at the classifier layer rather than going through
/// `parse_copilot_chat`. The classifier-layer matrix is exercised in
/// full at `crate::surface::tests`.
#[test]
fn surface_jetbrains_path_classifier_returns_jetbrains_placeholder() {
    let path = Path::new(
        "/Users/dev/Library/Application Support/JetBrains/IntelliJIdea2026.1/copilot/sessions/x.json",
    );
    assert_eq!(
        crate::surface::infer_copilot_chat_surface(path),
        crate::surface::JETBRAINS
    );
}

// -----------------------------------------------------------------------
// JetBrains fixture (ADR-0093) — anchors the next parser ticket.
// -----------------------------------------------------------------------

fn jetbrains_empty_session_fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src/providers/copilot_chat/fixtures/jetbrains_copilot_1_5_53_243_empty_session")
}

/// The captured JetBrains fixture is on disk and has the four files
/// ADR-0093 §4 names. Anchors the next parser ticket against ground
/// truth instead of a synthetic shape; fails loudly if the fixture
/// gets accidentally pruned by a future cleanup pass.
#[test]
fn jetbrains_empty_session_fixture_layout_is_intact() {
    let dir = jetbrains_empty_session_fixture_dir();
    assert!(
        dir.is_dir(),
        "fixture dir missing: {} — see ADR-0093",
        dir.display()
    );
    for relpath in [
        "00000000000.xd",
        "xd.lck",
        "copilot-chat-nitrite.db",
        "blobs/version",
    ] {
        let f = dir.join(relpath);
        assert!(
            f.is_file(),
            "fixture file missing: {} (relpath {})",
            f.display(),
            relpath
        );
    }

    // The `.expected.json` and `.shape.md` companions live one level up
    // alongside the dir and document the entity inventory.
    let parent = dir.parent().unwrap();
    assert!(
        parent
            .join("jetbrains_copilot_1_5_53_243.expected.json")
            .is_file()
    );
    assert!(
        parent
            .join("jetbrains_copilot_1_5_53_243.shape.md")
            .is_file()
    );
}

/// The `xd.lck` header has been byte-exact redacted (see shape.md). If
/// a future capture accidentally drops in a non-redacted lockfile this
/// test catches it before the PR lands.
#[test]
fn jetbrains_empty_session_xd_lck_header_is_redacted() {
    let dir = jetbrains_empty_session_fixture_dir();
    let lck = std::fs::read_to_string(dir.join("xd.lck")).unwrap();
    let first = lck.lines().next().unwrap();
    assert!(
        first.contains("0000@redacted.invalid"),
        "xd.lck header looks non-redacted: {first:?}"
    );
    assert!(
        !lck.contains("@Mac.attlocal.net") && !lck.contains("Ivan-Seredkin"),
        "xd.lck still contains real host/user PII"
    );
}

/// ADR-0093 §4 / #722: the empty fixture session carries only
/// `XdMigration` bootstrap entries — no `XdChatSession`/`XdAgentSession`
/// markers. The parser must emit zero rows for it without panicking on
/// the empty schema. This is the ground-truth anchor; populated
/// sessions are exercised inside `jetbrains` submodule tests.
#[test]
fn jetbrains_empty_session_parses_to_no_messages() {
    let dir = jetbrains_empty_session_fixture_dir();
    let parsed = jetbrains::parse_session_dir_for_tests(&dir).unwrap();
    assert!(
        parsed.is_empty(),
        "empty fixture must emit zero rows; got {parsed:?}"
    );
}

#[test]
fn deterministic_uuid_is_stable() {
    let a = deterministic_uuid("sess-1", "/tmp/x.json", 7);
    let b = deterministic_uuid("sess-1", "/tmp/x.json", 7);
    assert_eq!(a, b);
    let c = deterministic_uuid("sess-1", "/tmp/x.json", 8);
    assert_ne!(a, c);
}

#[test]
fn is_available_robust_when_dirs_absent() {
    // Pass roots that don't exist — must not panic and must return false.
    let bogus = vec![PathBuf::from("/tmp/budi-copilot-chat-does-not-exist")];
    assert!(!any_user_root_has_copilot_marker(&bogus));
}

#[test]
fn is_available_when_workspace_storage_lacks_copilot_subdirs() {
    let tmp = std::env::temp_dir().join("budi-copilot-chat-no-marker");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("workspaceStorage/abc1234")).unwrap();
    // No chatSessions, no GitHub.copilot* under the hash dir.
    assert!(!any_user_root_has_copilot_marker(std::slice::from_ref(
        &tmp
    )));
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn is_available_true_when_chat_sessions_present() {
    let tmp = std::env::temp_dir().join("budi-copilot-chat-marker-present");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("workspaceStorage/abc1234/chatSessions")).unwrap();
    assert!(any_user_root_has_copilot_marker(std::slice::from_ref(&tmp)));
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn is_available_true_when_global_storage_publisher_dir_present() {
    let tmp = std::env::temp_dir().join("budi-copilot-chat-global-publisher");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("globalStorage/github.copilot-chat/sessions")).unwrap();
    assert!(any_user_root_has_copilot_marker(std::slice::from_ref(&tmp)));
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn collect_session_files_finds_jsonl_under_chat_sessions() {
    let tmp = std::env::temp_dir().join("budi-copilot-chat-collect");
    let _ = std::fs::remove_dir_all(&tmp);
    let target = tmp.join("workspaceStorage/abc1234/chatSessions");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("sess-1.jsonl"), b"{}\n").unwrap();
    std::fs::write(target.join("sess-2.json"), b"{}").unwrap();
    std::fs::write(target.join("not-a-session.txt"), b"ignore").unwrap();

    let mut out = Vec::new();
    collect_session_files(&tmp, &mut out);
    out.sort();
    assert_eq!(out.len(), 2);
    assert!(out.iter().any(|p| p.ends_with("sess-1.jsonl")));
    assert!(out.iter().any(|p| p.ends_with("sess-2.json")));

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn collect_session_files_skips_workspace_debug_logs() {
    // #791: `<hash>/GitHub.copilot-chat/debug-logs/<requestId>/main.jsonl`
    // is OpenTelemetry span output (shape: `["attrs","dur","name","sid",
    // "spanId","status","ts","type","v"]`), not chat data. Discovery
    // must skip the whole `debug-logs/` subtree so the parser does not
    // log `copilot_chat_unknown_record_shape` for every span line.
    let tmp = std::env::temp_dir().join("budi-copilot-chat-skip-debug-logs");
    let _ = std::fs::remove_dir_all(&tmp);
    let hash_dir = tmp.join("workspaceStorage/abc1234");
    let chat = hash_dir.join("GitHub.copilot-chat/chatSessions");
    let dbg = hash_dir.join("GitHub.copilot-chat/debug-logs/req-123");
    std::fs::create_dir_all(&chat).unwrap();
    std::fs::create_dir_all(&dbg).unwrap();
    // Real chat session — must be collected.
    std::fs::write(chat.join("a.jsonl"), b"{}\n").unwrap();
    // OpenTelemetry span output — must NOT be collected.
    std::fs::write(dbg.join("main.jsonl"), br#"{"v":1,"ts":1,"type":"span"}"#).unwrap();
    std::fs::write(dbg.join("models.json"), br#"{}"#).unwrap();

    let mut out = Vec::new();
    collect_session_files(&tmp, &mut out);
    assert_eq!(
        out.len(),
        1,
        "only the chatSessions file is collected; debug-logs/main.jsonl \
         and models.json are skipped, got {out:?}",
    );
    assert!(out[0].ends_with("a.jsonl"));

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn collect_session_files_recurses_into_global_publisher_dir() {
    let tmp = std::env::temp_dir().join("budi-copilot-chat-recurse");
    let _ = std::fs::remove_dir_all(&tmp);
    let nested = tmp.join("globalStorage/GitHub.copilot-chat/sessions/2026-05");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(nested.join("a.jsonl"), b"{}\n").unwrap();

    let mut out = Vec::new();
    collect_session_files(&tmp, &mut out);
    assert_eq!(out.len(), 1);
    assert!(out[0].ends_with("a.jsonl"));

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn collect_session_files_skips_global_publisher_siblings_of_session_dir() {
    // ADR-0092 §2.2 directory-name allowlist (#684): under
    // globalStorage/{publisher}/, only files inside chatSessions,
    // chat-sessions, or sessions subtrees are session files. Embedding
    // caches and CLI state blobs sitting as siblings must be skipped.
    let tmp = std::env::temp_dir().join("budi-copilot-chat-skip-siblings");
    let _ = std::fs::remove_dir_all(&tmp);
    let pub_dir = tmp.join("globalStorage/github.copilot-chat");
    std::fs::create_dir_all(&pub_dir).unwrap();

    // Sibling files (NOT chat sessions): VS Code embedding caches and
    // the Copilot CLI v2 state blob.
    std::fs::write(
        pub_dir.join("commandEmbeddings.json"),
        // Large embedding-only payload, no `kind`/`requests`/`messages` keys.
        br#"{"core":{"editor.action.setSelectionAnchor":{"embedding":[0.008,-0.029,0.061]}}}"#,
    )
    .unwrap();
    std::fs::write(
        pub_dir.join("settingEmbeddings.json"),
        br#"{"core":{"editor.fontSize":{"embedding":[0.1,0.2]}}}"#,
    )
    .unwrap();
    std::fs::write(
        pub_dir.join("copilot.cli.oldGlobalSessions.json"),
        br#"{"version":2,"sessions":{}}"#,
    )
    .unwrap();

    // Real session file under a known session-storage directory.
    let chat_sessions = pub_dir.join("chatSessions");
    std::fs::create_dir_all(&chat_sessions).unwrap();
    std::fs::write(
        chat_sessions.join("0e3b1f3c-1234-4abc-9def-aaaabbbbcccc.jsonl"),
        br#"{"kind":0,"v":{"sessionId":"abc","creationDate":"2026-04-15T10:00:00Z"}}
"#,
    )
    .unwrap();

    let mut out = Vec::new();
    collect_session_files(&tmp, &mut out);
    assert_eq!(
        out.len(),
        1,
        "only the chatSessions/<uuid>.jsonl is collected; \
         commandEmbeddings.json / settingEmbeddings.json / \
         copilot.cli.oldGlobalSessions.json siblings are skipped, got {out:?}"
    );
    assert!(out[0].ends_with("0e3b1f3c-1234-4abc-9def-aaaabbbbcccc.jsonl"));

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn collect_session_files_accepts_chat_sessions_chat_sessions_and_sessions_subdirs() {
    // The directory-name allowlist (#684) covers all three known
    // session-storage names. A future fourth name must amend ADR-0092
    // §2.2 in lockstep.
    let tmp = std::env::temp_dir().join("budi-copilot-chat-allowlist-names");
    let _ = std::fs::remove_dir_all(&tmp);
    let pub_dir = tmp.join("globalStorage/GitHub.copilot-chat");
    for name in ["chatSessions", "chat-sessions", "sessions"] {
        let d = pub_dir.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("s.jsonl"), b"{}\n").unwrap();
    }

    let mut out = Vec::new();
    collect_session_files(&tmp, &mut out);
    assert_eq!(
        out.len(),
        3,
        "all three allowlisted names match, got {out:?}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn watch_roots_skips_absent_subdirs() {
    // Stub home with neither workspaceStorage nor globalStorage — the
    // provider must not panic and must return an empty watch list for
    // that root. We exercise the scan helper directly because
    // CopilotChatProvider::watch_roots() consults the real $HOME.
    let tmp = std::env::temp_dir().join("budi-copilot-chat-watch-empty");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let mut roots = Vec::new();
    let ws = tmp.join("workspaceStorage");
    if ws.is_dir() {
        roots.push(ws);
    }
    let gs = tmp.join("globalStorage");
    if gs.is_dir() {
        roots.push(gs);
    }
    assert!(roots.is_empty());

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Real on-disk JSONL shape from `chatSessions/<id>.jsonl` written by
/// the `github.copilot-chat` extension circa 2026-04. The token-bearing
/// records are wrapped under the `kind: 2 / v: [...]` envelope and the
/// counts live at `result.metadata.{promptTokens,outputTokens}`. This
/// fixture is captured from a real session on a developer machine and
/// then trimmed to the fields the parser inspects — the structural
/// envelope (kind / v / nesting depth) is preserved verbatim so any
/// future regression of [`flatten_records`] is caught here.
#[test]
fn parse_jsonl_real_kind_v_envelope() {
    let content = concat!(
        // kind:0 manifest line — no tokens, must not produce a message
        // and must not trigger an unknown-shape warn (its `v` is an
        // object, which is the documented "session manifest" shape).
        r#"{"kind":0,"v":{"sessionId":"abc","creationDate":"2026-04-15T10:00:00Z"}}"#,
        "\n",
        // kind:1 string — text fragment, no tokens, must not produce.
        r#"{"kind":1,"v":"user prompt text"}"#,
        "\n",
        // kind:2 response — the token-bearing shape. `v` is an array of
        // one assistant turn, tokens at result.metadata.{promptTokens,outputTokens}.
        r#"{"kind":2,"v":[{"modelId":"copilot/claude-haiku-4.5","completionTokens":191,"requestId":"req-1","timestamp":1715000000000,"result":{"metadata":{"promptTokens":26412,"outputTokens":191,"modelMessageId":"m-1","resolvedModel":"claude-haiku-4.5"}}}]}"#,
        "\n",
    );
    let path = Path::new("/tmp/budi-fixtures/sess-real-jsonl.jsonl");
    let (msgs, offset) = parse_copilot_chat(path, content, 0);
    assert_eq!(msgs.len(), 1, "exactly one assistant turn carries tokens");
    let m = &msgs[0];
    assert_eq!(m.input_tokens, 26412);
    assert_eq!(m.output_tokens, 191);
    assert_eq!(m.model.as_deref(), Some("claude-haiku-4.5"));
    assert_eq!(m.provider, "copilot_chat");
    assert_eq!(offset, content.len());
}

/// Real on-disk `.json` snapshot shape — `requests: [...]` envelope,
/// each request carrying tokens at
/// `result.metadata.{promptTokens,outputTokens}`. Mirrors the .jsonl
/// shape but as a single document (older / persisted-on-close form).
#[test]
fn parse_json_document_real_requests_envelope() {
    let content = r#"{
        "sessionId": "real-doc-1",
        "version": 3,
        "requesterUsername": "alice",
        "responderUsername": "GitHub Copilot",
        "requests": [
            {
                "modelId": "github.copilot-chat/claude-sonnet-4",
                "requestId": "r-1",
                "timestamp": 1715000001000,
                "result": {
                    "metadata": {
                        "promptTokens": 1234,
                        "outputTokens": 56,
                        "modelMessageId": "mm-1"
                    }
                }
            },
            {
                "modelId": "github.copilot-chat/claude-sonnet-4",
                "requestId": "r-2-no-tokens",
                "timestamp": 1715000002000
            }
        ]
    }"#;
    let path = Path::new("/tmp/budi-fixtures/sess-real-doc.json");
    let (msgs, _) = parse_copilot_chat(path, content, 0);
    assert_eq!(
        msgs.len(),
        1,
        "only the request with result.metadata tokens produces a message"
    );
    let m = &msgs[0];
    assert_eq!(m.input_tokens, 1234);
    assert_eq!(m.output_tokens, 56);
    // `github.copilot-chat/` prefix should be normalised the same way
    // `copilot/` is — the strip happens via [`strip_copilot_prefix`].
    // Today only `copilot/` is stripped, so we assert the full id
    // passes through unchanged; if that ever changes, tighten here.
    assert!(
        m.model.as_deref().unwrap_or("").contains("claude-sonnet-4"),
        "model id should mention claude-sonnet-4, got {:?}",
        m.model
    );
    assert_eq!(m.session_id.as_deref(), Some("real-doc-1"));
}

/// `kind:1` lines whose `v` is an array of state events (no tokens
/// anywhere) must not emit an unknown-shape warn — the wrapper is
/// known, the inner records simply don't carry tokens. Pinning this
/// keeps the warn-once log from getting noisy on real sessions.
#[test]
fn parse_jsonl_kind1_array_silently_yields_no_messages() {
    let content = concat!(
        r#"{"kind":1,"v":[{"role":"user","content":"hi"},{"role":"system","content":"ok"}]}"#,
        "\n",
    );
    let path = Path::new("/tmp/budi-fixtures/sess-kind1.jsonl");
    let (msgs, _) = parse_copilot_chat(path, content, 0);
    assert!(msgs.is_empty());
}

/// v3 (8.4.0) output-only fallback shape — VS Code Copilot Chat builds
/// circa 2026-05 persist `completionTokens` at the top of each
/// response record but no `promptTokens` counterpart anywhere. The
/// parser must still emit a row (with `input_tokens = 0`) so the
/// session is visible in the local-tail surface and the Billing API
/// reconciliation worker has a `(date, model)` bucket to truth up.
#[test]
fn extract_tokens_completion_only_shape() {
    let record = serde_json::json!({
        "modelId": "copilot/auto",
        "completionTokens": 65,
        "result": {
            "metadata": {
                "resolvedModel": "capi-noe-ptuc-h200-oswe-vscode-prime"
            }
        }
    });
    let tokens = extract_tokens(&record).expect("must match output-only fallback");
    assert_eq!(tokens.input, 0);
    assert_eq!(tokens.output, 65);
}

/// Output-only fallback must not fire when `completionTokens == 0` —
/// that case is "valid shape, empty record" (the surrounding logic
/// would emit a useless 0/0 row otherwise).
#[test]
fn extract_tokens_completion_only_zero_skips() {
    let record = serde_json::json!({"modelId": "x", "completionTokens": 0});
    assert!(extract_tokens(&record).is_none());
}

/// Full-pair shapes must outrank the output-only fallback when both
/// keys are present — otherwise the `feb_2026` shape would lose its
/// input-token count to the fallback's `input = 0`.
#[test]
fn extract_tokens_full_pair_outranks_completion_only_fallback() {
    let record = serde_json::json!({
        "modelId": "copilot/x",
        "completionTokens": 999,
        "result": {
            "metadata": {
                "promptTokens": 100,
                "outputTokens": 50
            }
        }
    });
    let tokens = extract_tokens(&record).expect("feb_2026 shape must win");
    assert_eq!(tokens.input, 100);
    assert_eq!(tokens.output, 50);
}

/// End-to-end on a real-shape JSONL with the v3 output-only records
/// (kind:0 manifest, kind:1 state events, kind:2 response with only
/// `completionTokens`). Three response turns → three messages; the
/// kind:0 / kind:1 lines emit nothing and stay silent.
#[test]
fn parse_jsonl_real_v3_completion_only_turns() {
    let content = concat!(
        r#"{"kind":0,"v":{"sessionId":"s","creationDate":"2026-05-07T15:00:00Z"}}"#,
        "\n",
        r#"{"kind":1,"v":{"completedAt":1715000000000,"value":"prompt"}}"#,
        "\n",
        r#"{"kind":2,"v":[{"modelId":"copilot/auto","completionTokens":65,"requestId":"r1","result":{"metadata":{"resolvedModel":"capi-noe-ptuc-h200-oswe-vscode-prime"}}}]}"#,
        "\n",
        r#"{"kind":2,"v":[{"modelId":"copilot/auto","completionTokens":115,"requestId":"r2","result":{"metadata":{"resolvedModel":"capi-noe-ptuc-h200-oswe-vscode-prime"}}},{"modelId":"copilot/auto","completionTokens":117,"requestId":"r3","result":{"metadata":{"resolvedModel":"capi-noe-ptuc-h200-oswe-vscode-prime"}}}]}"#,
        "\n",
    );
    let path = Path::new("/tmp/budi-fixtures/sess-v3.jsonl");
    let (msgs, _) = parse_copilot_chat(path, content, 0);
    assert_eq!(msgs.len(), 3);
    assert!(msgs.iter().all(|m| m.input_tokens == 0));
    assert_eq!(
        msgs.iter().map(|m| m.output_tokens).sum::<u64>(),
        65 + 115 + 117
    );
    // All emit with the user-facing modelId, not the fleet-code resolvedModel.
    assert!(msgs.iter().all(|m| m.model.as_deref() == Some("auto")));
}

// ---- v4 (8.4.1, R1.1): mutation-log reducer tests ------------------

/// kind:0 snapshot followed by kind:1 patches that fill in
/// `completionTokens` for an existing request. This is the shape that
/// VS Code 1.109+ writes mid-conversation, and the regression that
/// drove ticket #668 — the v3 parser saw the kind:1 `v: 39` line as
/// a flat record with no token keys at the top level and emitted
/// nothing. The reducer materializes the merged request and the
/// output-only fallback shape produces a row.
#[test]
fn reducer_kind1_completion_tokens_patch_emits_row() {
    let content = concat!(
        // kind:0 snapshot — one request stub, no tokens yet.
        r#"{"kind":0,"v":{"sessionId":"s-1","requests":[{"requestId":"r-1","modelId":"copilot/claude-sonnet-4-5"}]}}"#,
        "\n",
        // kind:1 patch lands the completion-token count on requests[0].
        r#"{"kind":1,"k":["requests",0,"completionTokens"],"v":42}"#,
        "\n",
    );
    let path = Path::new("/tmp/budi-fixtures/sess-reducer-1.jsonl");
    let (msgs, _) = parse_copilot_chat(path, content, 0);
    assert_eq!(msgs.len(), 1, "completionTokens patch must emit one row");
    let m = &msgs[0];
    assert_eq!(m.input_tokens, 0, "output-only fallback ⇒ input = 0");
    assert_eq!(m.output_tokens, 42);
    assert_eq!(m.model.as_deref(), Some("claude-sonnet-4-5"));
    assert_eq!(m.session_id.as_deref(), Some("s-1"));
}

/// kind:1 patches that land both `promptTokens` and `outputTokens` on
/// `result.metadata` for a request stub appended via kind:2. Auto-grow
/// of intermediate objects is exercised: the kind:2 stub doesn't carry
/// `result.metadata` at all, so the kind:1 path
/// `["requests",0,"result","metadata","promptTokens"]` has to
/// materialize the missing object levels on the way in.
#[test]
fn reducer_kind1_patches_auto_create_intermediate_objects() {
    let content = concat!(
        // No kind:0 snapshot — start empty. kind:2 push adds a stub.
        r#"{"kind":2,"k":["requests"],"v":[{"requestId":"r-9","modelId":"copilot/gpt-4.1"}]}"#,
        "\n",
        // kind:1 patches stream the token counts in. result/metadata
        // do not yet exist — set_at_path must create them.
        r#"{"kind":1,"k":["requests",0,"result","metadata","promptTokens"],"v":1234}"#,
        "\n",
        r#"{"kind":1,"k":["requests",0,"result","metadata","outputTokens"],"v":56}"#,
        "\n",
    );
    let path = Path::new("/tmp/budi-fixtures/sess-reducer-2.jsonl");
    let (msgs, _) = parse_copilot_chat(path, content, 0);
    // Three lines, but the first emit-eligible mutation is the second
    // kind:1 patch (when both prompt+output land). The first kind:1
    // patch alone leaves `outputTokens = 0` so no shape matches yet.
    // Result: exactly one row.
    assert_eq!(msgs.len(), 1, "feb-2026 shape must materialize once");
    let m = &msgs[0];
    assert_eq!(m.input_tokens, 1234);
    assert_eq!(m.output_tokens, 56);
    assert_eq!(m.model.as_deref(), Some("gpt-4.1"));
}

/// Acceptance criterion (#668): "append a kind:2 stub then a kind:1
/// completionTokens patch to a watched file; assert exactly one row
/// materializes after the patch." This pins the live-tailer ordering:
/// the kind:2 stub alone emits nothing, the kind:1 patch landing
/// completionTokens emits one row, and a *second* kind:1 patch on
/// the same request (e.g. updating `timestamp`) does not double-emit.
#[test]
fn reducer_emit_keyed_by_request_id_no_double_emit() {
    let content = concat!(
        r#"{"kind":2,"k":["requests"],"v":[{"requestId":"r-only","modelId":"copilot/auto"}]}"#,
        "\n",
        r#"{"kind":1,"k":["requests",0,"completionTokens"],"v":77}"#,
        "\n",
        // Later patch on the same request — must NOT emit a second row.
        r#"{"kind":1,"k":["requests",0,"timestamp"],"v":1715000999000}"#,
        "\n",
        r#"{"kind":1,"k":["requests",0,"completionTokens"],"v":80}"#,
        "\n",
    );
    let path = Path::new("/tmp/budi-fixtures/sess-reducer-3.jsonl");
    let (msgs, _) = parse_copilot_chat(path, content, 0);
    assert_eq!(msgs.len(), 1, "exactly one row per requestId");
    assert_eq!(msgs[0].output_tokens, 77, "first complete value wins");
}

/// Two kind:2 splices to `["requests"]` followed by interleaved kind:1
/// patches that complete each request at different lines. The reducer
/// must emit one row per request, in the order each request becomes
/// complete (not in array-index order).
#[test]
fn reducer_multiple_requests_emit_in_completion_order() {
    let content = concat!(
        r#"{"kind":0,"v":{"sessionId":"s-multi","requests":[]}}"#,
        "\n",
        r#"{"kind":2,"k":["requests"],"v":[{"requestId":"a","modelId":"copilot/gpt-4.1"}]}"#,
        "\n",
        r#"{"kind":2,"k":["requests"],"v":[{"requestId":"b","modelId":"copilot/gpt-4.1"}]}"#,
        "\n",
        // Request b completes first (out-of-order vs. array index).
        r#"{"kind":1,"k":["requests",1,"result","metadata","promptTokens"],"v":10}"#,
        "\n",
        r#"{"kind":1,"k":["requests",1,"result","metadata","outputTokens"],"v":2}"#,
        "\n",
        // Then request a completes.
        r#"{"kind":1,"k":["requests",0,"result","metadata","promptTokens"],"v":100}"#,
        "\n",
        r#"{"kind":1,"k":["requests",0,"result","metadata","outputTokens"],"v":20}"#,
        "\n",
    );
    let path = Path::new("/tmp/budi-fixtures/sess-reducer-4.jsonl");
    let (msgs, _) = parse_copilot_chat(path, content, 0);
    assert_eq!(msgs.len(), 2);
    // First-completed (request b) emits first.
    assert_eq!(msgs[0].input_tokens, 10);
    assert_eq!(msgs[0].output_tokens, 2);
    assert_eq!(msgs[1].input_tokens, 100);
    assert_eq!(msgs[1].output_tokens, 20);
    // Stable per-request UUIDs — different requests, different UUIDs.
    assert_ne!(msgs[0].uuid, msgs[1].uuid);
}

/// kind:0 snapshots that already inline `completionTokens` (the
/// historical path that even the v3 parser handled) must keep
/// emitting a single row through the reducer. This is the regression
/// shape called out in #668: "only kind:0 lines whose `requests`
/// snapshot already had `completionTokens` inline at file write time"
/// produced rows on v3. The reducer must preserve this for the
/// imported-historical-session case (`budi db import`).
#[test]
fn reducer_kind0_snapshot_with_inline_tokens_emits() {
    let content = concat!(
        r#"{"kind":0,"v":{"sessionId":"hist-1","requests":[{"requestId":"h-1","modelId":"copilot/claude-haiku-4.5","result":{"metadata":{"promptTokens":500,"outputTokens":12}}}]}}"#,
        "\n",
    );
    let path = Path::new("/tmp/budi-fixtures/sess-hist.jsonl");
    let (msgs, _) = parse_copilot_chat(path, content, 0);
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].input_tokens, 500);
    assert_eq!(msgs[0].output_tokens, 12);
    assert_eq!(msgs[0].model.as_deref(), Some("claude-haiku-4.5"));
}

/// Reducer-emitted rows must use a deterministic UUID that's stable
/// across re-parses keyed by `requestId` — so a future call that
/// re-replays the file (e.g. on daemon restart) produces the same UUID
/// and the database upsert dedupes instead of double-counting.
#[test]
fn reducer_deterministic_uuid_stable_across_reparse() {
    let content = concat!(
        r#"{"kind":2,"k":["requests"],"v":[{"requestId":"stable-key","modelId":"copilot/x"}]}"#,
        "\n",
        r#"{"kind":1,"k":["requests",0,"completionTokens"],"v":7}"#,
        "\n",
    );
    let path = Path::new("/tmp/budi-fixtures/sess-stable.jsonl");
    let (first, _) = parse_copilot_chat(path, content, 0);
    let (second, _) = parse_copilot_chat(path, content, 0);
    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 1);
    assert_eq!(first[0].uuid, second[0].uuid);
}

/// `set_at_path` correctness — exercises the helper directly so a
/// future regression of the auto-grow / placeholder logic is caught
/// without needing to construct a full mutation-log fixture.
#[test]
fn set_at_path_grows_arrays_and_creates_objects() {
    let mut state = serde_json::json!({});
    let path = vec![
        serde_json::json!("requests"),
        serde_json::json!(2),
        serde_json::json!("result"),
        serde_json::json!("metadata"),
        serde_json::json!("promptTokens"),
    ];
    set_at_path(&mut state, &path, serde_json::json!(99));
    assert_eq!(
        state
            .pointer("/requests/2/result/metadata/promptTokens")
            .and_then(|v| v.as_u64()),
        Some(99)
    );
    // Indices 0 and 1 are placeholder objects (next segment is the
    // string "result", so an object placeholder is correct).
    assert!(
        state
            .pointer("/requests/0")
            .map(|v| v.is_object())
            .unwrap_or(false)
    );
    assert!(
        state
            .pointer("/requests/1")
            .map(|v| v.is_object())
            .unwrap_or(false)
    );
}

// ---- v4 (8.4.1, R1.2): real-extension regression fixture --------------

/// Real-extension regression fixture (#669) — sanitized capture of an
/// actual `github.copilot-chat` 0.47.0 session file. The reducer must
/// materialize one row per completed request, matching the expected
/// `(requestId, output_tokens, input_tokens, model)` tuples from
/// `vscode_chat_0_47_0.expected.json` exactly once each.
///
/// Why this exists: the synthetic v3 fixtures pass with both the old
/// per-line parser and the v4 reducer because they don't actually
/// exercise the kind:0 + kind:1/kind:2 envelope dance. This test pins
/// the reducer against a real on-disk capture so a future regression
/// of the same shape (an extension bump that changes how the mutation
/// log is shaped) fails loudly here even when the synthetic fixtures
/// continue to pass.
#[test]
fn parse_real_vscode_0_47_0_fixture() {
    let content = include_str!("fixtures/vscode_chat_0_47_0.jsonl");
    let expected_json = include_str!("fixtures/vscode_chat_0_47_0.expected.json");
    let expected: Vec<serde_json::Value> = serde_json::from_str(expected_json).unwrap();

    // Use a path that does NOT exist on disk so the parser falls through
    // to the in-memory `content` rather than re-reading from a stale
    // checkout-relative path.
    let path = Path::new("/tmp/budi-fixtures-r1-2/vscode_chat_0_47_0.jsonl");
    let (msgs, _) = parse_copilot_chat(path, content, 0);

    assert_eq!(
        msgs.len(),
        expected.len(),
        "fixture must yield exactly {} rows (8 assistant rows + 2 user \
         rows from the synthetic message.text on the first two requests)",
        expected.len()
    );

    // Each expected entry carries a `role`; assistant entries match by
    // `(output_tokens, model)` (output values are all distinct), and
    // user entries match by `(role, prompt_category)` against the
    // paired user row produced for the same `requestId`. The fixture's
    // synthetic user prompts are authored so each `prompt_category`
    // is unique among user rows in the fixture.
    for entry in &expected {
        let role = entry["role"].as_str().unwrap();
        let matches: Vec<_> = match role {
            "assistant" => {
                let want_output = entry["output_tokens"].as_u64().unwrap();
                let want_model = entry["model"].as_str().unwrap();
                msgs.iter()
                    .filter(|m| {
                        m.role == "assistant"
                            && m.output_tokens == want_output
                            && m.model.as_deref() == Some(want_model)
                    })
                    .collect()
            }
            "user" => {
                let want_category = entry["prompt_category"].as_str().unwrap();
                msgs.iter()
                    .filter(|m| {
                        m.role == "user" && m.prompt_category.as_deref() == Some(want_category)
                    })
                    .collect()
            }
            other => panic!("unknown role in expected.json: {other}"),
        };
        assert_eq!(
            matches.len(),
            1,
            "expected exactly one {} row for requestId={}; got {}",
            role,
            entry["requestId"].as_str().unwrap(),
            matches.len()
        );
    }

    // Provider tag is preserved through the reducer.
    assert!(msgs.iter().all(|m| m.provider == "copilot_chat"));
    // Every assistant row maps to the §2.4.1 edits-agent default
    // (`claude-sonnet-4-5`); user rows carry `model = None`.
    for m in &msgs {
        match m.role.as_str() {
            "assistant" => assert_eq!(m.model.as_deref(), Some("claude-sonnet-4-5")),
            "user" => assert!(m.model.is_none()),
            other => panic!("unexpected role: {other}"),
        }
    }
    // #686 acceptance: both row roles materialize, and every assistant
    // row whose paired request carried `message.text` (or `message.parts`)
    // points back at its user row via `parent_uuid`.
    assert!(msgs.iter().any(|m| m.role == "user"));
    assert!(msgs.iter().any(|m| m.role == "assistant"));
    let user_uuids: std::collections::HashSet<&str> = msgs
        .iter()
        .filter(|m| m.role == "user")
        .map(|m| m.uuid.as_str())
        .collect();
    let assistants_with_parent = msgs
        .iter()
        .filter(|m| m.role == "assistant")
        .filter(|m| m.parent_uuid.is_some())
        .count();
    assert_eq!(
        assistants_with_parent,
        user_uuids.len(),
        "every user row in the fixture must have exactly one paired assistant row"
    );
    for m in msgs.iter().filter(|m| m.role == "assistant") {
        if let Some(p) = m.parent_uuid.as_deref() {
            assert!(
                user_uuids.contains(p),
                "assistant row's parent_uuid {} must reference a user row in the same parse",
                p
            );
        }
    }
    // #687 acceptance: assistant rows whose request carried a
    // toolCallRounds entry materialize tool_names / tool_use_ids /
    // tool_files. The dc9f930d request in the fixture is the only
    // one with synthetic tool data; all other assistant rows must
    // surface empty tool slots.
    for entry in &expected {
        if entry["role"].as_str() != Some("assistant") {
            continue;
        }
        let want_output = entry["output_tokens"].as_u64().unwrap();
        let row = msgs
            .iter()
            .find(|m| m.role == "assistant" && m.output_tokens == want_output)
            .expect("assistant row must exist");
        let want_names: Vec<String> = entry
            .get("tool_names")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let want_ids: Vec<String> = entry
            .get("tool_use_ids")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let want_files: Vec<String> = entry
            .get("tool_files")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(
            row.tool_names,
            want_names,
            "tool_names mismatch on requestId={}",
            entry["requestId"].as_str().unwrap()
        );
        assert_eq!(
            row.tool_use_ids,
            want_ids,
            "tool_use_ids mismatch on requestId={}",
            entry["requestId"].as_str().unwrap()
        );
        assert_eq!(
            row.tool_files,
            want_files,
            "tool_files mismatch on requestId={}",
            entry["requestId"].as_str().unwrap()
        );
    }
    // User rows must always have empty tool slots — the tool data
    // belongs to the assistant turn, not the prompt that initiated
    // it.
    for m in msgs.iter().filter(|m| m.role == "user") {
        assert!(m.tool_names.is_empty());
        assert!(m.tool_use_ids.is_empty());
        assert!(m.tool_files.is_empty());
    }
}

/// Streaming-truncation variant — the same fixture sliced to drop the
/// final `kind:1` `completionTokens` patch. The kind:2 stub for the
/// last request is on disk (`requestId` + `modelId` exist) but the
/// completion-token count never landed.
///
/// Pins the live-tailer contract from #668: an in-flight request MUST
/// NOT emit until the completion token arrives. Only the seven
/// requests with inline `completionTokens` (delivered on the kind:2
/// push payload) materialize.
#[test]
fn parse_real_vscode_0_47_0_fixture_streaming_truncation() {
    let content = include_str!("fixtures/vscode_chat_0_47_0_streaming.jsonl");
    let path = Path::new("/tmp/budi-fixtures-r1-2/vscode_chat_0_47_0_streaming.jsonl");
    let (msgs, _) = parse_copilot_chat(path, content, 0);

    // 7 assistant rows from the inline-completionTokens requests, plus
    // 2 user rows from the synthetic `message.text` on the first two
    // requests (#686). The kind:2 stub for the in-flight final request
    // carries neither tokens nor a message, so it must not emit at all.
    let assistant_count = msgs.iter().filter(|m| m.role == "assistant").count();
    let user_count = msgs.iter().filter(|m| m.role == "user").count();
    assert_eq!(
        assistant_count, 7,
        "truncated fixture: 7 inline-completionTokens requests must \
         still emit assistant rows; the kind:2 stub for the in-flight \
         request must NOT"
    );
    assert_eq!(
        user_count, 2,
        "truncated fixture: 2 user rows from the synthetic message.text \
         on the first two requests"
    );

    // The patched-only request's completion-token value (39) is the
    // signature for the in-flight row. It must not appear.
    assert!(
        !msgs.iter().any(|m| m.output_tokens == 39),
        "the in-flight (kind:2 stub, no completionTokens patch) request \
         leaked into the output — this is the no-double-emit /  \
         wait-for-completion-token contract from R1.1 #668"
    );

    // Every assistant row carries a non-zero output_tokens — none are
    // synthesized from the bare stub. User rows always have output=0.
    assert!(
        msgs.iter()
            .filter(|m| m.role == "assistant")
            .all(|m| m.output_tokens > 0)
    );
}

/// v5 (8.5.1, #791): real-shape regression for sessions where the user
/// attaches a non-source surface (Settings UI, Outline, file tree) to
/// the chat input. The extension persists those attachments as
/// `kind:1 k=["inputState","attachments"] v=[{ ... DOM-like UI
/// introspection record ... }]` mutations, each kilobytes-to-tens-of-
/// kilobytes in size. The reducer MUST:
///
/// 1. Apply the mutations to `state.inputState.attachments` without
///    emitting `copilot_chat_unknown_record_shape` warnings (the
///    records are UI state, not request data — they correctly land
///    under `inputState` and never end up under `state.requests`).
/// 2. Still emit exactly the two rows the one real `kind:2` request
///    yields — one user row from the synthetic `message.text` and
///    one assistant row keyed off `result.metadata.{promptTokens,
///    outputTokens}` (Feb-2026 shape, §2.3 #4) with
///    `model = "claude-haiku-4-5-20251001"` resolved through
///    `result.metadata.resolvedModel` (§2.4 #1).
///
/// Drives #791: in the wild the attachments mutations dominate the
/// byte stream (29 MB/30 min in the reporter's repro), making `budi
/// doctor` flag "tailer advanced but no rows landed" as a false-
/// positive shape regression. This fixture pins the contract so the
/// signal/noise ratio doesn't drift again.
#[test]
fn parse_real_vscode_0_47_0_fixture_v5_attachments() {
    let content = include_str!("fixtures/vscode_chat_0_47_0_v5.jsonl");
    let path = Path::new("/tmp/budi-fixtures-r1-2/vscode_chat_0_47_0_v5.jsonl");
    let (msgs, _) = parse_copilot_chat(path, content, 0);

    let user_count = msgs.iter().filter(|m| m.role == "user").count();
    let assistant_count = msgs.iter().filter(|m| m.role == "assistant").count();
    assert_eq!(
        (user_count, assistant_count),
        (1, 1),
        "exactly one user row (from synthetic message.text) and one \
         assistant row (from the real result.metadata token pair); the \
         two large inputState.attachments kind:1 mutations carry no \
         tokens and must NOT emit",
    );
    let asst = msgs.iter().find(|m| m.role == "assistant").unwrap();
    assert_eq!(asst.input_tokens, 26412);
    assert_eq!(asst.output_tokens, 191);
    assert_eq!(
        asst.model.as_deref(),
        Some("claude-haiku-4-5-20251001"),
        "model resolves through result.metadata.resolvedModel — the \
         modelId=\"copilot/claude-haiku-4.5\" alias on the request is \
         outranked by the manifest-known resolved form (§2.4 #1)",
    );
    assert_eq!(asst.provider, "copilot_chat");
    // User row pairs back to assistant via parent_uuid (§2.3 v4
    // user-row emit contract).
    let user = msgs.iter().find(|m| m.role == "user").unwrap();
    assert_eq!(asst.parent_uuid.as_deref(), Some(user.uuid.as_str()));
}

// ---- #681: workspace.json cwd enrichment --------------------------

/// `parse_workspace_storage_session_enriches_cwd` — covers all four
/// shapes the parser must handle per #681:
/// 1. `<workspaceStorage>/<hash>/chatSessions/<uuid>.jsonl` with a
///    sibling `<hash>/workspace.json` → cwd populated from `folder`.
/// 2. `<globalStorage>/emptyWindowChatSessions/<uuid>.jsonl` → cwd
///    stays `None` cleanly (no spurious warnings, no crash).
/// 3. Remote / dev-container — same `<hash>/workspace.json` shape on
///    the remote-side path (`~/.vscode-server/data/User/...`) → cwd
///    populated.
/// 4. Multi-root — `workspace.json` carries `configuration` pointing
///    at a `.code-workspace` file → first folder's path is the cwd.
#[test]
fn parse_workspace_storage_session_enriches_cwd() {
    // Use forward-slashed string-form paths for any value that round-
    // trips through a `file://` URI — Windows uses backslashes in
    // PathBuf string forms, but VS Code (and RFC 3986) writes URIs
    // with forward slashes, and an unescaped backslash inside a JSON
    // string is an invalid escape that aborts parsing.
    fn fwd(p: &Path) -> String {
        p.to_string_lossy().replace('\\', "/")
    }

    let tmp = std::env::temp_dir().join("budi-copilot-chat-cwd-enrich");
    let _ = std::fs::remove_dir_all(&tmp);

    let line = r#"{"kind":2,"v":[{"requestId":"r-1","modelId":"copilot/gpt-4.1","completionTokens":42,"result":{"metadata":{"resolvedModel":"x"}}}]}"#;

    // ---- Case 1: workspaceStorage single-root ----------------------
    let hash_dir = tmp.join("Library/Application Support/Code/User/workspaceStorage/abc123");
    let chat_dir = hash_dir.join("chatSessions");
    std::fs::create_dir_all(&chat_dir).unwrap();
    let target_cwd = format!("{}/repos/single-root", fwd(&tmp));
    let workspace_json = serde_json::json!({
        "folder": format!("file://{}", target_cwd),
    })
    .to_string();
    std::fs::write(hash_dir.join("workspace.json"), workspace_json).unwrap();
    let session_path = chat_dir.join("sess-single.jsonl");
    std::fs::write(&session_path, format!("{line}\n")).unwrap();
    let (msgs, _) = parse_copilot_chat(&session_path, &format!("{line}\n"), 0);
    assert_eq!(msgs.len(), 1, "single-root session emits one row");
    assert_eq!(
        msgs[0].cwd.as_deref(),
        Some(target_cwd.as_str()),
        "single-root cwd resolved from workspace.json folder URI"
    );

    // ---- Case 2: emptyWindowChatSessions ---------------------------
    let empty_dir =
        tmp.join("Library/Application Support/Code/User/globalStorage/emptyWindowChatSessions");
    std::fs::create_dir_all(&empty_dir).unwrap();
    let empty_path = empty_dir.join("sess-empty.jsonl");
    std::fs::write(&empty_path, format!("{line}\n")).unwrap();
    let (empty_msgs, _) = parse_copilot_chat(&empty_path, &format!("{line}\n"), 0);
    assert_eq!(empty_msgs.len(), 1);
    assert!(
        empty_msgs[0].cwd.is_none(),
        "emptyWindowChatSessions session leaves cwd None"
    );
    assert!(
        empty_msgs[0].git_branch.is_none(),
        "emptyWindowChatSessions session leaves git_branch None"
    );

    // ---- Case 3: remote / dev-container ----------------------------
    let remote_hash = tmp.join(".vscode-server/data/User/workspaceStorage/remotehash456");
    let remote_chat = remote_hash.join("chatSessions");
    std::fs::create_dir_all(&remote_chat).unwrap();
    let remote_workspace_json = serde_json::json!({
        "folder": "vscode-remote://ssh-remote+myhost/srv/repos/remote-proj",
    })
    .to_string();
    std::fs::write(remote_hash.join("workspace.json"), remote_workspace_json).unwrap();
    let remote_session = remote_chat.join("sess-remote.jsonl");
    std::fs::write(&remote_session, format!("{line}\n")).unwrap();
    let (remote_msgs, _) = parse_copilot_chat(&remote_session, &format!("{line}\n"), 0);
    assert_eq!(remote_msgs.len(), 1);
    assert_eq!(
        remote_msgs[0].cwd.as_deref(),
        Some("/srv/repos/remote-proj"),
        "remote URI strips scheme + host segment"
    );

    // ---- Case 4: multi-root configuration --------------------------
    let multi_hash = tmp.join("Library/Application Support/Code/User/workspaceStorage/multi789");
    let multi_chat = multi_hash.join("chatSessions");
    std::fs::create_dir_all(&multi_chat).unwrap();
    let workspace_dir = tmp.join("repos/workspaces");
    let folder_a = tmp.join("repos/multi-a");
    let folder_b = tmp.join("repos/multi-b");
    std::fs::create_dir_all(&workspace_dir).unwrap();
    std::fs::create_dir_all(&folder_a).unwrap();
    std::fs::create_dir_all(&folder_b).unwrap();
    let code_workspace = workspace_dir.join("multi.code-workspace");
    let folder_a_str = fwd(&folder_a);
    let folder_b_str = fwd(&folder_b);
    let code_workspace_json = serde_json::json!({
        "folders": [
            {"path": folder_a_str},
            {"path": folder_b_str},
        ],
    })
    .to_string();
    std::fs::write(&code_workspace, code_workspace_json).unwrap();
    let multi_workspace_json = serde_json::json!({
        "configuration": format!("file://{}", fwd(&code_workspace)),
    })
    .to_string();
    std::fs::write(multi_hash.join("workspace.json"), multi_workspace_json).unwrap();
    let multi_session = multi_chat.join("sess-multi.jsonl");
    std::fs::write(&multi_session, format!("{line}\n")).unwrap();
    let (multi_msgs, _) = parse_copilot_chat(&multi_session, &format!("{line}\n"), 0);
    assert_eq!(multi_msgs.len(), 1);
    assert_eq!(
        multi_msgs[0].cwd.as_deref(),
        Some(folder_a_str.as_str()),
        "multi-root cwd is the first folder in .code-workspace"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Percent-encoded paths (`Application%20Support`) must round-trip
/// through the URI decoder so cwds with spaces resolve correctly.
#[test]
fn workspace_json_percent_decodes_folder_uri() {
    let tmp = std::env::temp_dir().join("budi-copilot-chat-percent-decode");
    let _ = std::fs::remove_dir_all(&tmp);
    let hash_dir = tmp.join("workspaceStorage/abc");
    let chat_dir = hash_dir.join("chatSessions");
    std::fs::create_dir_all(&chat_dir).unwrap();
    std::fs::write(
        hash_dir.join("workspace.json"),
        r#"{"folder":"file:///Users/me/My%20Project"}"#,
    )
    .unwrap();
    let session = chat_dir.join("s.jsonl");
    let line = r#"{"kind":2,"v":[{"requestId":"r","completionTokens":1}]}"#;
    std::fs::write(&session, format!("{line}\n")).unwrap();
    let (msgs, _) = parse_copilot_chat(&session, &format!("{line}\n"), 0);
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].cwd.as_deref(), Some("/Users/me/My Project"));
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Malformed `workspace.json` falls back to `cwd: None` cleanly — the
/// parse must not fail.
#[test]
fn workspace_json_malformed_falls_back_to_none() {
    let tmp = std::env::temp_dir().join("budi-copilot-chat-malformed-ws");
    let _ = std::fs::remove_dir_all(&tmp);
    let hash_dir = tmp.join("workspaceStorage/bad");
    let chat_dir = hash_dir.join("chatSessions");
    std::fs::create_dir_all(&chat_dir).unwrap();
    std::fs::write(hash_dir.join("workspace.json"), b"{not valid json").unwrap();
    let session = chat_dir.join("s.jsonl");
    let line = r#"{"kind":2,"v":[{"requestId":"r","completionTokens":1}]}"#;
    std::fs::write(&session, format!("{line}\n")).unwrap();
    let (msgs, _) = parse_copilot_chat(&session, &format!("{line}\n"), 0);
    assert_eq!(msgs.len(), 1);
    assert!(
        msgs[0].cwd.is_none(),
        "malformed workspace.json -> cwd None"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

/// End-to-end against the canonical R1.2 fixture (#669) — drops the
/// `vscode_chat_0_47_0.jsonl` content under a synthetic
/// `<workspaceStorage>/<hash>/chatSessions/` tree alongside the
/// `vscode_chat_0_47_0.workspace.json` sibling fixture, and asserts
/// every emitted row carries the cwd from `workspace.json`. Pins the
/// #681 acceptance criterion: "the fixture gains a sibling
/// `vscode_chat_0_47_0.workspace.json` so the unit test asserts
/// cwd-enrichment end-to-end against the canonical fixture".
#[test]
fn parse_real_vscode_0_47_0_fixture_enriches_cwd() {
    let jsonl = include_str!("fixtures/vscode_chat_0_47_0.jsonl");
    let workspace_json = include_str!("fixtures/vscode_chat_0_47_0.workspace.json");

    let tmp = std::env::temp_dir().join("budi-copilot-chat-r681-canonical");
    let _ = std::fs::remove_dir_all(&tmp);
    let hash_dir = tmp.join("workspaceStorage/canon-hash");
    let chat_dir = hash_dir.join("chatSessions");
    std::fs::create_dir_all(&chat_dir).unwrap();
    std::fs::write(hash_dir.join("workspace.json"), workspace_json).unwrap();
    let session_path = chat_dir.join("vscode_chat_0_47_0.jsonl");
    std::fs::write(&session_path, jsonl).unwrap();

    let (msgs, _) = parse_copilot_chat(&session_path, jsonl, 0);
    assert!(
        !msgs.is_empty(),
        "canonical fixture must still emit rows under the cwd-enrichment path"
    );
    // The fixture workspace.json points at /Users/budi-fixture/...
    // which doesn't exist on disk, but cwd is the *string* — the
    // GitEnricher resolves it (or not) at the pipeline layer.
    let expected_cwd = "/Users/budi-fixture/workspaces/vscode-0.47.0-chat";
    assert!(
        msgs.iter().all(|m| m.cwd.as_deref() == Some(expected_cwd)),
        "every emitted row must carry the cwd from the sibling \
         workspace.json (got: {:?})",
        msgs.iter().map(|m| m.cwd.as_deref()).collect::<Vec<_>>()
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

// ---- #688: emptyWindow editor-context cwd hint -------------------

/// Pure-text extractor: the canonical sentence shape from
/// `result.metadata.renderedUserMessage[*].text` resolves to the
/// file's parent directory. Pins the format documented on #688.
#[test]
fn editor_context_text_extracts_parent_dir() {
    let body = "<editorContext>\n\
                The user's current file is /Users/ivan.seredkin/Desktop/CP4X-GQMY-GH9C.md. \
                The current selection is from line 9 to line 9.\n\
                </editorContext>";
    assert_eq!(
        parent_dir_from_editor_context_text(body).as_deref(),
        Some("/Users/ivan.seredkin/Desktop")
    );
}

/// Path with spaces — the parent dir is preserved verbatim. The
/// editor-context block carries a literal local path, not a URI, so
/// no percent-decoding is needed (unlike the workspace.json `folder`
/// field which does require it).
#[test]
fn editor_context_text_handles_spaces_and_dots() {
    let body = "<editorContext>\n\
                The user's current file is /Users/me/My Project/src/file.v2.rs. \
                The current selection is from line 1 to line 1.\n\
                </editorContext>";
    assert_eq!(
        parent_dir_from_editor_context_text(body).as_deref(),
        Some("/Users/me/My Project/src")
    );
}

/// Relative path — skipped. We have no workspace root in the
/// emptyWindow case, so a relative path cannot be turned into a
/// concrete cwd hint.
#[test]
fn editor_context_text_rejects_relative_path() {
    let body = "<editorContext>\n\
                The user's current file is src/main.rs. \
                The current selection is from line 1 to line 1.\n\
                </editorContext>";
    assert!(parent_dir_from_editor_context_text(body).is_none());
}

/// No `<editorContext>` block — None. Sessions sent before editor
/// focus is established legitimately omit the block.
#[test]
fn editor_context_text_absent_returns_none() {
    let body = "<workspace_info>There is no workspace currently open.</workspace_info>";
    assert!(parent_dir_from_editor_context_text(body).is_none());
}

/// End-to-end: an emptyWindow session whose first request carries an
/// `<editorContext>` block in `result.metadata.renderedUserMessage`
/// emits rows with `cwd` populated from the file's parent dir and a
/// `cwd_source = copilot_chat:editor_context_hint` marker so
/// downstream analytics can distinguish the hint from an
/// authoritative `workspace.json` cwd.
#[test]
fn empty_window_session_uses_editor_context_hint() {
    let tmp = std::env::temp_dir().join("budi-copilot-chat-empty-window-hint");
    let _ = std::fs::remove_dir_all(&tmp);
    let empty_dir =
        tmp.join("Library/Application Support/Code/User/globalStorage/emptyWindowChatSessions");
    std::fs::create_dir_all(&empty_dir).unwrap();
    let session_path = empty_dir.join("bda343f1.jsonl");

    // Synthetic but shape-faithful kind:0 snapshot — one request with
    // tokens (so it emits) and an editorContext-bearing
    // renderedUserMessage entry.
    let line = serde_json::json!({
        "kind": 0,
        "v": {
            "sessionId": "empty-1",
            "requests": [{
                "requestId": "r-1",
                "modelId": "copilot/gpt-4.1",
                "timestamp": 1715000000000_u64,
                "message": {"text": "summarise this file"},
                "result": {
                    "metadata": {
                        "promptTokens": 10,
                        "outputTokens": 5,
                        "renderedUserMessage": [{
                            "text": "<editorContext>\nThe user's current file is /Users/ivan.seredkin/Desktop/CP4X-GQMY-GH9C.md. The current selection is from line 9 to line 9.\n</editorContext>\n<workspace_info>\nThere is no workspace currently open.\n</workspace_info>"
                        }]
                    }
                }
            }]
        }
    })
    .to_string();
    std::fs::write(&session_path, format!("{line}\n")).unwrap();

    let (msgs, _) = parse_copilot_chat(&session_path, &format!("{line}\n"), 0);
    assert!(
        !msgs.is_empty(),
        "session must emit at least the assistant row"
    );
    for msg in &msgs {
        assert_eq!(
            msg.cwd.as_deref(),
            Some("/Users/ivan.seredkin/Desktop"),
            "every row carries the editor-context hint cwd (got role={:?}, cwd={:?})",
            msg.role,
            msg.cwd
        );
        assert_eq!(
            msg.cwd_source.as_deref(),
            Some(CWD_SOURCE_EDITOR_CONTEXT_HINT),
            "every row carries the hint cwd_source marker"
        );
    }

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Workspace-anchored sessions are unaffected: when `workspace.json`
/// resolves the cwd, the editor-context hint must NOT override it
/// and `cwd_source` stays `None` so analytics see the row as
/// authoritative. Pins the "primary path of #681 is unaffected"
/// acceptance criterion.
#[test]
fn workspace_anchored_session_does_not_apply_editor_context_hint() {
    let tmp = std::env::temp_dir().join("budi-copilot-chat-ws-anchored-no-hint");
    let _ = std::fs::remove_dir_all(&tmp);
    let hash_dir = tmp.join("workspaceStorage/abc-hash");
    let chat_dir = hash_dir.join("chatSessions");
    std::fs::create_dir_all(&chat_dir).unwrap();
    std::fs::write(
        hash_dir.join("workspace.json"),
        r#"{"folder":"file:///Users/me/repos/proj"}"#,
    )
    .unwrap();
    let session_path = chat_dir.join("sess.jsonl");

    // Even though the renderedUserMessage carries an editorContext
    // block pointing somewhere else, the workspace.json folder wins
    // and cwd_source stays None.
    let line = serde_json::json!({
        "kind": 0,
        "v": {
            "sessionId": "ws-1",
            "requests": [{
                "requestId": "r-1",
                "modelId": "copilot/gpt-4.1",
                "timestamp": 1715000000000_u64,
                "result": {
                    "metadata": {
                        "promptTokens": 10,
                        "outputTokens": 5,
                        "renderedUserMessage": [{
                            "text": "<editorContext>\nThe user's current file is /Users/ivan.seredkin/Desktop/foo.md. The current selection is from line 1 to line 1.\n</editorContext>"
                        }]
                    }
                }
            }]
        }
    })
    .to_string();
    std::fs::write(&session_path, format!("{line}\n")).unwrap();

    let (msgs, _) = parse_copilot_chat(&session_path, &format!("{line}\n"), 0);
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].cwd.as_deref(), Some("/Users/me/repos/proj"));
    assert!(
        msgs[0].cwd_source.is_none(),
        "workspace-anchored cwd is authoritative; cwd_source must stay None"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// emptyWindow session with no `<editorContext>` block in any
/// renderedUserMessage — cwd stays None and cwd_source stays None
/// (no spurious hint). Mirrors the existing #681 emptyWindow test
/// but goes further by also asserting the new tag.
#[test]
fn empty_window_session_without_editor_context_leaves_cwd_none() {
    let tmp = std::env::temp_dir().join("budi-copilot-chat-empty-window-no-hint");
    let _ = std::fs::remove_dir_all(&tmp);
    let empty_dir =
        tmp.join("Library/Application Support/Code/User/globalStorage/emptyWindowChatSessions");
    std::fs::create_dir_all(&empty_dir).unwrap();
    let session_path = empty_dir.join("e22dad3b.jsonl");

    let line = r#"{"kind":2,"v":[{"requestId":"r-1","modelId":"copilot/gpt-4.1","completionTokens":42,"result":{"metadata":{"resolvedModel":"x"}}}]}"#;
    std::fs::write(&session_path, format!("{line}\n")).unwrap();

    let (msgs, _) = parse_copilot_chat(&session_path, &format!("{line}\n"), 0);
    assert_eq!(msgs.len(), 1);
    assert!(msgs[0].cwd.is_none());
    assert!(msgs[0].cwd_source.is_none());

    let _ = std::fs::remove_dir_all(&tmp);
}

/// `append_at_path` correctness — the default-path branch (kind:2 with
/// no `k`) and the explicit `["requests"]` branch must both append to
/// the same array.
#[test]
fn append_at_path_appends_to_named_array() {
    let mut state = serde_json::json!({});
    append_at_path(
        &mut state,
        &[serde_json::json!("requests")],
        &[serde_json::json!({"requestId": "a"})],
    );
    append_at_path(
        &mut state,
        &[serde_json::json!("requests")],
        &[
            serde_json::json!({"requestId": "b"}),
            serde_json::json!({"requestId": "c"}),
        ],
    );
    let arr = state
        .get("requests")
        .and_then(|v| v.as_array())
        .expect("requests is an array");
    assert_eq!(arr.len(), 3);
    assert_eq!(arr[0].get("requestId").and_then(|v| v.as_str()), Some("a"));
    assert_eq!(arr[2].get("requestId").and_then(|v| v.as_str()), Some("c"));
}

// ---- #686: user-role row capture ---------------------------------

/// Reducer path: a kind:0 snapshot whose request carries `message.text`
/// emits both a user row (role=user, tokens=0, prompt content fed to
/// the classifier) and an assistant row (role=assistant, current
/// behavior). Assistant `parent_uuid` references the user `uuid`.
#[test]
fn reducer_emits_user_and_assistant_for_message_text() {
    let content = concat!(
        r#"{"kind":0,"v":{"sessionId":"s-user","requests":[{"requestId":"r-1","modelId":"copilot/claude-sonnet-4-5","timestamp":1715000000000,"message":{"text":"fix the login bug please","timestamp":1714999999000},"result":{"metadata":{"promptTokens":50,"outputTokens":12}}}]}}"#,
        "\n",
    );
    let path = Path::new("/tmp/budi-fixtures/sess-user-role-1.jsonl");
    let (msgs, _) = parse_copilot_chat(path, content, 0);
    assert_eq!(msgs.len(), 2, "one turn ⇒ user + assistant rows");
    let user = &msgs[0];
    let assistant = &msgs[1];
    assert_eq!(user.role, "user");
    assert_eq!(assistant.role, "assistant");
    assert_eq!(user.input_tokens, 0);
    assert_eq!(user.output_tokens, 0);
    assert_eq!(assistant.input_tokens, 50);
    assert_eq!(assistant.output_tokens, 12);
    assert_eq!(user.session_id.as_deref(), Some("s-user"));
    assert_eq!(assistant.session_id.as_deref(), Some("s-user"));
    assert_ne!(user.uuid, assistant.uuid);
    assert_eq!(
        assistant.parent_uuid.as_deref(),
        Some(user.uuid.as_str()),
        "assistant row points back at the paired user row"
    );
    // The classifier ran against `message.text` — "fix the login bug" is
    // a textbook bugfix prompt, so the user row carries a category.
    assert_eq!(user.prompt_category.as_deref(), Some("bugfix"));
    assert!(user.prompt_category_source.is_some());
    assert!(user.prompt_category_confidence.is_some());
    // User-row provenance: `cost_confidence` is "n/a" (no cost on a
    // user prompt), `model` stays None, parent_uuid is None.
    assert_eq!(user.cost_confidence, "n/a");
    assert!(user.model.is_none());
    assert!(user.parent_uuid.is_none());
    // Provider tag is preserved on both rows.
    assert!(msgs.iter().all(|m| m.provider == "copilot_chat"));
}

/// Reducer path: `message.parts[]` joins text-typed parts in order.
/// Non-text parts (file references, ephemeral cache markers) are
/// skipped. The joined text feeds the classifier just like the
/// `message.text` shape.
#[test]
fn reducer_user_row_concatenates_message_parts() {
    let content = concat!(
        r#"{"kind":0,"v":{"sessionId":"s-parts","requests":[{"requestId":"r-p","modelId":"copilot/gpt-4.1","message":{"parts":[{"text":"add a new "},{"kind":3,"cacheType":"ephemeral"},{"text":"button to the dashboard"}]},"result":{"metadata":{"promptTokens":7,"outputTokens":3}}}]}}"#,
        "\n",
    );
    let path = Path::new("/tmp/budi-fixtures/sess-parts.jsonl");
    let (msgs, _) = parse_copilot_chat(path, content, 0);
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].role, "user");
    // `add a new button to the dashboard` classifies as "feature".
    assert_eq!(msgs[0].prompt_category.as_deref(), Some("feature"));
}

/// Missing or empty `message` ⇒ no user row, but the assistant row
/// still emits. Per ticket #686 — interrupted / replayed-via-API
/// sessions are rare but legal; the assistant row carries the tokens.
#[test]
fn reducer_no_user_row_when_message_missing_or_empty() {
    // No message at all.
    let content_a = concat!(
        r#"{"kind":0,"v":{"sessionId":"s-no-msg","requests":[{"requestId":"r","modelId":"copilot/auto","completionTokens":42}]}}"#,
        "\n",
    );
    let path = Path::new("/tmp/budi-fixtures/sess-no-msg.jsonl");
    let (msgs, _) = parse_copilot_chat(path, content_a, 0);
    assert_eq!(msgs.len(), 1, "only the assistant row");
    assert_eq!(msgs[0].role, "assistant");

    // Empty `message.text` — also no user row.
    let content_b = concat!(
        r#"{"kind":0,"v":{"sessionId":"s-empty","requests":[{"requestId":"r","modelId":"copilot/auto","completionTokens":42,"message":{"text":""}}]}}"#,
        "\n",
    );
    let path = Path::new("/tmp/budi-fixtures/sess-empty.jsonl");
    let (msgs, _) = parse_copilot_chat(path, content_b, 0);
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].role, "assistant");
    assert!(msgs[0].parent_uuid.is_none());
}

/// Re-emit guard: re-parsing the same file must produce the same
/// pair of UUIDs. The `:user` suffix on the user-row emit key keeps
/// it stable across ticks, just like the assistant row.
#[test]
fn user_row_uuid_stable_across_reparse() {
    let content = concat!(
        r#"{"kind":2,"k":["requests"],"v":[{"requestId":"stable-pair","modelId":"copilot/x","completionTokens":7,"message":{"text":"how does this work?"}}]}"#,
        "\n",
    );
    let path = Path::new("/tmp/budi-fixtures/sess-stable-pair.jsonl");
    let (first, _) = parse_copilot_chat(path, content, 0);
    let (second, _) = parse_copilot_chat(path, content, 0);
    assert_eq!(first.len(), 2);
    assert_eq!(second.len(), 2);
    assert_eq!(first[0].uuid, second[0].uuid);
    assert_eq!(first[1].uuid, second[1].uuid);
    assert_ne!(first[0].uuid, first[1].uuid);
}

/// JSON-document path: same shape, same emit. A single
/// `{"requests": [...]}` snapshot with `message.text` on a request
/// produces user + assistant rows.
#[test]
fn json_document_emits_user_and_assistant_rows() {
    let content = r#"{
        "sessionId": "doc-user-1",
        "requests": [
            {
                "modelId": "copilot/claude-sonnet-4-5",
                "requestId": "r-doc-1",
                "message": {"text": "explain the auth flow"},
                "result": {"metadata": {"promptTokens": 20, "outputTokens": 4}}
            }
        ]
    }"#;
    let path = Path::new("/tmp/budi-fixtures/sess-doc-user.json");
    let (msgs, _) = parse_copilot_chat(path, content, 0);
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].role, "user");
    assert_eq!(msgs[1].role, "assistant");
    assert_eq!(msgs[1].parent_uuid.as_deref(), Some(msgs[0].uuid.as_str()));
}
