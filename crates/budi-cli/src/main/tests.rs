use super::*;
use clap::CommandFactory;

#[test]
fn cli_parses_init() {
    let _ = Cli::command();
}

#[test]
fn daemon_command_match_is_port_scoped() {
    let cmd = "/usr/local/bin/budi-daemon serve --host 127.0.0.1 --port 7878";
    assert!(daemon::is_budi_daemon_command_for_port(cmd, 7878));
    assert!(!daemon::is_budi_daemon_command_for_port(cmd, 9999));
    assert!(!daemon::is_budi_daemon_command_for_port(
        "python3 -m http.server 7878",
        7878
    ));
}

#[test]
fn help_shows_expected_commands() {
    let mut command = Cli::command();
    let help = command.render_help().to_string();
    let lower = help.to_ascii_lowercase();
    assert!(lower.contains("init"));
    assert!(lower.contains("doctor"));
    assert!(lower.contains("stats"));
    assert!(lower.contains("autostart"));
    // `budi db` is the canonical DB admin namespace. The pre-8.2.1
    // bare verbs (`budi migrate` / `budi repair` / `budi import`)
    // were removed in 8.3.0 (#428) and must not come back as a
    // top-level command.
    assert!(
        lower.contains("\n  db "),
        "top-level help should advertise the `budi db` namespace"
    );
    assert!(
        !lower.contains("\n  sync"),
        "sync command should be removed"
    );
}

#[test]
fn cli_parses_db_subcommands() {
    let cli = Cli::try_parse_from(["budi", "db", "check"]).expect("budi db check parses");
    assert!(matches!(
        cli.command,
        Commands::Db {
            action: DbAction::Check { fix: false }
        }
    ));

    let cli =
        Cli::try_parse_from(["budi", "db", "check", "--fix"]).expect("budi db check --fix parses");
    assert!(matches!(
        cli.command,
        Commands::Db {
            action: DbAction::Check { fix: true }
        }
    ));

    // #586: the pre-8.3.14 `db migrate` / `db repair` verbs were
    // collapsed into `db check [--fix]`. Both must error out as
    // unknown subcommands so they don't quietly come back.
    assert!(Cli::try_parse_from(["budi", "db", "migrate"]).is_err());
    assert!(Cli::try_parse_from(["budi", "db", "repair"]).is_err());

    let cli = Cli::try_parse_from(["budi", "db", "import"]).expect("budi db import parses");
    match cli.command {
        Commands::Db {
            action: DbAction::Import { force, format },
        } => {
            assert!(!force);
            assert_eq!(format, StatsFormat::Text);
        }
        _ => panic!("expected db import command"),
    }

    let cli = Cli::try_parse_from(["budi", "db", "import", "--force"])
        .expect("budi db import --force parses");
    match cli.command {
        Commands::Db {
            action: DbAction::Import { force, format },
        } => {
            assert!(force);
            assert_eq!(format, StatsFormat::Text);
        }
        _ => panic!("expected db import --force command"),
    }

    let cli = Cli::try_parse_from(["budi", "db", "import", "--format", "json"])
        .expect("budi db import --format json parses");
    match cli.command {
        Commands::Db {
            action: DbAction::Import { force, format },
        } => {
            assert!(!force);
            assert_eq!(format, StatsFormat::Json);
        }
        _ => panic!("expected db import --format json command"),
    }
}

#[test]
fn cli_rejects_removed_db_bare_verbs() {
    // #428 — the pre-8.2.1 bare verbs `budi migrate` / `budi repair` /
    // `budi import` were removed in 8.3.0 after shipping in 8.2.x as
    // hidden deprecation aliases. Users must now reach these via the
    // `budi db` namespace. Regression guard so they don't quietly come
    // back.
    assert!(Cli::try_parse_from(["budi", "migrate"]).is_err());
    assert!(Cli::try_parse_from(["budi", "repair"]).is_err());
    assert!(Cli::try_parse_from(["budi", "import"]).is_err());
    assert!(Cli::try_parse_from(["budi", "import", "--force"]).is_err());
}

#[test]
fn cli_parses_autostart_subcommands() {
    for sub in &["status", "install", "uninstall"] {
        Cli::try_parse_from(["budi", "autostart", sub])
            .unwrap_or_else(|_| panic!("budi autostart {sub} should parse"));
    }
}

#[test]
fn cli_rejects_removed_proxy_commands() {
    assert!(Cli::try_parse_from(["budi", "launch", "claude"]).is_err());
    assert!(Cli::try_parse_from(["budi", "enable", "claude"]).is_err());
    assert!(Cli::try_parse_from(["budi", "disable", "cursor"]).is_err());
    assert!(Cli::try_parse_from(["budi", "proxy-install", "claude"]).is_err());
}

#[test]
fn cli_parses_doctor_deep_flag() {
    let cli = Cli::try_parse_from(["budi", "doctor", "--deep"]).expect("doctor --deep parses");
    match cli.command {
        Commands::Doctor { deep, quiet, .. } => {
            assert!(deep);
            assert!(!quiet, "--deep should not imply --quiet");
        }
        _ => panic!("expected doctor command"),
    }
}

/// #487 (U-4): `--quiet` suppresses individual PASS lines on a
/// green run. The flag is parsed in isolation here; the rendering
/// contract (`CheckResult::print_respecting`) has its own unit
/// coverage in `commands::doctor::tests`.
#[test]
fn cli_parses_doctor_quiet_flag() {
    let cli = Cli::try_parse_from(["budi", "doctor", "--quiet"]).expect("doctor --quiet parses");
    match cli.command {
        Commands::Doctor { deep, quiet, .. } => {
            assert!(quiet);
            assert!(!deep, "--quiet should not imply --deep");
        }
        _ => panic!("expected doctor command"),
    }
}

#[test]
fn cli_parses_stats_tickets_subcommand() {
    let cli = Cli::try_parse_from(["budi", "stats", "tickets"]).expect("budi stats tickets parses");
    match cli.command {
        Commands::Stats(args) => {
            assert!(matches!(args.view, Some(StatsView::Tickets)));
        }
        _ => panic!("expected stats command"),
    }
}

#[test]
fn cli_parses_stats_ticket_subcommand() {
    let cli = Cli::try_parse_from(["budi", "stats", "ticket", "PAVA-2057"])
        .expect("budi stats ticket PAVA-2057 parses");
    match cli.command {
        Commands::Stats(args) => match args.view {
            Some(StatsView::Ticket { id }) => assert_eq!(id, "PAVA-2057"),
            other => panic!("expected ticket variant, got {:?}", other),
        },
        _ => panic!("expected stats command"),
    }
}

#[test]
fn cli_stats_legacy_view_flags_are_rejected() {
    // #589: the 11 mutually-exclusive view flags (--projects /
    // --branches / --branch / --tickets / --ticket / --activities /
    // --activity / --files / --file / --models / --tag) were removed
    // in favor of subcommands. Regression guard so they don't quietly
    // come back.
    assert!(Cli::try_parse_from(["budi", "stats", "--tickets"]).is_err());
    assert!(Cli::try_parse_from(["budi", "stats", "--ticket", "X-1"]).is_err());
    assert!(Cli::try_parse_from(["budi", "stats", "--branches"]).is_err());
    assert!(Cli::try_parse_from(["budi", "stats", "--branch", "main"]).is_err());
    assert!(Cli::try_parse_from(["budi", "stats", "--activities"]).is_err());
    assert!(Cli::try_parse_from(["budi", "stats", "--activity", "bugfix"]).is_err());
    assert!(Cli::try_parse_from(["budi", "stats", "--files"]).is_err());
    assert!(Cli::try_parse_from(["budi", "stats", "--file", "x.rs"]).is_err());
    assert!(Cli::try_parse_from(["budi", "stats", "--models"]).is_err());
    assert!(Cli::try_parse_from(["budi", "stats", "--projects"]).is_err());
    assert!(Cli::try_parse_from(["budi", "stats", "--tag", "ticket_id"]).is_err());
}

#[test]
fn cli_stats_subcommands_are_mutually_exclusive_by_construction() {
    // With clap subcommands, only one drill-in view can be selected
    // per invocation by definition. Passing two subcommand names back
    // to back parses the first as the view and the second as a
    // positional / unknown argument, which clap rejects.
    assert!(Cli::try_parse_from(["budi", "stats", "tickets", "branches"]).is_err());
    assert!(Cli::try_parse_from(["budi", "stats", "models", "files"]).is_err());
}

#[test]
fn cli_stats_ticket_accepts_repo_filter() {
    let cli = Cli::try_parse_from([
        "budi",
        "stats",
        "ticket",
        "PAVA-2057",
        "--repo",
        "siropkin/budi",
    ])
    .expect("budi stats ticket --repo parses");
    match cli.command {
        Commands::Stats(args) => {
            match args.view {
                Some(StatsView::Ticket { id }) => assert_eq!(id, "PAVA-2057"),
                other => panic!("expected ticket variant, got {:?}", other),
            }
            assert_eq!(args.opts.repo.as_deref(), Some("siropkin/budi"));
        }
        _ => panic!("expected stats command"),
    }
}

#[test]
fn cli_parses_sessions_ticket_flag() {
    let cli = Cli::try_parse_from(["budi", "sessions", "--ticket", "PAVA-2057"])
        .expect("budi sessions --ticket parses");
    match cli.command {
        Commands::Sessions { ticket, .. } => {
            assert_eq!(ticket.as_deref(), Some("PAVA-2057"));
        }
        _ => panic!("expected sessions command"),
    }
}

/// #683: `--provider` complements `--search` so `budi sessions
/// --provider copilot_chat` no longer leaks substring matches into
/// `claude_code` / `vscode` / `codex`. Pre-fix the flag didn't exist.
#[test]
fn cli_parses_sessions_provider_flag() {
    let cli = Cli::try_parse_from(["budi", "sessions", "--provider", "copilot_chat"])
        .expect("budi sessions --provider parses");
    match cli.command {
        Commands::Sessions { provider, .. } => {
            assert_eq!(provider.as_deref(), Some("copilot_chat"));
        }
        _ => panic!("expected sessions command"),
    }
}

/// #702: `budi sessions --surface jetbrains` parses cleanly. CSV +
/// repeated forms collapse into the same `Vec<String>` so callers can
/// pick whichever shape they prefer.
#[test]
fn cli_parses_sessions_surface_flag() {
    let cli = Cli::try_parse_from(["budi", "sessions", "--surface", "jetbrains"])
        .expect("budi sessions --surface parses");
    match cli.command {
        Commands::Sessions { surface, .. } => {
            assert_eq!(surface, vec!["jetbrains".to_string()]);
        }
        _ => panic!("expected sessions command"),
    }

    // CSV form
    let cli = Cli::try_parse_from(["budi", "sessions", "--surface", "vscode,cursor"])
        .expect("budi sessions --surface CSV parses");
    match cli.command {
        Commands::Sessions { surface, .. } => {
            assert_eq!(surface, vec!["vscode".to_string(), "cursor".to_string()]);
        }
        _ => panic!("expected sessions command"),
    }

    // Repeated form
    let cli = Cli::try_parse_from([
        "budi",
        "sessions",
        "--surface",
        "vscode",
        "--surface",
        "cursor",
    ])
    .expect("budi sessions --surface repeated parses");
    match cli.command {
        Commands::Sessions { surface, .. } => {
            assert_eq!(surface, vec!["vscode".to_string(), "cursor".to_string()]);
        }
        _ => panic!("expected sessions command"),
    }
}

/// #702: `budi stats surfaces` is a first-class breakdown subcommand.
#[test]
fn cli_parses_stats_surfaces_subcommand() {
    let cli =
        Cli::try_parse_from(["budi", "stats", "surfaces"]).expect("budi stats surfaces parses");
    match cli.command {
        Commands::Stats(args) => {
            assert!(matches!(args.view, Some(StatsView::Surfaces)));
        }
        _ => panic!("expected stats command"),
    }
}

/// #702: `budi stats --surface vscode` is global and applies before or
/// after a subcommand name (parity with `--provider`).
#[test]
fn cli_parses_stats_surface_global_flag() {
    let cli = Cli::try_parse_from(["budi", "stats", "--surface", "vscode", "models"])
        .expect("budi stats --surface vscode models parses");
    match cli.command {
        Commands::Stats(args) => {
            assert_eq!(args.opts.surface, vec!["vscode".to_string()]);
            assert!(matches!(args.view, Some(StatsView::Models)));
        }
        _ => panic!("expected stats command"),
    }
}

#[test]
fn cli_parses_stats_activities_subcommand() {
    let cli =
        Cli::try_parse_from(["budi", "stats", "activities"]).expect("budi stats activities parses");
    match cli.command {
        Commands::Stats(args) => {
            assert!(matches!(args.view, Some(StatsView::Activities)));
        }
        _ => panic!("expected stats command"),
    }
}

#[test]
fn cli_parses_stats_activity_subcommand() {
    let cli = Cli::try_parse_from(["budi", "stats", "activity", "bugfix"])
        .expect("budi stats activity bugfix parses");
    match cli.command {
        Commands::Stats(args) => match args.view {
            Some(StatsView::Activity { name }) => assert_eq!(name, "bugfix"),
            other => panic!("expected activity variant, got {:?}", other),
        },
        _ => panic!("expected stats command"),
    }
}

#[test]
fn cli_stats_activity_accepts_repo_filter() {
    let cli = Cli::try_parse_from([
        "budi",
        "stats",
        "activity",
        "bugfix",
        "--repo",
        "siropkin/budi",
    ])
    .expect("budi stats activity --repo parses");
    match cli.command {
        Commands::Stats(args) => {
            match args.view {
                Some(StatsView::Activity { name }) => assert_eq!(name, "bugfix"),
                other => panic!("expected activity variant, got {:?}", other),
            }
            assert_eq!(args.opts.repo.as_deref(), Some("siropkin/budi"));
        }
        _ => panic!("expected stats command"),
    }
}

#[test]
fn cli_parses_stats_files_subcommand() {
    let cli = Cli::try_parse_from(["budi", "stats", "files"]).expect("budi stats files parses");
    match cli.command {
        Commands::Stats(args) => {
            assert!(matches!(args.view, Some(StatsView::Files)));
        }
        _ => panic!("expected stats command"),
    }
}

#[test]
fn cli_parses_stats_file_subcommand() {
    let cli = Cli::try_parse_from(["budi", "stats", "file", "crates/budi-core/src/lib.rs"])
        .expect("budi stats file <path> parses");
    match cli.command {
        Commands::Stats(args) => match args.view {
            Some(StatsView::File { path }) => {
                assert_eq!(path, "crates/budi-core/src/lib.rs")
            }
            other => panic!("expected file variant, got {:?}", other),
        },
        _ => panic!("expected stats command"),
    }
}

#[test]
fn cli_stats_file_accepts_repo_filter() {
    let cli = Cli::try_parse_from([
        "budi",
        "stats",
        "file",
        "src/main.rs",
        "--repo",
        "siropkin/budi",
    ])
    .expect("budi stats file --repo parses");
    match cli.command {
        Commands::Stats(args) => {
            match args.view {
                Some(StatsView::File { path }) => assert_eq!(path, "src/main.rs"),
                other => panic!("expected file variant, got {:?}", other),
            }
            assert_eq!(args.opts.repo.as_deref(), Some("siropkin/budi"));
        }
        _ => panic!("expected stats command"),
    }
}

#[test]
fn cli_stats_global_flags_parse_after_subcommand() {
    // Regression guard: shared flags marked `global = true` must
    // parse equivalently before or after the subcommand name. The
    // typical user reach is `budi stats projects -p week` (after).
    let cli = Cli::try_parse_from(["budi", "stats", "projects", "-p", "week"])
        .expect("budi stats projects -p week should parse");
    match cli.command {
        Commands::Stats(args) => {
            assert!(matches!(args.view, Some(StatsView::Projects)));
            assert_eq!(args.opts.period, StatsPeriod::Week);
        }
        _ => panic!("expected stats command"),
    }

    // `budi stats branches --limit 5 --format json` exercises three
    // shared knobs at once on a breakdown view.
    let cli = Cli::try_parse_from([
        "budi", "stats", "branches", "--limit", "5", "--format", "json",
    ])
    .expect("budi stats branches with shared opts should parse");
    match cli.command {
        Commands::Stats(args) => {
            assert!(matches!(args.view, Some(StatsView::Branches)));
            assert_eq!(args.opts.limit, 5);
            assert_eq!(args.opts.format, StatsFormat::Json);
        }
        _ => panic!("expected stats command"),
    }
}

#[test]
fn cli_parses_stats_models_subcommand() {
    let cli = Cli::try_parse_from(["budi", "stats", "models"]).expect("budi stats models parses");
    match cli.command {
        Commands::Stats(args) => assert!(matches!(args.view, Some(StatsView::Models))),
        _ => panic!("expected stats command"),
    }
}

#[test]
fn cli_parses_stats_projects_subcommand() {
    let cli = Cli::try_parse_from(["budi", "stats", "projects", "-p", "all"])
        .expect("budi stats projects -p all parses");
    match cli.command {
        Commands::Stats(args) => {
            assert!(matches!(args.view, Some(StatsView::Projects)));
            assert_eq!(args.opts.period, StatsPeriod::All);
        }
        _ => panic!("expected stats command"),
    }
}

#[test]
fn cli_parses_stats_branches_subcommand() {
    let cli =
        Cli::try_parse_from(["budi", "stats", "branches"]).expect("budi stats branches parses");
    match cli.command {
        Commands::Stats(args) => assert!(matches!(args.view, Some(StatsView::Branches))),
        _ => panic!("expected stats command"),
    }
}

#[test]
fn cli_parses_stats_branch_subcommand() {
    let cli = Cli::try_parse_from(["budi", "stats", "branch", "main"])
        .expect("budi stats branch main parses");
    match cli.command {
        Commands::Stats(args) => match args.view {
            Some(StatsView::Branch { name }) => assert_eq!(name, "main"),
            other => panic!("expected branch variant, got {:?}", other),
        },
        _ => panic!("expected stats command"),
    }
}

#[test]
fn cli_parses_stats_tag_subcommand() {
    let cli = Cli::try_parse_from(["budi", "stats", "tag", "activity"])
        .expect("budi stats tag activity parses");
    match cli.command {
        Commands::Stats(args) => match args.view {
            Some(StatsView::Tag { key }) => assert_eq!(key, "activity"),
            other => panic!("expected tag variant, got {:?}", other),
        },
        _ => panic!("expected stats command"),
    }
}

#[test]
fn cli_parses_bare_stats() {
    // No subcommand → default summary view. Shared opts still parse
    // at the top level.
    let cli = Cli::try_parse_from(["budi", "stats"]).expect("bare budi stats parses");
    match cli.command {
        Commands::Stats(args) => {
            assert!(args.view.is_none());
            assert_eq!(args.opts.period, StatsPeriod::Today);
        }
        _ => panic!("expected stats command"),
    }

    let cli = Cli::try_parse_from(["budi", "stats", "--provider", "cursor"])
        .expect("budi stats --provider cursor parses");
    match cli.command {
        Commands::Stats(args) => {
            assert!(args.view.is_none());
            assert_eq!(args.opts.provider.as_deref(), Some("cursor"));
        }
        _ => panic!("expected stats command"),
    }
}

#[test]
fn cli_parses_cloud_subcommands() {
    let cli = Cli::try_parse_from(["budi", "cloud", "sync"]).expect("budi cloud sync parses");
    match cli.command {
        Commands::Cloud {
            action: CloudAction::Sync { format, full, yes },
        } => {
            assert!(matches!(format, StatsFormat::Text));
            assert!(!full, "default invocation must not drop watermarks");
            assert!(!yes, "default invocation must be interactive");
        }
        _ => panic!("expected cloud sync command"),
    }

    let cli = Cli::try_parse_from(["budi", "cloud", "status", "--format", "json"])
        .expect("budi cloud status --format json parses");
    match cli.command {
        Commands::Cloud {
            action: CloudAction::Status { format },
        } => assert!(matches!(format, StatsFormat::Json)),
        _ => panic!("expected cloud status command"),
    }
}

#[test]
fn cli_parses_cloud_sync_full() {
    // #583: `--full` folds the prior `cloud reset && cloud sync`
    // two-step into a single verb. `--yes` is the non-interactive
    // escape hatch CI / scripts need so the confirmation prompt
    // isn't a hard block.
    let cli = Cli::try_parse_from(["budi", "cloud", "sync", "--full"])
        .expect("budi cloud sync --full parses");
    match cli.command {
        Commands::Cloud {
            action: CloudAction::Sync { full, yes, .. },
        } => {
            assert!(full, "--full must request a watermark drop");
            assert!(!yes, "--full alone must keep the confirmation prompt");
        }
        _ => panic!("expected cloud sync --full command"),
    }

    let cli = Cli::try_parse_from(["budi", "cloud", "sync", "--full", "--yes"])
        .expect("budi cloud sync --full --yes parses");
    match cli.command {
        Commands::Cloud {
            action: CloudAction::Sync { full, yes, .. },
        } => {
            assert!(full);
            assert!(yes, "--yes must skip the confirmation");
        }
        _ => panic!("expected cloud sync --full --yes command"),
    }

    let cli = Cli::try_parse_from(["budi", "cloud", "sync", "--full", "--format", "json"])
        .expect("budi cloud sync --full --format json parses");
    match cli.command {
        Commands::Cloud {
            action:
                CloudAction::Sync {
                    full,
                    format,
                    yes: _,
                },
        } => {
            assert!(full);
            assert!(matches!(format, StatsFormat::Json));
        }
        _ => panic!("expected cloud sync --full --format json command"),
    }
}

#[test]
fn cli_parses_cloud_reset() {
    // #564: bare `budi cloud reset` parses and defaults to
    // interactive (yes=false). `--yes` is the non-interactive
    // escape hatch CI / scripts need so the prompt isn't a
    // hard block.
    let cli = Cli::try_parse_from(["budi", "cloud", "reset"]).expect("budi cloud reset parses");
    match cli.command {
        Commands::Cloud {
            action: CloudAction::Reset { yes, format },
        } => {
            assert!(!yes, "default invocation must be interactive");
            assert_eq!(format, StatsFormat::Text);
        }
        _ => panic!("expected cloud reset command"),
    }

    let cli = Cli::try_parse_from(["budi", "cloud", "reset", "--yes"])
        .expect("budi cloud reset --yes parses");
    match cli.command {
        Commands::Cloud {
            action: CloudAction::Reset { yes, format },
        } => {
            assert!(yes, "--yes must skip the confirmation");
            assert_eq!(format, StatsFormat::Text);
        }
        _ => panic!("expected cloud reset --yes command"),
    }

    // #588: `--format json` parses on `cloud reset` so scripted
    // callers (CI gates, dashboards) can emit a stable JSON shape
    // without scraping the human render.
    let cli = Cli::try_parse_from(["budi", "cloud", "reset", "--yes", "--format", "json"])
        .expect("budi cloud reset --yes --format json parses");
    match cli.command {
        Commands::Cloud {
            action: CloudAction::Reset { yes, format },
        } => {
            assert!(yes);
            assert_eq!(format, StatsFormat::Json);
        }
        _ => panic!("expected cloud reset --yes --format json command"),
    }
}

#[test]
fn cli_parses_cloud_init_bare() {
    let cli = Cli::try_parse_from(["budi", "cloud", "init"]).expect("budi cloud init parses");
    match cli.command {
        Commands::Cloud {
            action:
                CloudAction::Init {
                    api_key,
                    force,
                    yes,
                    device_id,
                    workspace_id,
                },
        } => {
            assert!(api_key.is_none());
            assert!(!force);
            assert!(!yes);
            assert!(device_id.is_none());
            assert!(workspace_id.is_none());
        }
        _ => panic!("expected cloud init command"),
    }
}

#[test]
fn cli_parses_cloud_init_with_flags() {
    let cli = Cli::try_parse_from([
        "budi",
        "cloud",
        "init",
        "--api-key",
        "fake-test-key",
        "--force",
        "--yes",
    ])
    .expect("budi cloud init --api-key --force --yes parses");
    match cli.command {
        Commands::Cloud {
            action:
                CloudAction::Init {
                    api_key,
                    force,
                    yes,
                    device_id,
                    workspace_id,
                },
        } => {
            assert_eq!(api_key.as_deref(), Some("fake-test-key"));
            assert!(force);
            assert!(yes);
            assert!(device_id.is_none());
            assert!(workspace_id.is_none());
        }
        _ => panic!("expected cloud init command"),
    }
}

#[test]
fn cli_parses_cloud_init_manual_ids() {
    // #541: the escape hatch for offline installs / self-hosted
    // endpoints without /v1/whoami. `--device-id` / `--workspace-id`
    // bypass the whoami fetch and write the provided values
    // verbatim into the template.
    let cli = Cli::try_parse_from([
        "budi",
        "cloud",
        "init",
        "--api-key",
        "fake-test-key",
        "--device-id",
        "my-laptop",
        "--workspace-id",
        "ws_selfhost",
    ])
    .expect("budi cloud init with manual ids parses");
    match cli.command {
        Commands::Cloud {
            action:
                CloudAction::Init {
                    api_key,
                    device_id,
                    workspace_id,
                    ..
                },
        } => {
            assert_eq!(api_key.as_deref(), Some("fake-test-key"));
            assert_eq!(device_id.as_deref(), Some("my-laptop"));
            assert_eq!(workspace_id.as_deref(), Some("ws_selfhost"));
        }
        _ => panic!("expected cloud init command"),
    }
}

#[test]
fn cli_rejects_dropped_org_id_flag() {
    // #843: `--org-id` is no longer accepted. The flag was a back-compat
    // shim during the org→workspace rename window; the user base is
    // small and re-running `budi cloud init --api-key <KEY> --force
    // --yes` with `--workspace-id` is the clean path forward.
    let err = Cli::try_parse_from([
        "budi",
        "cloud",
        "init",
        "--api-key",
        "fake-test-key",
        "--org-id",
        "org_selfhost",
    ])
    .expect_err("`--org-id` must be rejected after #843");
    assert!(
        err.to_string().contains("--org-id") || err.to_string().contains("unexpected argument"),
        "clap error must point at the removed flag, got: {err}"
    );
}

#[test]
fn cli_parses_pricing_subcommands() {
    // #584: split `pricing status --refresh` (the lone read-only-shaped
    // verb that hid a network call) into `pricing status` (read-only)
    // and `pricing sync` (network), with bare `budi pricing` defaulting
    // to the read-only view.
    let cli = Cli::try_parse_from(["budi", "pricing"]).expect("bare budi pricing parses");
    match cli.command {
        Commands::Pricing(args) => {
            assert!(args.view.is_none(), "bare invocation has no view");
            assert!(matches!(args.format, StatsFormat::Text));
        }
        _ => panic!("expected pricing command"),
    }

    let cli =
        Cli::try_parse_from(["budi", "pricing", "status"]).expect("budi pricing status parses");
    match cli.command {
        Commands::Pricing(args) => assert!(matches!(args.view, Some(PricingView::Status))),
        _ => panic!("expected pricing status command"),
    }

    let cli = Cli::try_parse_from(["budi", "pricing", "sync"]).expect("budi pricing sync parses");
    match cli.command {
        Commands::Pricing(args) => assert!(matches!(args.view, Some(PricingView::Sync))),
        _ => panic!("expected pricing sync command"),
    }

    let cli = Cli::try_parse_from(["budi", "pricing", "--format", "json"])
        .expect("budi pricing --format json parses");
    match cli.command {
        Commands::Pricing(args) => {
            assert!(args.view.is_none());
            assert!(matches!(args.format, StatsFormat::Json));
        }
        _ => panic!("expected pricing command"),
    }

    let cli = Cli::try_parse_from(["budi", "pricing", "sync", "--format", "json"])
        .expect("budi pricing sync --format json parses");
    match cli.command {
        Commands::Pricing(args) => {
            assert!(matches!(args.view, Some(PricingView::Sync)));
            assert!(matches!(args.format, StatsFormat::Json));
        }
        _ => panic!("expected pricing sync command"),
    }
}

#[test]
fn cli_parses_pricing_recompute_subcommand() {
    // #732: `budi pricing recompute [--force]` is the new manual
    // trigger for the team-pricing recompute pass.
    let cli = Cli::try_parse_from(["budi", "pricing", "recompute"])
        .expect("budi pricing recompute parses");
    match cli.command {
        Commands::Pricing(args) => match args.view {
            Some(PricingView::Recompute { force }) => assert!(!force),
            other => panic!("expected pricing recompute, got {other:?}"),
        },
        _ => panic!("expected pricing recompute command"),
    }

    let cli = Cli::try_parse_from(["budi", "pricing", "recompute", "--force"])
        .expect("budi pricing recompute --force parses");
    match cli.command {
        Commands::Pricing(args) => match args.view {
            Some(PricingView::Recompute { force }) => assert!(force),
            other => panic!("expected pricing recompute --force, got {other:?}"),
        },
        _ => panic!("expected pricing recompute command"),
    }
}

#[test]
fn cli_rejects_legacy_pricing_refresh_flag() {
    // #584: `--refresh` was the only flag in the entire CLI that
    // performed a network call from a read-only-shaped verb. It is
    // dropped entirely (no users, safe to break per the ticket); the
    // replacement is `budi pricing sync`.
    assert!(
        Cli::try_parse_from(["budi", "pricing", "status", "--refresh"]).is_err(),
        "`pricing status --refresh` must hard-fail; use `pricing sync` instead",
    );
    assert!(
        Cli::try_parse_from(["budi", "pricing", "--refresh"]).is_err(),
        "bare `pricing --refresh` must hard-fail; use `pricing sync` instead",
    );
}

#[test]
fn help_lists_cloud_commands() {
    let mut command = Cli::command();
    let help = command.render_help().to_string();
    let lower = help.to_ascii_lowercase();
    assert!(
        lower.contains("cloud"),
        "top-level help should advertise cloud subcommand"
    );
    assert!(
        lower.contains("budi cloud sync"),
        "top-level help should mention `budi cloud sync`"
    );
}

#[test]
fn cli_no_longer_exposes_vitals_or_health_top_level() {
    // `budi vitals` was folded into `budi sessions <id>` / `budi
    // sessions latest` in #585; the top-level `vitals` and the
    // deprecated `health` alias are removed. Both must hard-fail
    // at the clap layer rather than silently parsing.
    assert!(
        Cli::try_parse_from(["budi", "vitals"]).is_err(),
        "budi vitals should no longer parse — folded into `budi sessions latest`"
    );
    assert!(
        Cli::try_parse_from(["budi", "health"]).is_err(),
        "budi health alias should no longer parse — folded into `budi sessions latest`"
    );
}

#[test]
fn help_advertises_sessions_latest_for_vitals_replacement() {
    let mut command = Cli::command();
    let help = command.render_help().to_string();
    assert!(
        help.contains("budi sessions latest"),
        "top-level help should advertise `budi sessions latest` as the vitals replacement"
    );
    assert!(
        !help.contains("\n  vitals "),
        "removed `budi vitals` must not appear in the subcommand list"
    );
    assert!(
        !help.contains("\n  health "),
        "removed `budi health` alias must not appear in the subcommand list"
    );
}

#[test]
fn cli_sessions_accepts_latest_as_session_id() {
    // `budi sessions latest` is the canonical replacement for the
    // bare `budi vitals` invocation. The dispatcher recognises the
    // literal string and resolves it to the most recent session
    // server-side.
    let cli = Cli::try_parse_from(["budi", "sessions", "latest"])
        .expect("budi sessions latest should parse");
    match cli.command {
        Commands::Sessions { session_id, .. } => {
            assert_eq!(session_id.as_deref(), Some("latest"));
        }
        _ => panic!("expected sessions command"),
    }
}

#[test]
fn cli_sessions_accepts_current_as_session_id() {
    // `budi sessions current` (#603) is the cwd-scoped sibling of
    // `latest`. It backs the auto-installed `/budi` Claude Code
    // skill and resolves to the active session for this cwd
    // server-side.
    let cli = Cli::try_parse_from(["budi", "sessions", "current"])
        .expect("budi sessions current should parse");
    match cli.command {
        Commands::Sessions { session_id, .. } => {
            assert_eq!(session_id.as_deref(), Some("current"));
        }
        _ => panic!("expected sessions command"),
    }
}

#[test]
fn help_advertises_sessions_current_for_budi_skill() {
    // `/budi` skill calls `budi sessions current`; the help
    // surface should mention it so users discovering the
    // command from a terminal see the correspondence.
    let mut command = Cli::command();
    let help = command.render_help().to_string();
    assert!(
        help.contains("budi sessions current"),
        "top-level help should advertise `budi sessions current` for the /budi skill"
    );
}

#[test]
fn stats_period_parses_calendar_windows() {
    use std::str::FromStr;
    assert_eq!(StatsPeriod::from_str("today").unwrap(), StatsPeriod::Today);
    assert_eq!(StatsPeriod::from_str("Today").unwrap(), StatsPeriod::Today);
    assert_eq!(StatsPeriod::from_str("week").unwrap(), StatsPeriod::Week);
    assert_eq!(StatsPeriod::from_str("month").unwrap(), StatsPeriod::Month);
    assert_eq!(StatsPeriod::from_str("all").unwrap(), StatsPeriod::All);
}

#[test]
fn stats_period_parses_relative_windows() {
    use std::str::FromStr;
    assert_eq!(
        StatsPeriod::from_str("1d").unwrap(),
        StatsPeriod::Days(1),
        "1d should parse as a 1-day rolling window"
    );
    assert_eq!(StatsPeriod::from_str("7d").unwrap(), StatsPeriod::Days(7));
    assert_eq!(
        StatsPeriod::from_str("30D").unwrap(),
        StatsPeriod::Days(30),
        "unit suffix should be case-insensitive"
    );
    assert_eq!(StatsPeriod::from_str("1w").unwrap(), StatsPeriod::Weeks(1));
    assert_eq!(StatsPeriod::from_str("2w").unwrap(), StatsPeriod::Weeks(2));
    assert_eq!(StatsPeriod::from_str("1m").unwrap(), StatsPeriod::Months(1));
    assert_eq!(StatsPeriod::from_str("3m").unwrap(), StatsPeriod::Months(3));
    assert_eq!(
        StatsPeriod::from_str(" 7d ").unwrap(),
        StatsPeriod::Days(7),
        "whitespace should be trimmed"
    );
}

#[test]
fn stats_period_rejects_invalid_input() {
    use std::str::FromStr;

    // Zero is rejected with a hint rather than silently collapsing
    // the window to "today" (0d) and producing confusing stats.
    assert!(StatsPeriod::from_str("0d").is_err());
    assert!(StatsPeriod::from_str("0w").is_err());
    assert!(StatsPeriod::from_str("0m").is_err());

    // Empty / whitespace / missing count.
    assert!(StatsPeriod::from_str("").is_err());
    assert!(StatsPeriod::from_str("   ").is_err());
    assert!(StatsPeriod::from_str("d").is_err());
    assert!(StatsPeriod::from_str("w").is_err());

    // Unknown unit.
    assert!(StatsPeriod::from_str("7y").is_err());
    assert!(StatsPeriod::from_str("7h").is_err());

    // Non-numeric count.
    assert!(StatsPeriod::from_str("abcd").is_err());
    assert!(StatsPeriod::from_str("-1d").is_err());

    // Multi-byte UTF-8 input must not panic (`split_at` byte
    // safety — regression guard for the pre-#404 implementation).
    assert!(StatsPeriod::from_str("1日").is_err());
    assert!(StatsPeriod::from_str("日").is_err());
}

#[test]
fn cli_stats_parses_relative_period_flag() {
    let cli =
        Cli::try_parse_from(["budi", "stats", "-p", "7d"]).expect("budi stats -p 7d should parse");
    match cli.command {
        Commands::Stats(args) => assert_eq!(args.opts.period, StatsPeriod::Days(7)),
        _ => panic!("expected stats command"),
    }

    let cli = Cli::try_parse_from(["budi", "stats", "--period", "2w"])
        .expect("budi stats --period 2w should parse");
    match cli.command {
        Commands::Stats(args) => assert_eq!(args.opts.period, StatsPeriod::Weeks(2)),
        _ => panic!("expected stats command"),
    }

    let cli =
        Cli::try_parse_from(["budi", "stats", "-p", "1m"]).expect("budi stats -p 1m should parse");
    match cli.command {
        Commands::Stats(args) => assert_eq!(args.opts.period, StatsPeriod::Months(1)),
        _ => panic!("expected stats command"),
    }

    // Invalid relative period must be rejected by clap with a clear
    // message (the `FromStr::Err` string is surfaced by clap).
    assert!(Cli::try_parse_from(["budi", "stats", "-p", "0d"]).is_err());
    assert!(Cli::try_parse_from(["budi", "stats", "-p", "7y"]).is_err());
}

#[test]
fn cli_sessions_parses_relative_period_flag() {
    let cli = Cli::try_parse_from(["budi", "sessions", "-p", "7d"])
        .expect("budi sessions -p 7d should parse");
    match cli.command {
        Commands::Sessions { period, .. } => assert_eq!(period, StatsPeriod::Days(7)),
        _ => panic!("expected sessions command"),
    }

    let cli = Cli::try_parse_from(["budi", "sessions", "--period", "2w"])
        .expect("budi sessions --period 2w should parse");
    match cli.command {
        Commands::Sessions { period, .. } => assert_eq!(period, StatsPeriod::Weeks(2)),
        _ => panic!("expected sessions command"),
    }
}

#[test]
fn cli_parses_sessions_activity_flag() {
    let cli = Cli::try_parse_from(["budi", "sessions", "--activity", "bugfix"])
        .expect("budi sessions --activity parses");
    match cli.command {
        Commands::Sessions {
            ticket, activity, ..
        } => {
            assert!(ticket.is_none());
            assert_eq!(activity.as_deref(), Some("bugfix"));
        }
        _ => panic!("expected sessions command"),
    }
}

/// #485: `budi statusline --format text` should parse as the
/// default (Claude) render so a fresh user doesn't have to
/// remember that statusline's format vocabulary differs from
/// `budi stats` / `budi sessions`. Added via `#[value(alias = "text")]`
/// on `StatuslineFormat::Claude`.
#[test]
fn cli_statusline_accepts_text_as_claude_alias() {
    let cli = Cli::try_parse_from(["budi", "statusline", "--format", "text"])
        .expect("budi statusline --format text should parse");
    match cli.command {
        Commands::Statusline { format, .. } => {
            assert_eq!(format, StatuslineFormat::Claude);
        }
        _ => panic!("expected statusline command"),
    }

    // Explicit `--format claude` still parses as Claude (no
    // regression from the alias addition).
    let cli = Cli::try_parse_from(["budi", "statusline", "--format", "claude"])
        .expect("budi statusline --format claude should parse");
    match cli.command {
        Commands::Statusline { format, .. } => {
            assert_eq!(format, StatuslineFormat::Claude);
        }
        _ => panic!("expected statusline command"),
    }

    // Other non-default formats still resolve to their own
    // variants — alias only applies to the default.
    let cli = Cli::try_parse_from(["budi", "statusline", "--format", "json"])
        .expect("budi statusline --format json should parse");
    match cli.command {
        Commands::Statusline { format, .. } => {
            assert_eq!(format, StatuslineFormat::Json);
        }
        _ => panic!("expected statusline command"),
    }
}

/// #639: `budi statusline --slots <list>` must parse and surface the
/// raw value to the command handler. The handler is responsible for
/// trimming/normalizing — this test only locks the clap surface so
/// the flag isn't accidentally renamed or dropped.
#[test]
fn cli_statusline_accepts_slots_flag() {
    let cli = Cli::try_parse_from(["budi", "statusline", "--slots", "session,message"])
        .expect("budi statusline --slots session,message should parse");
    match cli.command {
        Commands::Statusline { slots, .. } => {
            assert_eq!(slots.as_deref(), Some("session,message"));
        }
        _ => panic!("expected statusline command"),
    }

    // Default: no `--slots` → None (config file wins).
    let cli = Cli::try_parse_from(["budi", "statusline"])
        .expect("budi statusline should parse without --slots");
    match cli.command {
        Commands::Statusline { slots, .. } => {
            assert!(slots.is_none());
        }
        _ => panic!("expected statusline command"),
    }
}

// #588: every system-state command must accept `--format json` so CI
// gates and dashboards can stop scraping the human render. The
// following tests lock the clap surface; the per-command JSON shapes
// are tested next to their implementations.

#[test]
fn cli_status_accepts_format_json() {
    let cli = Cli::try_parse_from(["budi", "status"]).expect("budi status parses");
    match cli.command {
        Commands::Status { format } => assert_eq!(format, StatsFormat::Text),
        _ => panic!("expected status command"),
    }
    let cli = Cli::try_parse_from(["budi", "status", "--format", "json"])
        .expect("budi status --format json parses");
    match cli.command {
        Commands::Status { format } => assert_eq!(format, StatsFormat::Json),
        _ => panic!("expected status --format json command"),
    }
}

#[test]
fn cli_doctor_accepts_format_json() {
    let cli = Cli::try_parse_from(["budi", "doctor"]).expect("budi doctor parses");
    match cli.command {
        Commands::Doctor { format, .. } => assert_eq!(format, StatsFormat::Text),
        _ => panic!("expected doctor command"),
    }
    let cli = Cli::try_parse_from(["budi", "doctor", "--format", "json"])
        .expect("budi doctor --format json parses");
    match cli.command {
        Commands::Doctor { format, .. } => assert_eq!(format, StatsFormat::Json),
        _ => panic!("expected doctor --format json command"),
    }
}

#[test]
fn cli_autostart_status_accepts_format_json() {
    let cli = Cli::try_parse_from(["budi", "autostart", "status"]).expect("budi autostart status");
    match cli.command {
        Commands::Autostart {
            action: AutostartAction::Status { format },
        } => assert_eq!(format, StatsFormat::Text),
        _ => panic!("expected autostart status command"),
    }

    let cli = Cli::try_parse_from(["budi", "autostart", "status", "--format", "json"])
        .expect("budi autostart status --format json parses");
    match cli.command {
        Commands::Autostart {
            action: AutostartAction::Status { format },
        } => assert_eq!(format, StatsFormat::Json),
        _ => panic!("expected autostart status --format json command"),
    }
}

#[test]
fn cli_cloud_status_accepts_format_json() {
    // Already shipped pre-#588 — guard regression.
    let cli = Cli::try_parse_from(["budi", "cloud", "status", "--format", "json"])
        .expect("budi cloud status --format json parses");
    match cli.command {
        Commands::Cloud {
            action: CloudAction::Status { format },
        } => assert_eq!(format, StatsFormat::Json),
        _ => panic!("expected cloud status --format json command"),
    }
}
