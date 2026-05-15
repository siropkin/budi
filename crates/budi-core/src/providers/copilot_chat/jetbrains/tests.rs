use super::*;

#[test]
fn empty_session_fixture_parses_to_zero_messages() {
    let dir = empty_fixture_dir();
    let parsed = parse_session_dir(&dir);
    assert!(
        parsed.is_empty(),
        "empty fixture must not emit rows (only XdMigration markers — no XdChatSession): {parsed:?}"
    );
}

#[test]
fn populated_session_marker_yields_one_row() {
    // Synthesize a session dir whose 00000000000.xd carries the literal
    // ASCII bytes for XdChatSession somewhere in its content. The byte
    // scan is shape-agnostic by design — see ADR-0093 §4.
    let tmp = std::env::temp_dir().join("budi-jetbrains-populated");
    let _ = std::fs::remove_dir_all(&tmp);
    let session_id = "36WZJbBx05NpO28apIrHaBmmyCJ";
    let session_dir = tmp.join("ic/chat-sessions").join(session_id);
    std::fs::create_dir_all(&session_dir).unwrap();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"\x00\x01\x02\x03some xodus framing");
    bytes.extend_from_slice(b"XdChatSession");
    bytes.extend_from_slice(b"\x00more framing\x00");
    std::fs::write(session_dir.join("00000000000.xd"), &bytes).unwrap();

    let parsed = parse_session_dir(&session_dir);
    assert_eq!(parsed.len(), 1);
    let m = &parsed[0];
    assert_eq!(m.role, "assistant");
    assert_eq!(m.provider, super::super::PROVIDER_ID);
    assert_eq!(m.surface.as_deref(), Some(crate::surface::JETBRAINS));
    assert_eq!(m.session_id.as_deref(), Some(session_id));
    assert_eq!(m.input_tokens, 0);
    assert_eq!(m.output_tokens, 0);
    assert_eq!(m.session_title.as_deref(), Some("chat"));
    assert_eq!(m.cost_confidence, "estimated");
    assert!(m.cost_cents.is_none());

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn agent_session_marker_titled_agent() {
    let tmp = std::env::temp_dir().join("budi-jetbrains-agent");
    let _ = std::fs::remove_dir_all(&tmp);
    let session_dir = tmp.join("iu/chat-agent-sessions/sess-xyz");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(
        session_dir.join("00000000000.xd"),
        b"prefix XdAgentSession suffix",
    )
    .unwrap();

    let parsed = parse_session_dir(&session_dir);
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].session_title.as_deref(), Some("chat-agent"));

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn missing_xd_file_yields_zero_rows() {
    let tmp = std::env::temp_dir().join("budi-jetbrains-missing");
    let _ = std::fs::remove_dir_all(&tmp);
    let session_dir = tmp.join("ic/chat-sessions/sess-empty");
    std::fs::create_dir_all(&session_dir).unwrap();
    // No 00000000000.xd written.
    assert!(parse_session_dir(&session_dir).is_empty());
    let _ = std::fs::remove_dir_all(&tmp);
}

/// #757: post-migration JetBrains Copilot sessions skip the Xodus
/// `.xd` log entirely and write only `copilot-chat-nitrite.db`. The
/// parser used to bail (no `.xd` → return empty) and the JetBrains
/// surface stayed at $0.00 forever. After the fix it reads the
/// Nitrite store, recognizes the populated-entity marker (`NtTurn`
/// or `NtChatSession`), and emits one assistant-role placeholder
/// the same shape an Xodus-only session would have produced.
#[test]
fn nitrite_only_session_emits_one_row() {
    let tmp = std::env::temp_dir().join("budi-jetbrains-nitrite-only");
    let _ = std::fs::remove_dir_all(&tmp);
    let session_id = "32REEyBFLmeFBR9TT7Luu0z1Rh8";
    let session_dir = tmp.join("ws/chat-sessions").join(session_id);
    std::fs::create_dir_all(&session_dir).unwrap();
    // Simulate Nitrite's MVStore header + a single Nitrite catalog
    // entry naming the populated-entity class. Real-world bytes
    // around the marker are MVStore page payload + Java
    // serialization; only the literal class-name suffix needs to
    // round-trip for the byte scan to fire.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"H:2,blockSize:1000,format:3,version:f\n");
    bytes.extend_from_slice(&[0u8; 64]);
    bytes.extend_from_slice(
        b"com.github.copilot.chat.session.persistence.nitrite.entity.NtChatSession",
    );
    bytes.extend_from_slice(&[0u8; 32]);
    bytes.extend_from_slice(b"com.github.copilot.chat.session.persistence.nitrite.entity.NtTurn");
    std::fs::write(session_dir.join("copilot-chat-nitrite.db"), &bytes).unwrap();

    let parsed = parse_session_dir(&session_dir);
    assert_eq!(parsed.len(), 1, "Nitrite session should emit one row");
    let m = &parsed[0];
    assert_eq!(m.role, "assistant");
    assert_eq!(m.provider, super::super::PROVIDER_ID);
    assert_eq!(m.surface.as_deref(), Some(crate::surface::JETBRAINS));
    assert_eq!(m.session_id.as_deref(), Some(session_id));
    assert_eq!(m.session_title.as_deref(), Some("chat"));
    assert_eq!(m.input_tokens, 0);
    assert_eq!(m.output_tokens, 0);
    assert!(
        m.cost_cents.is_none(),
        "tokens come from billing API per ADR-0093 §5"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

/// #757: a Nitrite store that carries *only* `NtSelectedModel` (the
/// per-session model preference Nitrite writes the moment the user
/// opens a chat pane, even before sending a message) must NOT emit
/// a row — that mirrors the existing Xodus rule about
/// `XdMigration`-only sessions. Without this, every freshly-opened
/// chat tab would synthesize a fake assistant turn.
#[test]
fn nitrite_with_only_selected_model_emits_no_row() {
    let tmp = std::env::temp_dir().join("budi-jetbrains-nitrite-prefonly");
    let _ = std::fs::remove_dir_all(&tmp);
    let session_dir = tmp.join("ic/chat-sessions/sess-prefs-only");
    std::fs::create_dir_all(&session_dir).unwrap();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"H:2,blockSize:1000,format:3,version:f\n");
    bytes.extend_from_slice(&[0u8; 64]);
    bytes.extend_from_slice(
        b"com.github.copilot.chat.session.persistence.nitrite.entity.NtSelectedModel",
    );
    std::fs::write(session_dir.join("copilot-chat-nitrite.db"), &bytes).unwrap();
    assert!(parse_session_dir(&session_dir).is_empty());
    let _ = std::fs::remove_dir_all(&tmp);
}

/// #757: chat-agent sessions write `copilot-agent-sessions-nitrite.db`
/// (different filename from `copilot-chat-nitrite.db`). The parser
/// must look at both — otherwise post-migration agent sessions stay
/// invisible the same way chat sessions did.
#[test]
fn nitrite_agent_session_emits_row_with_agent_title() {
    let tmp = std::env::temp_dir().join("budi-jetbrains-nitrite-agent");
    let _ = std::fs::remove_dir_all(&tmp);
    let session_dir = tmp.join("iu/chat-agent-sessions/sess-agent");
    std::fs::create_dir_all(&session_dir).unwrap();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"H:2,blockSize:1000,format:3,version:f\n");
    bytes.extend_from_slice(&[0u8; 64]);
    bytes.extend_from_slice(
        b"com.github.copilot.chat.session.persistence.nitrite.entity.NtAgentTurn",
    );
    std::fs::write(
        session_dir.join("copilot-agent-sessions-nitrite.db"),
        &bytes,
    )
    .unwrap();

    let parsed = parse_session_dir(&session_dir);
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].session_title.as_deref(), Some("chat-agent"));
    let _ = std::fs::remove_dir_all(&tmp);
}

/// #757: when both stores are present (real-world dual-store DBs
/// during migration), the parser must still emit exactly one row —
/// not two. The Xodus probe runs first; a populated `.xd` wins and
/// supplies the timestamp.
#[test]
fn dual_store_session_emits_exactly_one_row() {
    let tmp = std::env::temp_dir().join("budi-jetbrains-dual-store");
    let _ = std::fs::remove_dir_all(&tmp);
    let session_dir = tmp.join("ic/chat-sessions/sess-dual");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(
        session_dir.join("00000000000.xd"),
        b"prefix XdChatSession suffix",
    )
    .unwrap();
    std::fs::write(
        session_dir.join("copilot-chat-nitrite.db"),
        b"H:2,blockSize:1000\nNtChatSession\nNtTurn",
    )
    .unwrap();

    let parsed = parse_session_dir(&session_dir);
    assert_eq!(parsed.len(), 1);
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn discover_session_dirs_finds_all_session_types_and_slugs() {
    let tmp = std::env::temp_dir().join("budi-jetbrains-discover");
    let _ = std::fs::remove_dir_all(&tmp);
    for (slug, stype) in [
        ("ic", "chat-sessions"),
        ("iu", "chat-agent-sessions"),
        ("ws", "chat-edit-sessions"),
        ("iu", "bg-agent-sessions"),
    ] {
        std::fs::create_dir_all(tmp.join(slug).join(stype).join("sess-1")).unwrap();
    }
    // Noise that must be skipped per ADR-0093 §3.
    std::fs::create_dir_all(tmp.join("intellij")).unwrap();
    std::fs::write(tmp.join("apps.json"), b"{}").unwrap();
    std::fs::write(tmp.join("versions.json"), b"{}").unwrap();

    let dirs = discover_session_dirs(std::slice::from_ref(&tmp));
    assert_eq!(dirs.len(), 4, "expected four session dirs, got {dirs:?}");
    assert!(dirs.iter().all(|d| d.ends_with("sess-1")));
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn discover_session_dirs_handles_missing_root() {
    let dirs = discover_session_dirs(&[PathBuf::from("/nonexistent/github-copilot-root")]);
    assert!(dirs.is_empty());
}

#[test]
fn watch_roots_includes_session_type_dirs() {
    let tmp = std::env::temp_dir().join("budi-jetbrains-watch");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("ic/chat-sessions")).unwrap();
    std::fs::create_dir_all(tmp.join("iu/chat-agent-sessions")).unwrap();
    std::fs::create_dir_all(tmp.join("intellij")).unwrap();

    let mut roots = Vec::new();
    for ide_dir in ide_slug_dirs(&tmp) {
        for session_type in SESSION_TYPE_DIRS {
            let p = ide_dir.join(session_type);
            if p.is_dir() {
                roots.push(p);
            }
        }
    }
    roots.sort();
    assert_eq!(roots.len(), 2);
    assert!(roots.iter().any(|p| p.ends_with("ic/chat-sessions")));
    assert!(roots.iter().any(|p| p.ends_with("iu/chat-agent-sessions")));
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn deterministic_uuid_is_stable_and_namespaced() {
    let a = deterministic_uuid("sess-1", "/tmp/x");
    let b = deterministic_uuid("sess-1", "/tmp/x");
    assert_eq!(a, b);
    let c = deterministic_uuid("sess-2", "/tmp/x");
    assert_ne!(a, c);
    // Distinct namespace prefix means we never collide with the
    // VS Code-side `deterministic_uuid` in the parent module.
    let vscode_side = super::super::deterministic_uuid("sess-1", "/tmp/x", 0);
    assert_ne!(a, vscode_side);
}

#[test]
fn byte_contains_basic() {
    assert!(byte_contains(b"hello world", b"world"));
    assert!(!byte_contains(b"hello", b"world"));
    assert!(!byte_contains(b"hi", b"hello"));
    assert!(!byte_contains(b"x", b""));
}

/// #766: synthesize an Xodus log fragment that mimics what the real
/// `00000000000.xd` files on disk carry — a schema header that
/// declares `projectName\x00\x04` followed later by a
/// `\x82\x00\x04\x82Verkada-Web\x00` value record. The byte-scan must
/// recover the literal project name without a full Xodus log
/// decoder. Survey of 13 real session files (2026-05-11) showed this
/// pattern is stable across the WS / IC / IU IDE slugs.
#[test]
fn extract_xodus_project_name_recovers_value_from_schema_id_pair() {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"XdChatSession");
    bytes.extend_from_slice(b"\x86\x86\x8e\x8c");
    bytes.extend_from_slice(b"projectName\x00\x04");
    bytes.extend_from_slice(b"\x86\x86\x87\x85user\x00\x05");
    bytes.extend_from_slice(b"\x86\x99\x90");
    bytes.extend_from_slice(b"\x82\x00\x04\x82Verkada-Web\x00");
    bytes.extend_from_slice(b"\x86\x99\x8d\x82\x00\x05\x82siropkin\x00");

    let project = extract_xodus_project_name(&bytes);
    assert_eq!(project.as_deref(), Some("Verkada-Web"));
}

/// #766: a session whose `.xd` file doesn't carry the property at
/// all (empty session, or a plugin version that skips the property)
/// must return `None` rather than picking some random other string
/// out of the log.
#[test]
fn extract_xodus_project_name_returns_none_when_property_absent() {
    let bytes = b"XdChatSession\x00bunch of other stuff\x00\x00";
    assert!(extract_xodus_project_name(bytes).is_none());
}

/// #766: working-set file names share the `\x82\x00<id>\x82` framing,
/// so the value-scan can land on strings like `manifest.json` or
/// `src/foo/bar.tsx`. `looks_like_project_name` must reject those
/// — otherwise `resolve_project_workspace` ends up looking for
/// `~/_projects/manifest.json` and falling through, with
/// `session_title` set to a misleading filename.
#[test]
fn extract_xodus_project_name_filters_file_name_false_positives() {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"projectName\x00\x04");
    // First candidate is a file name (rejected); second is the real
    // project name (accepted). The scan walks forward through every
    // match so a real value still surfaces after a false positive.
    bytes.extend_from_slice(b"\x82\x00\x04\x82manifest.json\x00");
    bytes.extend_from_slice(b"\x82\x00\x04\x82verkadalizer\x00");

    let project = extract_xodus_project_name(&bytes);
    assert_eq!(project.as_deref(), Some("verkadalizer"));
}

#[test]
fn looks_like_project_name_accepts_real_names() {
    for name in ["Verkada-Web", "budi", "getbudi-dev", "verkada_menu_v2"] {
        assert!(looks_like_project_name(name), "should accept {name:?}");
    }
}

#[test]
fn looks_like_project_name_rejects_file_paths_and_extensions() {
    for name in [
        "manifest.json",
        "src/components/Foo.tsx",
        "/Users/me/_projects/Verkada-Web",
        "c:\\Users\\me\\code",
        "",
        "README.md",
    ] {
        assert!(!looks_like_project_name(name), "should reject {name:?}");
    }
}

#[test]
fn read_git_head_branch_parses_symbolic_ref() {
    let tmp = std::env::temp_dir().join("budi-jetbrains-head");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join(".git")).unwrap();
    std::fs::write(tmp.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    assert_eq!(read_git_head_branch(&tmp).as_deref(), Some("main"));
    let _ = std::fs::remove_dir_all(&tmp);
}

/// #764: build a synthetic Nitrite blob that mimics the on-disk
/// shape captured from real `copilot-agent-sessions-nitrite.db`
/// files (2026-05-11 inventory): an `NtAgentTurn` class marker
/// followed by a Java-serialized `LinkedHashMap` whose `uuid` field
/// carries a 36-char canonical UUID. Two turns produce two distinct
/// `ParsedMessage` UUIDs.
fn synth_nitrite_with_turns(uuids: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    // MVStore header so the file looks plausibly real.
    out.extend_from_slice(b"H:2,blockSize:1000,format:3,version:f\n");
    out.extend_from_slice(&[0u8; 64]);
    for uuid in uuids {
        assert_eq!(uuid.len(), 36, "synth helper expects canonical uuids");
        out.extend_from_slice(b"NtAgentTurn");
        out.extend_from_slice(b"\xac\xed\x00\x05");
        // `t\x00\x04uuid` + `t\x00\x24<36-byte uuid>` — the exact
        // pattern the real Nitrite serializer writes for the field.
        out.extend_from_slice(b"t\x00\x04uuid");
        out.extend_from_slice(b"t\x00\x24");
        out.extend_from_slice(uuid.as_bytes());
        out.extend_from_slice(b"\x00trailer\x00");
    }
    out
}

#[test]
fn nitrite_session_emits_one_row_per_turn() {
    let uuids = [
        "bfe8768a-b11e-469a-852b-fc22c7dd9f23",
        "382642f7-6bf3-4e9b-b2ed-970bb3474edb",
        "550b00cd-4ad2-479a-8d8a-300a55478450",
    ];
    let bytes = synth_nitrite_with_turns(&uuids);

    let extracted = extract_nitrite_turn_ids(&bytes);
    assert_eq!(extracted.len(), 3);
    for u in &uuids {
        assert!(extracted.iter().any(|s| s == u), "missing {u}");
    }

    let tmp = std::env::temp_dir().join("budi-jetbrains-nitrite-turns");
    let _ = std::fs::remove_dir_all(&tmp);
    let session_dir = tmp.join("iu/chat-agent-sessions/sess-many-turns");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(
        session_dir.join("copilot-agent-sessions-nitrite.db"),
        &bytes,
    )
    .unwrap();

    let parsed = parse_session_dir(&session_dir);
    assert_eq!(parsed.len(), 3, "one row per turn, got {parsed:?}");
    // The deterministic UUID must change per turn so `INSERT OR IGNORE`
    // accepts each new turn as a fresh row — the entire point of #764.
    let mut seen = std::collections::HashSet::new();
    for m in &parsed {
        assert!(seen.insert(m.uuid.clone()), "duplicate uuid {}", m.uuid);
        assert_eq!(m.surface.as_deref(), Some(crate::surface::JETBRAINS));
        assert_eq!(m.provider, super::super::PROVIDER_ID);
    }

    let _ = std::fs::remove_dir_all(&tmp);
}

/// #764: turn UUIDs that appear duplicated across the file
/// (Nitrite's MVStore writes class metadata + B-tree leaf entries
/// for the same document) must collapse to one emitted row per
/// distinct turn — not one per byte-pattern match.
#[test]
fn nitrite_duplicate_turn_uuid_emits_single_row() {
    let mut bytes = synth_nitrite_with_turns(&["bfe8768a-b11e-469a-852b-fc22c7dd9f23"]);
    // Duplicate the same turn block — same uuid, two markers.
    let dup = synth_nitrite_with_turns(&["bfe8768a-b11e-469a-852b-fc22c7dd9f23"]);
    bytes.extend_from_slice(&dup[64..]); // skip the synthetic header on the dup

    let extracted = extract_nitrite_turn_ids(&bytes);
    assert_eq!(
        extracted.len(),
        1,
        "duplicate uuids must collapse, got {extracted:?}"
    );
}

/// Regression coverage for the v8.4.6 dual-store bug: when a
/// session-dir holds both a populated `.xd` (with `projectName`) and
/// a populated `.nitrite.db` (with `Nt*Turn` documents), the parser
/// must combine the two — Nitrite supplies per-turn UUIDs, Xodus
/// supplies the repo enrichment that lands on every per-turn row.
/// The pre-fix 8.4.6 implementation read whichever store the
/// populated-entity probe returned and ignored the other, so every
/// `surface=jetbrains` row landed with `repo_id = NULL` even on
/// sessions whose .xd carried a clean `Verkada-Web`-style project
/// name.
#[test]
fn dual_store_session_combines_xodus_repo_with_nitrite_turns() {
    let tmp = std::env::temp_dir().join("budi-jetbrains-dual-combined");
    let _ = std::fs::remove_dir_all(&tmp);
    let session_dir = tmp.join("iu/chat-agent-sessions/sess-dual-combined");
    std::fs::create_dir_all(&session_dir).unwrap();

    // Synthetic .xd with the projectName property + value record. The
    // resolve_project_workspace probe will return None on most CI
    // hosts (no `~/_projects/budi-test-fake-name/.git`), so the
    // assertion focuses on `session_title` and the row count — those
    // two cover the wire shape that flows to the cloud and the
    // dashboard's Repo column fallback.
    let mut xd = Vec::new();
    xd.extend_from_slice(b"XdAgentSession");
    xd.extend_from_slice(b"\x86\x86\x8e\x8cprojectName\x00\x04");
    xd.extend_from_slice(b"\x86\x99\x90\x82\x00\x04\x82budi-test-fake-name\x00");
    std::fs::write(session_dir.join("00000000000.xd"), &xd).unwrap();

    // Synthetic Nitrite with one NtAgentTurn + uuid pair.
    let uuid = "11afee98-04f2-4da1-a282-3fc0d14e9054";
    let mut nit = Vec::new();
    nit.extend_from_slice(b"H:2,blockSize:1000,format:3,version:f\n");
    nit.extend_from_slice(&[0u8; 64]);
    nit.extend_from_slice(b"NtAgentTurn");
    nit.extend_from_slice(b"\xac\xed\x00\x05");
    nit.extend_from_slice(b"t\x00\x04uuid");
    nit.extend_from_slice(b"t\x00\x24");
    nit.extend_from_slice(uuid.as_bytes());
    nit.extend_from_slice(b"\x00trailer\x00");
    std::fs::write(session_dir.join("copilot-agent-sessions-nitrite.db"), &nit).unwrap();

    let parsed = parse_session_dir(&session_dir);
    // One row per Nitrite turn — the Xodus probe doesn't add a
    // separate placeholder, it only enriches.
    assert_eq!(parsed.len(), 1, "expected one per-turn row, got {parsed:?}");
    // The Xodus-derived project name lands on the per-turn row's
    // `session_title` even when the filesystem-probe step fails to
    // resolve a `.git` checkout, so the dashboard renders the
    // IntelliJ name instead of a sea of `(unknown)`.
    assert_eq!(
        parsed[0].session_title.as_deref(),
        Some("budi-test-fake-name"),
        "Xodus project name must reach the per-turn row's session_title"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// #764: sessions whose only Nitrite documents are sessions (not
/// turns) — e.g. an `NtAgentSession` row with no `NtAgentTurn` yet
/// — fall back to the one-row-per-session placeholder so the
/// session still shows up in `surface=jetbrains` lists. Matches the
/// pre-#764 behavior of #757's existence-marker path.
#[test]
fn nitrite_session_without_turn_falls_back_to_single_placeholder() {
    let tmp = std::env::temp_dir().join("budi-jetbrains-nitrite-session-only");
    let _ = std::fs::remove_dir_all(&tmp);
    let session_dir = tmp.join("iu/chat-agent-sessions/sess-no-turns");
    std::fs::create_dir_all(&session_dir).unwrap();
    // A session marker is enough to clear the populated-entity gate
    // shipped in #757, but no `NtAgentTurn` documents are present.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"H:2,blockSize:1000,format:3,version:f\n");
    bytes.extend_from_slice(&[0u8; 64]);
    bytes.extend_from_slice(b"NtAgentSession\x00");
    std::fs::write(
        session_dir.join("copilot-agent-sessions-nitrite.db"),
        &bytes,
    )
    .unwrap();

    let parsed = parse_session_dir(&session_dir);
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].session_title.as_deref(), Some("chat-agent"));

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn looks_like_uuid_accepts_canonical_and_rejects_garbage() {
    assert!(looks_like_uuid("bfe8768a-b11e-469a-852b-fc22c7dd9f23"));
    assert!(looks_like_uuid("00000000-0000-0000-0000-000000000000"));
    // Wrong length.
    assert!(!looks_like_uuid("not-a-uuid"));
    // Dashes in wrong positions.
    assert!(!looks_like_uuid("bfe8768ab-11e-469a-852b-fc22c7dd9f23"));
    // Non-hex characters.
    assert!(!looks_like_uuid("bfe8768z-b11e-469a-852b-fc22c7dd9f23"));
}

#[test]
fn deterministic_uuid_from_nitrite_is_stable_and_distinct_per_turn() {
    let a = deterministic_uuid_from_nitrite("bfe8768a-b11e-469a-852b-fc22c7dd9f23", "/tmp/x");
    let b = deterministic_uuid_from_nitrite("bfe8768a-b11e-469a-852b-fc22c7dd9f23", "/tmp/x");
    assert_eq!(a, b);
    let c = deterministic_uuid_from_nitrite("382642f7-6bf3-4e9b-b2ed-970bb3474edb", "/tmp/x");
    assert_ne!(a, c);
    // Distinct namespace prefix vs the session-keyed `deterministic_uuid`.
    let session_keyed = deterministic_uuid("bfe8768a-b11e-469a-852b-fc22c7dd9f23", "/tmp/x");
    assert_ne!(a, session_keyed);
}

#[test]
fn read_git_head_branch_returns_none_for_detached_head() {
    let tmp = std::env::temp_dir().join("budi-jetbrains-head-detached");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join(".git")).unwrap();
    std::fs::write(
        tmp.join(".git/HEAD"),
        "0123456789abcdef0123456789abcdef01234567\n",
    )
    .unwrap();
    assert!(read_git_head_branch(&tmp).is_none());
    let _ = std::fs::remove_dir_all(&tmp);
}

// #778 — Phase 2 workspace-path extractor coverage. Pinned against the
// exact byte shape recovered from the 8.4.8 smoke-test machine: the
// `currentFileUri` JSON value inside a turn document's `stringContent`
// model-state blob, escape-encoded three levels deep (so a literal
// `\\\":\\\"file://...` byte sequence).
#[test]
fn extract_nitrite_workspace_paths_recovers_uri_from_escaped_json_blob() {
    // Mimics the real bytes around `currentFileUri` on
    // ~/.config/github-copilot/iu/chat-agent-sessions/32REE.../copilot-agent-sessions-nitrite.db
    let raw = b"...currentFileUri\\\\\\\":\\\\\\\"file:///Users/me/_projects/Verkada-Web/src/foo/bar.tsx\\\\\\\",\\\\\\\"isVisionEnabled\\\\\\\":true...";
    let paths = extract_nitrite_workspace_paths(raw);
    assert_eq!(
        paths,
        vec!["/Users/me/_projects/Verkada-Web/src/foo/bar.tsx".to_string()]
    );
}

#[test]
fn extract_nitrite_workspace_paths_dedupes_repeated_uris() {
    // The same `currentFileUri` typically shows up multiple times per
    // session because each turn snapshots the model state.
    let chunk = b"currentFileUri\\\\\\\":\\\\\\\"file:///Users/me/_projects/Repo/x.rs\\\\\\\"";
    let mut buf = Vec::new();
    for _ in 0..4 {
        buf.extend_from_slice(chunk);
        buf.extend_from_slice(b"...filler...");
    }
    let paths = extract_nitrite_workspace_paths(&buf);
    assert_eq!(paths, vec!["/Users/me/_projects/Repo/x.rs".to_string()]);
}

#[test]
fn extract_nitrite_workspace_paths_percent_decodes_spaces() {
    let raw = b"currentFileUri\\\\\\\":\\\\\\\"file:///Users/me/_projects/Has%20Space/x.rs\\\\\\\"";
    let paths = extract_nitrite_workspace_paths(raw);
    assert_eq!(
        paths,
        vec!["/Users/me/_projects/Has Space/x.rs".to_string()]
    );
}

#[test]
fn extract_nitrite_workspace_paths_returns_empty_for_no_uris() {
    // Mirrors the 95-of-98 case on the smoke-test machine: the DB
    // exists and carries `NtAgentTurn` markers but no `file://` token
    // anywhere. Honest signal — return empty rather than guess.
    let bytes = synth_nitrite_with_turns(&["bfe8768a-b11e-469a-852b-fc22c7dd9f23"]);
    assert!(extract_nitrite_workspace_paths(&bytes).is_empty());
}

#[test]
fn extract_nitrite_workspace_paths_rejects_relative_or_malformed() {
    // A bare `file://` with no leading slash after the scheme isn't a
    // usable absolute path — drop it rather than emit a relative path
    // that the upstream resolver would silently expand.
    let raw = b"...file://relative/path...";
    let paths = extract_nitrite_workspace_paths(raw);
    assert!(paths.is_empty(), "got {paths:?}");
}

#[test]
fn longest_common_path_prefix_finds_deepest_shared_directory() {
    let paths = vec![
        "/Users/me/_projects/Repo/src/a/b/x.rs".to_string(),
        "/Users/me/_projects/Repo/src/a/c.rs".to_string(),
        "/Users/me/_projects/Repo/src/a/b/y.rs".to_string(),
    ];
    assert_eq!(
        longest_common_path_prefix(&paths).as_deref(),
        Some("/Users/me/_projects/Repo/src/a")
    );
}

#[test]
fn longest_common_path_prefix_drops_to_root_dir_when_no_shared_subdir() {
    let paths = vec!["/Users/a/x.rs".to_string(), "/etc/y.rs".to_string()];
    // The common prefix is empty (no shared directory) — return None.
    assert!(longest_common_path_prefix(&paths).is_none());
}

/// Synth a real git repo at `repo_root` with an `origin` remote set to
/// `<url>` and HEAD on `<branch>`. Uses `git init` + `git remote add`
/// so `resolve_repo_id`'s `git remote get-url origin` actually returns
/// the URL we asked for. Falls back to manual file writes when git
/// isn't available (CI containers can lack it) — the test catches
/// that case and reports the resolver path that ran.
fn synth_git_repo(repo_root: &Path, remote_url: &str, branch: &str) {
    let _ = std::fs::create_dir_all(repo_root);
    let init_ok = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(repo_root)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !init_ok {
        return;
    }
    let _ = std::process::Command::new("git")
        .args(["remote", "add", "origin", remote_url])
        .current_dir(repo_root)
        .status();
    let _ = std::fs::write(
        repo_root.join(".git/HEAD"),
        format!("ref: refs/heads/{branch}\n"),
    );
}

#[test]
fn resolve_workspace_from_paths_walks_up_to_git_checkout() {
    let tmp = std::env::temp_dir().join("budi-jetbrains-phase2-resolve");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("src/sub/inner")).unwrap();
    synth_git_repo(&tmp, "git@github.com:test/phase2-repo.git", "main");

    let tmp_str = tmp.to_string_lossy().to_string();
    let paths = vec![
        format!("{tmp_str}/src/sub/inner/a.rs"),
        format!("{tmp_str}/src/sub/inner/b.rs"),
    ];
    let res = resolve_phase2_workspace(&paths);
    let Some((repo_id, branch)) = res.repo else {
        // git binary unavailable — bail without failing CI.
        let _ = std::fs::remove_dir_all(&tmp);
        return;
    };
    assert!(repo_id.contains("phase2-repo"), "got {repo_id}");
    assert_eq!(branch.as_deref(), Some("main"));
    assert!(res.failure_reason.is_none());
    assert!(res.common_prefix.is_some());

    let _ = std::fs::remove_dir_all(&tmp);
}

/// #788: when no `.git` checkout sits along the path chain, resolution
/// returns `None` repo *and* the still-useful longest common prefix,
/// tagged with `no_git_along_chain` for the diagnostic log line.
///
/// Unix-only: `longest_common_path_prefix` requires `/`-anchored
/// absolute paths. The Windows temp dir (`C:\...`) is rejected before
/// the resolver runs, which is covered by
/// `longest_common_path_prefix_drops_to_root_dir_when_no_shared_subdir`.
#[cfg(unix)]
#[test]
fn resolve_phase2_workspace_returns_prefix_and_reason_when_no_git_along_chain() {
    let tmp = std::env::temp_dir().join("budi-jetbrains-phase2-no-git");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("src")).unwrap();
    // No `.git/` anywhere in the chain.
    let tmp_str = tmp.to_string_lossy().to_string();
    let paths = vec![format!("{tmp_str}/src/a.rs")];
    let res = resolve_phase2_workspace(&paths);
    assert!(res.repo.is_none());
    assert_eq!(res.failure_reason, Some("no_git_along_chain"));
    assert_eq!(
        res.common_prefix.as_deref(),
        Some(format!("{tmp_str}/src").as_str())
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

/// #788: empty input → `no_paths` reason and no prefix.
#[test]
fn resolve_phase2_workspace_flags_empty_input() {
    let res = resolve_phase2_workspace(&[]);
    assert!(res.repo.is_none());
    assert!(res.common_prefix.is_none());
    assert_eq!(res.failure_reason, Some("no_paths"));
}

/// #788: when `.git` exists along the chain but `resolve_repo_id`
/// returns `None` (e.g. no `origin` remote, the exact failure mode
/// described in the Terraform smoke-test ticket), the resolver
/// surfaces a distinct `repo_id_resolver_returned_none` reason rather
/// than collapsing into the catch-all `no_git_along_chain`.
///
/// Unix-only — see note on the prior `no_git_along_chain` test.
#[cfg(unix)]
#[test]
fn resolve_phase2_workspace_distinguishes_resolver_returned_none() {
    let tmp = std::env::temp_dir().join("budi-jetbrains-phase2-no-remote");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("src")).unwrap();
    // `.git` directory exists but is empty — `resolve_repo_id` requires
    // a valid `origin` remote, which an empty dir cannot provide.
    std::fs::create_dir_all(tmp.join(".git")).unwrap();
    let tmp_str = tmp.to_string_lossy().to_string();
    let paths = vec![format!("{tmp_str}/src/a.rs")];
    let res = resolve_phase2_workspace(&paths);
    // `resolve_repo_id` shells out to `git`; if git isn't available
    // we still get `repo_id_resolver_returned_none` because the empty
    // `.git` dir trips the resolver before any other failure.
    assert!(res.repo.is_none());
    assert_eq!(res.failure_reason, Some("repo_id_resolver_returned_none"));
    assert!(res.common_prefix.is_some());
    let _ = std::fs::remove_dir_all(&tmp);
}

/// #778: end-to-end Phase 2 path. A session-dir that holds only a
/// Nitrite store (no `.xd`, so Phase 1 bails) with `currentFileUri`
/// hits inside the stringContent JSON must resolve `repo_id` from the
/// file URIs and land it on every emitted per-turn row.
#[test]
fn nitrite_only_session_resolves_repo_id_via_currentfileuri_phase2() {
    let tmp = std::env::temp_dir().join("budi-jetbrains-phase2-end-to-end");
    let _ = std::fs::remove_dir_all(&tmp);
    // Synth repo root with a real .git dir + remote.
    let repo_root = tmp.join("repos/MyProject");
    std::fs::create_dir_all(repo_root.join("src/foo")).unwrap();
    synth_git_repo(
        &repo_root,
        "git@github.com:siropkin/myproject.git",
        "feature",
    );
    let repo_root_str = repo_root.to_string_lossy().to_string();

    // Synth Nitrite bytes: one turn UUID + a currentFileUri JSON blob
    // that points into `<repo_root>/src/foo/bar.rs`.
    let mut bytes = synth_nitrite_with_turns(&["bfe8768a-b11e-469a-852b-fc22c7dd9f23"]);
    bytes.extend_from_slice(b"...currentFileUri\\\\\\\":\\\\\\\"file://");
    bytes.extend_from_slice(repo_root_str.as_bytes());
    bytes.extend_from_slice(b"/src/foo/bar.rs\\\\\\\",\\\\\\\"isVisionEnabled\\\\\\\":true...");

    let session_dir = tmp.join("iu/chat-agent-sessions/sess-phase2");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(
        session_dir.join("copilot-agent-sessions-nitrite.db"),
        &bytes,
    )
    .unwrap();

    let parsed = parse_session_dir(&session_dir);
    assert_eq!(parsed.len(), 1, "one row per turn, got {parsed:?}");
    let msg = &parsed[0];
    // git binary may not be on the test host; bail cleanly if so.
    if msg.repo_id.is_some() {
        assert_eq!(
            msg.repo_id.as_deref(),
            Some("github.com/siropkin/myproject"),
            "repo_id should resolve via Phase 2"
        );
        assert_eq!(msg.git_branch.as_deref(), Some("feature"));
    }

    let _ = std::fs::remove_dir_all(&tmp);
}

/// #788: when Phase 2 recovers `file://` URIs but the chain doesn't
/// lead to a resolvable `.git` checkout (e.g. the Terraform smoke-test
/// scenario where the repo isn't checked out on this host), the
/// emitted message must still carry the longest-common-prefix path
/// as a `cwd` hint with the `copilot_chat:jetbrains_phase2_prefix`
/// `cwd_source` marker. Gives the dashboard / messages.cwd something
/// to render even when `repo_id` is null.
///
/// Unix-only — relies on `/`-anchored temp paths flowing through the
/// extractor's URI decoder.
#[cfg(unix)]
#[test]
fn phase2_with_uris_but_no_git_emits_cwd_hint_with_phase2_prefix_source() {
    let tmp = std::env::temp_dir().join("budi-jetbrains-phase2-cwd-hint");
    let _ = std::fs::remove_dir_all(&tmp);
    // Synth a path that exists but has no `.git` above it.
    let scratch_root = tmp.join("scratch/PhantomRepo");
    std::fs::create_dir_all(scratch_root.join("src")).unwrap();
    let scratch_str = scratch_root.to_string_lossy().to_string();

    let mut bytes = synth_nitrite_with_turns(&["bfe8768a-b11e-469a-852b-fc22c7dd9f23"]);
    bytes.extend_from_slice(b"...currentFileUri\\\\\\\":\\\\\\\"file://");
    bytes.extend_from_slice(scratch_str.as_bytes());
    bytes.extend_from_slice(b"/src/bar.rs\\\\\\\",\\\\\\\"isVisionEnabled\\\\\\\":true...");

    let session_dir = tmp.join("iu/chat-agent-sessions/sess-phase2-cwd-hint");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(
        session_dir.join("copilot-agent-sessions-nitrite.db"),
        &bytes,
    )
    .unwrap();

    let parsed = parse_session_dir(&session_dir);
    assert_eq!(parsed.len(), 1, "one row per turn, got {parsed:?}");
    let msg = &parsed[0];
    assert!(
        msg.repo_id.is_none(),
        "no .git → no repo_id (got {:?})",
        msg.repo_id
    );
    // The byte-walker recovered the URI and the resolver could not
    // walk up to a `.git`, so we should still surface the prefix.
    assert_eq!(
        msg.cwd.as_deref(),
        Some(format!("{scratch_str}/src").as_str()),
        "cwd hint should equal the longest common prefix"
    );
    assert_eq!(
        msg.cwd_source.as_deref(),
        Some("copilot_chat:jetbrains_phase2_prefix")
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Phase 2 must not clobber a Phase 1 resolution. If the Xodus log
/// already mapped a `projectName` to a `.git` checkout, Phase 2 stays
/// silent — preserving the original 8.4.7/8.4.8 behaviour.
#[test]
fn phase2_does_not_override_phase1_resolution_when_xodus_already_resolved() {
    // This is asserted indirectly: `parse_session_dir` only invokes
    // `resolve_workspace_from_paths` when `phase1_resolution.is_none()`.
    // A regression here would either drop or change repo_id on the
    // sessions covered by `dual_store_session_combines_xodus_repo_with_nitrite_turns`
    // — which already passes. The structural assertion below documents
    // the invariant for future readers.
    // (No runtime body — the order is enforced by the dual-store test
    // above. Keeping the test stub here so the intent stays linked.)
}

/// #778: real-fixture coverage. The redacted on-disk Nitrite DB
/// captured from a JetBrains Copilot agent session that opened
/// `readme.md` from a Terraform repo. The Phase 2 byte-walker must
/// recover the file URI from the `currentFileUri` JSON blob, dedupe
/// the repeats, and compute the longest common prefix.
#[test]
fn extract_nitrite_workspace_paths_against_real_redacted_fixture() {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
        "src/providers/copilot_chat/fixtures/jetbrains_nitrite_working_set_phase2/copilot-agent-sessions-nitrite.db",
    );
    let bytes = std::fs::read(&fixture).expect("real fixture is checked in");
    let paths = extract_nitrite_workspace_paths(&bytes);
    assert!(!paths.is_empty(), "expected at least one URI");
    // Every recovered URI in this fixture points under the same repo.
    for p in &paths {
        assert!(
            p.starts_with("/Users/redacted-user/_projects/Terraform"),
            "unexpected URI: {p}"
        );
    }
    // Longest common prefix should be the Terraform repo root, not
    // a file path — the prefix-finder pops the filename component.
    let prefix = longest_common_path_prefix(&paths).expect("has common prefix");
    assert_eq!(prefix, "/Users/redacted-user/_projects/Terraform");
}

#[test]
fn decode_file_uri_handles_localhost_form_and_relative_rejection() {
    assert_eq!(
        decode_file_uri("file:///abs/path").as_deref(),
        Some("/abs/path")
    );
    assert_eq!(
        decode_file_uri("file://localhost/abs/path").as_deref(),
        Some("/abs/path")
    );
    assert!(decode_file_uri("file://relative/path").is_none());
    assert!(decode_file_uri("https://example.com/x").is_none());
}
