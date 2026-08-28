//! Headless probes: deterministic single-shot inspections of the running
//! editor that the e2e suites drive through `zec probe <case>`.

use super::*;

pub(crate) fn run_repository_probe(probe: RepositoryProbe) -> Result<()> {
    let (sender, receiver) = mpsc::sync_channel(1);
    editor_application().run(move |cx| {
        init_zed(cx);
        let services = file_services(cx);
        cx.spawn(async move |cx| {
            let result = execute_repository_probe(probe, &services, cx)
                .await
                .and_then(|value| {
                    serde_json::to_string(&value).context("serialize repository probe result")
                });
            sender.send(result).expect("send repository probe result");
            let _ = cx.update(|cx| cx.quit());
        })
        .detach();
    });

    let json = receiver
        .recv()
        .context("repository probe runtime exited without a result")??;
    println!("{json}");
    Ok(())
}

pub(crate) fn run_language_probe(probe: LanguageProbe) -> Result<()> {
    let (sender, receiver) = mpsc::sync_channel(1);
    editor_application().run(move |cx| {
        init_zed(cx);
        let watches_configuration = matches!(&probe, LanguageProbe::SettingsReload { .. });
        let (event_sender, event_receiver) = async_channel::bounded(64);
        let configuration_file_system = cx.global::<ProjectRuntime>().file_system.clone();
        start_configuration_watchers(configuration_file_system, event_sender.clone(), cx);
        if watches_configuration {
            start_terminal_action_interceptor(
                Rc::new(RefCell::new(VecDeque::new())),
                Some(event_sender.clone()),
                cx,
            );
        }
        let services = file_services(cx);
        start_project_configuration_notifications(&services.project, event_sender.clone(), cx);
        cx.spawn(async move |cx| {
            let result = execute_language_probe(probe, &services, event_receiver, cx)
                .await
                .and_then(|value| {
                    serde_json::to_string(&value).context("serialize language probe result")
                });
            sender.send(result).expect("send language probe result");
            let _ = cx.update(|cx| cx.quit());
        })
        .detach();
    });

    let json = receiver
        .recv()
        .context("language probe runtime exited without a result")??;
    println!("{json}");
    Ok(())
}

async fn execute_language_probe(
    probe: LanguageProbe,
    services: &FileServices,
    configuration_events: async_channel::Receiver<TerminalEvent>,
    cx: &mut gpui::AsyncApp,
) -> Result<Value> {
    match probe {
        LanguageProbe::LanguageService { root, file } => {
            language_service_probe(&root, &file, services, cx).await
        }
        LanguageProbe::SettingsReload { root, file } => {
            settings_reload_probe(&root, &file, services, configuration_events, cx).await
        }
        LanguageProbe::LspFailure {
            root,
            file,
            scenario,
        } => lsp_failure_probe(&root, &file, &scenario, services, configuration_events, cx).await,
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct EffectiveLanguageSettings {
    tab_size: u32,
    format_on_save: String,
    completion_lsp: bool,
    show_completions_on_input: bool,
}

fn effective_language_settings(
    buffer: &Entity<Buffer>,
    cx: &gpui::AsyncApp,
) -> EffectiveLanguageSettings {
    buffer.read_with(cx, |buffer, cx| {
        let settings = language::language_settings::LanguageSettings::for_buffer(buffer, cx);
        EffectiveLanguageSettings {
            tab_size: settings.tab_size.get(),
            format_on_save: format!("{:?}", settings.format_on_save).to_ascii_lowercase(),
            completion_lsp: settings.completions.lsp,
            show_completions_on_input: settings.show_completions_on_input,
        }
    })
}

async fn wait_for_effective_language_settings(
    buffer: &Entity<Buffer>,
    expected: &EffectiveLanguageSettings,
    cx: &mut gpui::AsyncApp,
) -> Result<EffectiveLanguageSettings> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let actual = effective_language_settings(buffer, cx);
        if &actual == expected {
            return Ok(actual);
        }
        ensure!(
            Instant::now() < deadline,
            "effective settings did not reload within 5 seconds; expected {expected:?}, actual {actual:?}"
        );
        cx.background_executor()
            .timer(Duration::from_millis(10))
            .await;
    }
}

async fn wait_for_configuration_result(
    events: &async_channel::Receiver<TerminalEvent>,
    kind: &str,
    success: bool,
    cx: &mut gpui::AsyncApp,
) -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match events.try_recv() {
            Ok(TerminalEvent::ConfigurationReloaded {
                kind: event_kind,
                result,
            }) if event_kind == kind => match result {
                Ok(status) if success => return Ok(status),
                Err(error) if !success => return Ok(error),
                _ => {}
            },
            Ok(_) | Err(async_channel::TryRecvError::Empty) => {}
            Err(async_channel::TryRecvError::Closed) => {
                bail!("configuration event channel closed while waiting for {kind}")
            }
        }
        ensure!(
            Instant::now() < deadline,
            "timed out waiting for {kind} {} event",
            if success { "success" } else { "failure" }
        );
        cx.background_executor()
            .timer(Duration::from_millis(10))
            .await;
    }
}

fn dispatch_probe_keystrokes(
    editor_window: &WindowHandle<Editor>,
    keys: &[&str],
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    let input_window: AnyWindowHandle = (*editor_window).into();
    for key in keys {
        let keystroke =
            Keystroke::parse(key).with_context(|| format!("parse probe keystroke {key}"))?;
        cx.update_window(input_window, |_root, window, cx| {
            window.dispatch_keystroke(keystroke, cx)
        })?;
    }
    Ok(())
}

async fn wait_for_terminal_action(
    events: &async_channel::Receiver<TerminalEvent>,
    expected: actions::TerminalAction,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match events.try_recv() {
            Ok(TerminalEvent::Action(action)) if action == expected => return Ok(()),
            Ok(TerminalEvent::Action(action)) => {
                bail!("expected terminal action {expected:?}, received {action:?}")
            }
            Ok(_) | Err(async_channel::TryRecvError::Empty) => {}
            Err(async_channel::TryRecvError::Closed) => {
                bail!("terminal action event channel closed")
            }
        }
        ensure!(
            Instant::now() < deadline,
            "timed out waiting for terminal action {expected:?}"
        );
        cx.background_executor()
            .timer(Duration::from_millis(5))
            .await;
    }
}

async fn ensure_no_terminal_action(
    events: &async_channel::Receiver<TerminalEvent>,
    duration: Duration,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    let deadline = Instant::now() + duration;
    loop {
        match events.try_recv() {
            Ok(TerminalEvent::Action(action)) => {
                bail!("an unbound key unexpectedly dispatched {action:?}")
            }
            Ok(_) | Err(async_channel::TryRecvError::Empty) => {}
            Err(async_channel::TryRecvError::Closed) => {
                bail!("terminal action event channel closed")
            }
        }
        if Instant::now() >= deadline {
            return Ok(());
        }
        cx.background_executor()
            .timer(Duration::from_millis(5))
            .await;
    }
}

async fn wait_for_rust_analyzer_ready(
    project: &Entity<Project>,
    timeout: Duration,
    cx: &mut gpui::AsyncApp,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if project.read_with(cx, |project, cx| {
            project.language_server_statuses(cx).any(|(_, status)| {
                status.name.to_string() == "rust-analyzer" && status.process_id.is_some()
            })
        }) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        cx.background_executor()
            .timer(Duration::from_millis(10))
            .await;
    }
}

fn trust_probe_worktrees(services: &FileServices, cx: &mut gpui::AsyncApp) -> Result<()> {
    let trusted_worktrees = cx
        .update(|cx| TrustedWorktrees::try_get_global(cx))
        .context("worktree trust service is unavailable")?;
    let worktree_ids = services.worktree_store.read_with(cx, |store, cx| {
        store
            .worktrees()
            .map(|worktree| worktree.read(cx).id())
            .collect::<Vec<_>>()
    });
    trusted_worktrees.update(cx, |trusted_worktrees, cx| {
        trusted_worktrees.trust(
            &services.worktree_store,
            worktree_ids.into_iter().map(PathTrust::Worktree).collect(),
            cx,
        );
    });
    Ok(())
}

async fn bounded_completion_request(
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    position: usize,
    timeout: Duration,
    cx: &mut gpui::AsyncApp,
) -> (String, usize, usize, u128, usize) {
    let started = Instant::now();
    let request = project.update(cx, |project, cx| {
        project.completions(
            buffer,
            position,
            editor::CompletionContext {
                trigger_kind: lsp::CompletionTriggerKind::INVOKED,
                trigger_character: None,
            },
            cx,
        )
    });
    let timer = cx.background_executor().timer(timeout);
    let request = Box::pin(request);
    let timer = Box::pin(timer);
    match futures::future::select(request, timer).await {
        futures::future::Either::Left((result, _)) => match result {
            Ok(responses) => {
                let presentations = completion_presentations(&responses);
                let max_documentation_bytes = presentations
                    .iter()
                    .filter_map(|item| item.documentation.as_ref())
                    .map(String::len)
                    .max()
                    .unwrap_or(0);
                let completion_count = presentations.len();
                let mut prompt = CompletionPrompt::running(1, 1);
                let _ = prompt.complete(1, 1, Ok(presentations));
                let overlay_rows = prompt.overlay().rows.len();
                (
                    "ok".to_owned(),
                    completion_count,
                    max_documentation_bytes,
                    started.elapsed().as_millis(),
                    overlay_rows,
                )
            }
            Err(error) => (
                format!("error: {error:#}"),
                0,
                0,
                started.elapsed().as_millis(),
                0,
            ),
        },
        futures::future::Either::Right(((), pending_request)) => {
            drop(pending_request);
            (
                "cancelled".to_owned(),
                0,
                0,
                started.elapsed().as_millis(),
                0,
            )
        }
    }
}

async fn lsp_failure_probe(
    root_path: &Path,
    file_path: &Path,
    scenario: &str,
    services: &FileServices,
    service_events: async_channel::Receiver<TerminalEvent>,
    cx: &mut gpui::AsyncApp,
) -> Result<Value> {
    let repository = prepare_repository(root_path, None, services, cx).await?;
    trust_probe_worktrees(services, cx)?;
    let document = open_repository_document(file_path, &repository, services, cx).await?;
    let buffer = document.buffer.clone();
    let (redraw_sender, _redraw_receiver) = async_channel::bounded(64);
    let tab = create_document_tab(document, services, redraw_sender, cx)?;
    let original_disk = services
        .file_system
        .load(file_path)
        .await
        .with_context(|| format!("read failure fixture {}", file_path.display()))?;
    let original_buffer = buffer.read_with(cx, |buffer, _| buffer.text());
    ensure!(
        original_buffer == original_disk,
        "failure fixture buffer did not start from disk"
    );
    let marker_position = original_buffer
        .find("stub_")
        .map(|offset| offset + "stub_".len())
        .unwrap_or(0);

    let initially_ready = wait_for_rust_analyzer_ready(
        &services.project,
        if matches!(
            scenario,
            "request-error"
                | "hang-request"
                | "crash-request"
                | "malformed-response"
                | "large-payloads"
                | "formatter-error"
                | "huge-stderr"
                | "restart-once"
        ) {
            Duration::from_secs(10)
        } else {
            Duration::from_millis(500)
        },
        cx,
    )
    .await;

    if scenario == "formatter-error" {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let readiness = bounded_completion_request(
                &services.project,
                &buffer,
                marker_position,
                Duration::from_secs(1),
                cx,
            )
            .await;
            if readiness.0 == "ok" && readiness.1 == 2 {
                break;
            }
            ensure!(
                Instant::now() < deadline,
                "formatter fixture buffer was not registered with rust-analyzer; outcome {}, count {}",
                readiness.0,
                readiness.1
            );
            cx.background_executor()
                .timer(Duration::from_millis(25))
                .await;
        }
    }

    let edited_text = format!("{original_buffer}// continued after {scenario}\n");
    tab.editor_window.update(cx, |editor, window, cx| {
        editor.select_all(&SelectAll, window, cx);
        editor.insert(&edited_text, window, cx);
    })?;
    let dirty_after_edit = document_state(&tab.document, cx).dirty;
    tab.editor_window
        .update(cx, |editor, window, cx| editor.undo(&Undo, window, cx))?;
    let text_after_undo = buffer.read_with(cx, |buffer, _| buffer.text());
    tab.editor_window
        .update(cx, |editor, window, cx| editor.redo(&Redo, window, cx))?;
    let text_after_redo = buffer.read_with(cx, |buffer, _| buffer.text());
    ensure!(
        dirty_after_edit && text_after_undo == original_buffer && text_after_redo == edited_text,
        "LSP failure changed the Editor undo/redo authority"
    );

    let mut formatter_failure = None;
    let mut dirty_after_formatter_failure = None;
    let mut disk_after_formatter_failure = None;
    if scenario == "formatter-error" {
        let expected = EffectiveLanguageSettings {
            tab_size: 4,
            format_on_save: "on".to_owned(),
            completion_lsp: true,
            show_completions_on_input: true,
        };
        wait_for_effective_language_settings(&buffer, &expected, cx).await?;
        let error = save_tab(&tab, services, cx)
            .await
            .expect_err("controlled formatter error unexpectedly saved");
        formatter_failure = Some(format!("{error:#}"));
        dirty_after_formatter_failure = Some(document_state(&tab.document, cx).dirty);
        disk_after_formatter_failure = Some(
            services
                .file_system
                .load(file_path)
                .await
                .with_context(|| format!("read formatter failure disk {}", file_path.display()))?,
        );
        ensure!(
            dirty_after_formatter_failure == Some(true)
                && disk_after_formatter_failure.as_deref() == Some(original_disk.as_str()),
            "formatter failure changed disk or cleared dirty state"
        );
        let local_settings_path = root_path.join(".zed/settings.json");
        std::fs::write(
            &local_settings_path,
            r#"{
              "format_on_save": "off",
              "remove_trailing_whitespace_on_save": false,
              "ensure_final_newline_on_save": false
            }"#,
        )
        .with_context(|| {
            format!(
                "disable formatter after controlled failure {}",
                local_settings_path.display()
            )
        })?;
        let settings_deadline = Instant::now() + Duration::from_secs(5);
        while effective_language_settings(&buffer, cx).format_on_save != "off" {
            ensure!(
                Instant::now() < settings_deadline,
                "format_on_save did not turn off after formatter failure"
            );
            cx.background_executor()
                .timer(Duration::from_millis(10))
                .await;
        }
    }
    save_tab(&tab, services, cx).await?;
    let saved_disk = services
        .file_system
        .load(file_path)
        .await
        .with_context(|| format!("read continued save {}", file_path.display()))?;
    ensure!(
        saved_disk == edited_text && !document_state(&tab.document, cx).dirty,
        "editing/save did not remain usable after LSP failure"
    );

    let restart_requested = scenario == "restart-once";
    if restart_requested {
        // A crashed language server is intentionally left stopped by Project until the
        // user asks for a restart. Exercise that recovery path explicitly instead of
        // mistaking Project's temporary empty completion set for a recovered server.
        cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
        services.project.update(cx, |project, cx| {
            project.restart_language_servers_for_buffers(
                vec![buffer.clone()],
                Default::default(),
                true,
                cx,
            );
        });
    }
    let (
        request_outcome,
        completion_count,
        max_documentation_bytes,
        request_elapsed_ms,
        completion_overlay_rows,
    ) = if scenario == "restart-once" {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let result = bounded_completion_request(
                &services.project,
                &buffer,
                marker_position,
                Duration::from_secs(2),
                cx,
            )
            .await;
            if result.0 == "ok" && result.1 == 2 {
                break result;
            }
            ensure!(
                Instant::now() < deadline,
                "language server did not recover after controlled restart; last outcome {}, count {}",
                result.0,
                result.1
            );
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
        }
    } else {
        bounded_completion_request(
            &services.project,
            &buffer,
            marker_position,
            if scenario.contains("hang") {
                Duration::from_millis(250)
            } else {
                Duration::from_secs(3)
            },
            cx,
        )
        .await
    };

    let diagnostic_deadline = Instant::now() + Duration::from_secs(5);
    let diagnostic_summary = loop {
        let summary = services
            .project
            .read_with(cx, |project, cx| project.diagnostic_summary(false, cx));
        if scenario != "large-payloads"
            || summary.error_count.saturating_add(summary.warning_count) >= 10_000
            || Instant::now() >= diagnostic_deadline
        {
            break summary;
        }
        cx.background_executor()
            .timer(Duration::from_millis(10))
            .await;
    };
    let (diagnostic_item_count, diagnostic_overlay_rows) = if scenario == "large-payloads" {
        let items = collect_project_diagnostics(
            services.project.clone(),
            Some(root_path.to_path_buf()),
            vec![(file_path.to_path_buf(), buffer.clone())],
            cx,
        )
        .await?;
        let item_count = items.len();
        let mut prompt = DiagnosticsPrompt::running(1);
        let _ = prompt.complete(1, Ok(items));
        (item_count, prompt.overlay().rows.len())
    } else {
        (
            diagnostic_summary
                .error_count
                .saturating_add(diagnostic_summary.warning_count),
            0,
        )
    };
    let statuses = services.project.read_with(cx, |project, cx| {
        project
            .language_server_statuses(cx)
            .map(|(_, status)| {
                serde_json::json!({
                    "name": status.name.to_string(),
                    "process_id": status.process_id,
                })
            })
            .collect::<Vec<_>>()
    });

    let _keepalive_window = cx.update(|cx| open_editor(buffer.clone(), cx))?;
    let close_started = Instant::now();
    tab.editor_window
        .update(cx, |_editor, window, _cx| window.remove_window())?;
    let close_elapsed_ms = close_started.elapsed().as_millis();
    ensure!(
        close_elapsed_ms <= 250,
        "closing an editor during an LSP failure took {close_elapsed_ms} ms"
    );

    let mut service_notices = Vec::new();
    while let Ok(event) = service_events.try_recv() {
        if let TerminalEvent::LanguageServiceNotice { level, message } = event {
            service_notices.push(serde_json::json!({
                "level": level,
                "message": message,
            }));
        }
    }

    Ok(serde_json::json!({
        "scenario": scenario,
        "initially_ready": initially_ready,
        "restart_requested": restart_requested,
        "request": {
            "outcome": request_outcome,
            "user_message": (!initially_ready)
                .then(|| language_service_unavailable_message("completion")),
            "elapsed_ms": request_elapsed_ms,
            "completion_count": completion_count,
            "max_documentation_bytes": max_documentation_bytes,
            "overlay_rows": completion_overlay_rows,
        },
        "diagnostics": {
            "errors": diagnostic_summary.error_count,
            "warnings": diagnostic_summary.warning_count,
            "item_count": diagnostic_item_count,
            "overlay_rows": diagnostic_overlay_rows,
        },
        "limits": {
            "language_items": MAX_LANGUAGE_RESPONSE_ITEMS,
            "language_text_bytes": MAX_LANGUAGE_TEXT_BYTES,
            "overlay_rows": MAX_OVERLAY_SNAPSHOT_ROWS,
        },
        "editor": {
            "dirty_after_edit": dirty_after_edit,
            "undo_restored": text_after_undo == original_buffer,
            "redo_restored": text_after_redo == edited_text,
            "saved": saved_disk == edited_text,
            "close_elapsed_ms": close_elapsed_ms,
        },
        "formatter_failure": {
            "error": formatter_failure,
            "dirty": dirty_after_formatter_failure,
            "disk": disk_after_formatter_failure,
        },
        "statuses": statuses,
        "service_notices": service_notices,
    }))
}

async fn settings_reload_probe(
    root_path: &Path,
    file_path: &Path,
    services: &FileServices,
    configuration_events: async_channel::Receiver<TerminalEvent>,
    cx: &mut gpui::AsyncApp,
) -> Result<Value> {
    let repository = prepare_repository(root_path, None, services, cx).await?;
    trust_probe_worktrees(services, cx)?;
    let document = open_repository_document(file_path, &repository, services, cx).await?;
    let buffer = document.buffer.clone();
    let (redraw_sender, _redraw_receiver) = async_channel::bounded(64);
    let tab = create_document_tab(document, services, redraw_sender, cx)?;

    let initial_keymap_status =
        wait_for_configuration_result(&configuration_events, "user keymap", true, cx).await?;
    let initial_expected = EffectiveLanguageSettings {
        tab_size: 5,
        format_on_save: "off".to_owned(),
        completion_lsp: true,
        show_completions_on_input: true,
    };
    let initial = wait_for_effective_language_settings(&buffer, &initial_expected, cx).await?;

    dispatch_probe_keystrokes(&tab.editor_window, &["ctrl-k", "ctrl-p"], cx)?;
    wait_for_terminal_action(
        &configuration_events,
        actions::TerminalAction::CommandPalette,
        cx,
    )
    .await?;
    dispatch_probe_keystrokes(&tab.editor_window, &["f1"], cx)?;
    ensure_no_terminal_action(&configuration_events, Duration::from_millis(75), cx).await?;

    let keymap_path = paths::keymap_file().clone();
    std::fs::write(&keymap_path, "{")
        .with_context(|| format!("write invalid keymap {}", keymap_path.display()))?;
    let invalid_keymap_error =
        wait_for_configuration_result(&configuration_events, "user keymap", false, cx).await?;
    dispatch_probe_keystrokes(&tab.editor_window, &["ctrl-k", "ctrl-p"], cx)?;
    wait_for_terminal_action(
        &configuration_events,
        actions::TerminalAction::CommandPalette,
        cx,
    )
    .await?;

    std::fs::write(
        &keymap_path,
        r#"[
          {
            "context": "Editor",
            "bindings": {
              "f1": null,
              "f3": "editor::Hover"
            }
          }
        ]"#,
    )
    .with_context(|| format!("write replacement keymap {}", keymap_path.display()))?;
    let replacement_keymap_status =
        wait_for_configuration_result(&configuration_events, "user keymap", true, cx).await?;
    dispatch_probe_keystrokes(&tab.editor_window, &["f3"], cx)?;
    wait_for_terminal_action(&configuration_events, actions::TerminalAction::Hover, cx).await?;
    dispatch_probe_keystrokes(&tab.editor_window, &["f1"], cx)?;
    ensure_no_terminal_action(&configuration_events, Duration::from_millis(75), cx).await?;

    let local_settings_path = root_path.join(".zed/settings.json");
    std::fs::write(
        &local_settings_path,
        r#"{
          "tab_size": 6,
          "format_on_save": "off",
          "completions": { "lsp": true },
          "show_completions_on_input": true,
          "languages": {
            "Rust": {
              "tab_size": 7,
              "format_on_save": "on",
              "completions": { "lsp": false },
              "show_completions_on_input": false
            }
          }
        }"#,
    )
    .with_context(|| format!("write updated settings {}", local_settings_path.display()))?;
    let updated_expected = EffectiveLanguageSettings {
        tab_size: 7,
        format_on_save: "on".to_owned(),
        completion_lsp: false,
        show_completions_on_input: false,
    };
    let updated = wait_for_effective_language_settings(&buffer, &updated_expected, cx).await?;

    let language_server_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let ready = services.project.read_with(cx, |project, cx| {
            project.language_server_statuses(cx).any(|(_, status)| {
                status.name.to_string() == "rust-analyzer" && status.process_id.is_some()
            })
        });
        if ready {
            break;
        }
        ensure!(
            Instant::now() < language_server_deadline,
            "rust-analyzer did not become ready for format-on-save"
        );
        cx.background_executor()
            .timer(Duration::from_millis(10))
            .await;
    }
    tab.editor_window.update(cx, |editor, window, cx| {
        editor.select_all(&SelectAll, window, cx);
        editor.insert("fn main() { let value = 1; }   \n", window, cx);
    })?;
    save_tab(&tab, services, cx).await?;
    let formatted_disk = services
        .file_system
        .load(file_path)
        .await
        .with_context(|| format!("read format-on-save result {}", file_path.display()))?;
    ensure!(
        formatted_disk == "fn main() { let value = 1; }\n",
        "format-on-save result differs: {formatted_disk:?}"
    );

    std::fs::write(&local_settings_path, "{")
        .with_context(|| format!("write invalid settings {}", local_settings_path.display()))?;
    let invalid_settings_error =
        wait_for_configuration_result(&configuration_events, "project settings", false, cx).await?;
    let retained_after_invalid = effective_language_settings(&buffer, cx);
    ensure!(
        retained_after_invalid == updated_expected,
        "invalid project settings replaced the last valid settings"
    );

    std::fs::write(
        &local_settings_path,
        r#"{
          "tab_size": 8,
          "languages": {
            "Rust": {
              "tab_size": 9,
              "format_on_save": "off",
              "completions": { "lsp": true },
              "show_completions_on_input": true
            }
          }
        }"#,
    )
    .with_context(|| format!("restore settings {}", local_settings_path.display()))?;
    let recovered_expected = EffectiveLanguageSettings {
        tab_size: 9,
        format_on_save: "off".to_owned(),
        completion_lsp: true,
        show_completions_on_input: true,
    };
    let recovered = wait_for_effective_language_settings(&buffer, &recovered_expected, cx).await?;
    let recovery_status =
        wait_for_configuration_result(&configuration_events, "project settings", true, cx).await?;

    tab.editor_window.update(cx, |editor, window, cx| {
        editor.select_all(&SelectAll, window, cx);
        editor.insert("fn main() { let value = 2; }   \n", window, cx);
    })?;
    save_tab(&tab, services, cx).await?;
    let unformatted_disk = services
        .file_system
        .load(file_path)
        .await
        .with_context(|| format!("read format-off result {}", file_path.display()))?;

    Ok(serde_json::json!({
        "initial": initial,
        "updated": updated,
        "retained_after_invalid": retained_after_invalid,
        "recovered": recovered,
        "format_on_save_disk": formatted_disk,
        "format_off_disk": unformatted_disk,
        "settings": {
            "invalid_error": invalid_settings_error,
            "recovery_status": recovery_status,
            "path": local_settings_path.display().to_string(),
        },
        "keymap": {
            "initial_status": initial_keymap_status,
            "invalid_error": invalid_keymap_error,
            "replacement_status": replacement_keymap_status,
            "multi_chord_rebind": true,
            "unbind": true,
            "last_good_retained": true,
            "replacement_rebind": true,
            "path": keymap_path.display().to_string(),
        },
    }))
}

async fn language_service_probe(
    root_path: &Path,
    file_path: &Path,
    services: &FileServices,
    cx: &mut gpui::AsyncApp,
) -> Result<Value> {
    let repository = prepare_repository(root_path, None, services, cx).await?;
    trust_probe_worktrees(services, cx)?;
    let document = open_repository_document(file_path, &repository, services, cx).await?;
    let buffer = document.buffer.clone();
    let (event_sender, event_receiver) = async_channel::bounded(64);
    let tab = create_document_tab(document, services, event_sender.clone(), cx)?;
    let peer_path = file_path.with_file_name("lib.rs");
    let peer = if peer_path != file_path && peer_path.is_file() {
        let document = open_repository_document(&peer_path, &repository, services, cx).await?;
        let buffer = document.buffer.clone();
        let tab = create_document_tab(document, services, event_sender.clone(), cx)?;
        Some((peer_path, buffer, tab))
    } else {
        None
    };

    let text = buffer.read_with(cx, |buffer, _| buffer.text());
    let marker = "stub_";
    let position = text
        .find(marker)
        .map(|offset| offset + marker.len())
        .context("language-service probe file must contain stub_")?;

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let ready = services.project.read_with(cx, |project, cx| {
            project.language_server_statuses(cx).any(|(_, status)| {
                status.name.to_string() == "rust-analyzer" && status.process_id.is_some()
            })
        });
        if ready {
            break;
        }
        if Instant::now() >= deadline {
            let language = buffer.read_with(cx, |buffer, _| {
                buffer.language().map(|language| language.name().clone())
            });
            let adapters = language
                .as_ref()
                .map(|language| {
                    services
                        .language_registry
                        .lsp_adapters(language)
                        .into_iter()
                        .map(|adapter| adapter.name().to_string())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let statuses = services.project.read_with(cx, |project, cx| {
                project
                    .language_server_statuses(cx)
                    .map(|(_, status)| (status.name.to_string(), status.process_id))
                    .collect::<Vec<_>>()
            });
            bail!(
                "rust-analyzer did not become ready within 15 seconds; language={language:?}, adapters={adapters:?}, statuses={statuses:?}"
            );
        }
        cx.background_executor()
            .timer(Duration::from_millis(25))
            .await;
    }

    let completion_task = services.project.update(cx, |project, cx| {
        project.completions(
            &buffer,
            position,
            editor::CompletionContext {
                trigger_kind: lsp::CompletionTriggerKind::INVOKED,
                trigger_character: None,
            },
            cx,
        )
    });
    let completion_responses = completion_task
        .await
        .context("request fixture completions")?;
    let completions = completion_responses
        .into_iter()
        .flat_map(|response| response.completions)
        .map(|completion| {
            let lsp_completion = completion.source.lsp_completion(false);
            serde_json::json!({
                "label": completion.label.text,
                "new_text": completion.new_text,
                "detail": lsp_completion.as_ref().and_then(|item| item.detail.clone()),
                "kind": lsp_completion
                    .as_ref()
                    .and_then(|item| item.kind)
                    .map(|kind| format!("{kind:?}")),
            })
        })
        .collect::<Vec<_>>();

    let hover_task = services
        .project
        .update(cx, |project, cx| project.hover(&buffer, position, cx));
    let hovers = hover_task
        .await
        .unwrap_or_default()
        .into_iter()
        .flat_map(|hover| hover.contents)
        .map(|block| {
            serde_json::json!({
                "kind": format!("{:?}", block.kind),
                "text": block.text,
            })
        })
        .collect::<Vec<_>>();

    let diagnostic_deadline = Instant::now() + Duration::from_secs(5);
    let diagnostic_summary = loop {
        let summary = services
            .project
            .read_with(cx, |project, cx| project.diagnostic_summary(false, cx));
        if summary.error_count > 0 || summary.warning_count > 0 {
            break summary;
        }
        ensure!(
            Instant::now() < diagnostic_deadline,
            "fixture diagnostic was not published within 5 seconds"
        );
        cx.background_executor()
            .timer(Duration::from_millis(25))
            .await;
    };

    let statuses = services.project.read_with(cx, |project, cx| {
        project
            .language_server_statuses(cx)
            .map(|(id, status)| {
                serde_json::json!({
                    "id": id.0,
                    "name": status.name.to_string(),
                    "language": status.language_name.as_ref().map(ToString::to_string),
                    "process_id": status.process_id,
                })
            })
            .collect::<Vec<_>>()
    });
    let language = buffer.read_with(cx, |buffer, _| {
        buffer
            .language()
            .map(|language| language.name().to_string())
    });

    let prefix = &text[..position];
    let display_row = prefix.bytes().filter(|byte| *byte == b'\n').count();
    let byte_column = prefix
        .rfind('\n')
        .map_or(position, |newline| position.saturating_sub(newline + 1));
    move_caret_to_text_position(
        &tab.editor_window,
        TextPosition {
            row: display_row,
            byte_column,
        },
        cx,
    )?;
    let terminal_generation = tab
        .completion_generation
        .load(AtomicOrdering::SeqCst)
        .saturating_add(1);
    tab.editor_window.update(cx, |editor, window, cx| {
        editor.show_completions(&ShowCompletions, window, cx)
    })?;
    let terminal_deadline = Instant::now() + Duration::from_secs(5);
    let terminal_items = loop {
        match event_receiver.try_recv() {
            Ok(TerminalEvent::CompletionFinished {
                buffer_id: _,
                generation,
                result,
                ..
            }) if generation == terminal_generation => break result.map_err(anyhow::Error::msg)?,
            Ok(_) | Err(async_channel::TryRecvError::Empty) => {}
            Err(async_channel::TryRecvError::Closed) => {
                bail!("terminal completion event channel closed")
            }
        }
        ensure!(
            Instant::now() < terminal_deadline,
            "terminal completion projection was not published within 5 seconds"
        );
        cx.background_executor()
            .timer(Duration::from_millis(10))
            .await;
    };
    loop {
        let menu_visible = tab.editor_window.update(cx, |editor, _window, _cx| {
            editor.has_visible_completions_menu()
        })?;
        if menu_visible {
            break;
        }
        ensure!(
            Instant::now() < terminal_deadline,
            "Zed completion menu was not ready within 5 seconds"
        );
        cx.background_executor()
            .timer(Duration::from_millis(10))
            .await;
    }
    let confirm_task = tab.editor_window.update(cx, |editor, window, cx| {
        editor.confirm_completion(&ConfirmCompletion { item_ix: Some(0) }, window, cx)
    })?;
    confirm_task
        .context("Zed completion menu had no first item")?
        .await
        .context("apply terminal-projected completion")?;
    let text_after_completion = tab
        .editor_window
        .update(cx, |editor, _window, cx| editor.text(cx))?;
    tab.editor_window
        .update(cx, |editor, window, cx| editor.undo(&Undo, window, cx))?;
    let text_after_completion_undo = tab
        .editor_window
        .update(cx, |editor, _window, cx| editor.text(cx))?;

    let terminal_hover_generation = 1;
    let terminal_hover_buffer_id = start_hover_request(
        &tab.editor_window,
        &services.project,
        terminal_hover_generation,
        event_sender.clone(),
        cx,
    )?;
    let terminal_hover_deadline = Instant::now() + Duration::from_secs(5);
    let terminal_hover_items = loop {
        match event_receiver.try_recv() {
            Ok(TerminalEvent::HoverFinished {
                buffer_id,
                generation,
                result,
            }) if buffer_id == terminal_hover_buffer_id
                && generation == terminal_hover_generation =>
            {
                break result.map_err(anyhow::Error::msg)?;
            }
            Ok(_) | Err(async_channel::TryRecvError::Empty) => {}
            Err(async_channel::TryRecvError::Closed) => {
                bail!("terminal hover event channel closed")
            }
        }
        ensure!(
            Instant::now() < terminal_hover_deadline,
            "terminal hover projection was not published within 5 seconds"
        );
        cx.background_executor()
            .timer(Duration::from_millis(10))
            .await;
    };
    let mut terminal_hover_prompt =
        HoverPrompt::running(terminal_hover_buffer_id, terminal_hover_generation);
    ensure!(terminal_hover_prompt.complete(
        terminal_hover_buffer_id,
        terminal_hover_generation,
        Ok(terminal_hover_items.clone())
    ));
    let terminal_hover_overlay = terminal_hover_prompt.overlay();

    let terminal_diagnostic_items = collect_project_diagnostics(
        services.project.clone(),
        Some(root_path.to_path_buf()),
        vec![(file_path.to_path_buf(), buffer.clone())],
        cx,
    )
    .await?;
    let mut terminal_diagnostics_prompt = DiagnosticsPrompt::running(1);
    ensure!(terminal_diagnostics_prompt.complete(1, Ok(terminal_diagnostic_items.clone())));
    let terminal_diagnostics_overlay = terminal_diagnostics_prompt.overlay();
    let selected_diagnostic = terminal_diagnostics_prompt
        .selected_item()
        .context("fixture produced no terminal diagnostic")?;
    let mut probe_tabs = vec![tab];
    let peer_buffer = peer
        .as_ref()
        .map(|(path, buffer, _)| (path.clone(), buffer.clone()));
    if let Some((_, _, peer_tab)) = peer {
        probe_tabs.push(peer_tab);
    }
    let mut probe_active_index = 0;
    let diagnostic_navigation = navigate_to_diagnostic(
        &selected_diagnostic,
        Some(&repository),
        services,
        &mut probe_tabs,
        &mut probe_active_index,
        event_sender.clone(),
        cx,
    )
    .await?;
    let (_, diagnostic_point, _) =
        active_editor_buffer_point(&probe_tabs[probe_active_index].editor_window, cx)?;

    let mut terminal_location_results = Vec::new();
    for (index, kind) in [
        LocationRequestKind::Definition,
        LocationRequestKind::TypeDefinition,
        LocationRequestKind::References,
    ]
    .into_iter()
    .enumerate()
    {
        let generation = u64::try_from(index).unwrap_or_default().saturating_add(10);
        let buffer_id = start_locations_request(
            kind,
            &probe_tabs[probe_active_index].editor_window,
            &services.project,
            generation,
            Some(root_path.to_path_buf()),
            event_sender.clone(),
            cx,
        )?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let items = loop {
            match event_receiver.try_recv() {
                Ok(TerminalEvent::LocationsFinished {
                    buffer_id: completed_buffer_id,
                    generation: completed_generation,
                    kind: completed_kind,
                    result,
                }) if completed_buffer_id == buffer_id
                    && completed_generation == generation
                    && completed_kind == kind =>
                {
                    break result.map_err(anyhow::Error::msg)?;
                }
                Ok(_) | Err(async_channel::TryRecvError::Empty) => {}
                Err(async_channel::TryRecvError::Closed) => {
                    bail!("terminal location event channel closed")
                }
            }
            ensure!(
                Instant::now() < deadline,
                "terminal {} projection was not published within 5 seconds",
                kind.title().to_lowercase()
            );
            cx.background_executor()
                .timer(Duration::from_millis(10))
                .await;
        };
        let mut prompt = LocationsPrompt::running(buffer_id, generation, kind);
        ensure!(prompt.complete(buffer_id, generation, kind, Ok(items.clone())));
        let overlay_rows = prompt
            .overlay()
            .rows
            .into_iter()
            .map(|row| row.text)
            .collect::<Vec<_>>();
        terminal_location_results.push((kind, items, overlay_rows));
    }
    let symbol_generation = 20;
    let (_, _, symbol_buffer_id) =
        active_editor_buffer_point(&probe_tabs[probe_active_index].editor_window, cx)?;
    start_project_symbols_request(
        &services.project,
        symbol_buffer_id,
        symbol_generation,
        "stub".to_owned(),
        Some(root_path.to_path_buf()),
        event_sender.clone(),
        cx,
    );
    let symbol_deadline = Instant::now() + Duration::from_secs(5);
    let symbol_items = loop {
        match event_receiver.try_recv() {
            Ok(TerminalEvent::LocationsFinished {
                buffer_id,
                generation,
                kind: LocationRequestKind::ProjectSymbols,
                result,
            }) if buffer_id == symbol_buffer_id && generation == symbol_generation => {
                break result.map_err(anyhow::Error::msg)?;
            }
            Ok(_) | Err(async_channel::TryRecvError::Empty) => {}
            Err(async_channel::TryRecvError::Closed) => {
                bail!("terminal project-symbol event channel closed")
            }
        }
        ensure!(
            Instant::now() < symbol_deadline,
            "terminal project symbols were not published within 5 seconds"
        );
        cx.background_executor()
            .timer(Duration::from_millis(10))
            .await;
    };
    let mut symbol_prompt = LocationsPrompt::running(
        symbol_buffer_id,
        symbol_generation,
        LocationRequestKind::ProjectSymbols,
    );
    ensure!(symbol_prompt.complete(
        symbol_buffer_id,
        symbol_generation,
        LocationRequestKind::ProjectSymbols,
        Ok(symbol_items.clone())
    ));
    terminal_location_results.push((
        LocationRequestKind::ProjectSymbols,
        symbol_items,
        symbol_prompt
            .overlay()
            .rows
            .into_iter()
            .map(|row| row.text)
            .collect(),
    ));
    let definition_target = terminal_location_results
        .iter()
        .find(|(kind, _, _)| *kind == LocationRequestKind::Definition)
        .and_then(|(_, items, _)| items.first())
        .context("fixture produced no definition target")?;
    let semantic_navigation = navigate_to_location(
        definition_target,
        Some(&repository),
        services,
        &mut probe_tabs,
        &mut probe_active_index,
        event_sender.clone(),
        cx,
    )
    .await?;
    let (_, semantic_point, _) =
        active_editor_buffer_point(&probe_tabs[probe_active_index].editor_window, cx)?;

    let reference_items = terminal_location_results
        .iter()
        .find(|(kind, _, _)| *kind == LocationRequestKind::References)
        .map(|(_, items, _)| items.clone())
        .context("fixture produced no reference targets")?;
    let multibuffer_tab = create_locations_multibuffer_tab(
        "References".to_owned(),
        &reference_items,
        Some(&repository),
        services,
        event_sender.clone(),
        cx,
    )
    .await?;
    let multibuffer_source_count = multibuffer_tab
        .multi_buffer
        .as_ref()
        .context("reference result did not create a MultiBuffer tab")?
        .buffer
        .read_with(cx, |multi_buffer, _| {
            multi_buffer.all_buffers_iter().count()
        });
    let multibuffer_text = multibuffer_tab
        .editor_window
        .update(cx, |editor, _window, cx| editor.text(cx))?;
    let multibuffer_marker = multibuffer_text
        .find("stub_")
        .context("reference MultiBuffer omitted the fixture symbol")?;
    let multibuffer_prefix = &multibuffer_text[..multibuffer_marker];
    let multibuffer_row = multibuffer_prefix
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count();
    let multibuffer_column = multibuffer_prefix
        .rfind('\n')
        .map_or(multibuffer_marker, |newline| {
            multibuffer_marker.saturating_sub(newline + 1)
        });
    let multibuffer_before = multibuffer_tab
        .multi_buffer
        .as_ref()
        .expect("MultiBuffer checked above")
        .source_buffers
        .iter()
        .map(|buffer| {
            buffer.read_with(cx, |buffer, cx| {
                let path = buffer
                    .file()
                    .map(|file| {
                        file.as_local()
                            .map(|file| file.abs_path(cx))
                            .unwrap_or_else(|| file.full_path(cx))
                    })
                    .unwrap_or_default();
                (path, buffer.text())
            })
        })
        .collect::<BTreeMap<_, _>>();
    move_caret_to_text_position(
        &multibuffer_tab.editor_window,
        TextPosition {
            row: multibuffer_row,
            byte_column: multibuffer_column,
        },
        cx,
    )?;
    multibuffer_tab
        .editor_window
        .update(cx, |editor, window, cx| editor.insert("mb_", window, cx))?;
    let multibuffer_after_edit = multibuffer_tab
        .multi_buffer
        .as_ref()
        .expect("MultiBuffer checked above")
        .source_buffers
        .iter()
        .map(|buffer| {
            buffer.read_with(cx, |buffer, cx| {
                let path = buffer
                    .file()
                    .map(|file| {
                        file.as_local()
                            .map(|file| file.abs_path(cx))
                            .unwrap_or_else(|| file.full_path(cx))
                    })
                    .unwrap_or_default();
                (path, buffer.text())
            })
        })
        .collect::<BTreeMap<_, _>>();
    let changed_multibuffer_paths = multibuffer_after_edit
        .iter()
        .filter_map(|(path, text)| {
            (multibuffer_before.get(path) != Some(text)).then_some(path.clone())
        })
        .collect::<Vec<_>>();
    ensure!(
        changed_multibuffer_paths.len() == 1,
        "MultiBuffer edit changed {:?}, expected exactly one source",
        changed_multibuffer_paths
    );
    let changed_multibuffer_path = changed_multibuffer_paths[0].clone();
    save_tab(&multibuffer_tab, services, cx).await?;
    let multibuffer_disk_after_save = std::fs::read_to_string(&changed_multibuffer_path)
        .with_context(|| {
            format!(
                "read {} after MultiBuffer save",
                changed_multibuffer_path.display()
            )
        })?;
    multibuffer_tab
        .editor_window
        .update(cx, |editor, window, cx| editor.undo(&Undo, window, cx))?;
    let multibuffer_after_undo = multibuffer_tab
        .multi_buffer
        .as_ref()
        .expect("MultiBuffer checked above")
        .source_buffers
        .iter()
        .map(|buffer| {
            buffer.read_with(cx, |buffer, cx| {
                let path = buffer
                    .file()
                    .map(|file| {
                        file.as_local()
                            .map(|file| file.abs_path(cx))
                            .unwrap_or_else(|| file.full_path(cx))
                    })
                    .unwrap_or_default();
                (path, buffer.text())
            })
        })
        .collect::<BTreeMap<_, _>>();
    ensure!(
        multibuffer_after_undo == multibuffer_before,
        "MultiBuffer undo did not restore every source buffer"
    );
    save_tab(&multibuffer_tab, services, cx).await?;
    let multibuffer_disk_after_restore = std::fs::read_to_string(&changed_multibuffer_path)
        .with_context(|| {
            format!(
                "read {} after MultiBuffer restore",
                changed_multibuffer_path.display()
            )
        })?;
    let multibuffer_changed_label = worktree_path_label(
        changed_multibuffer_path
            .strip_prefix(root_path)
            .unwrap_or(&changed_multibuffer_path),
    );
    probe_tabs.push(multibuffer_tab);
    cx.background_executor()
        .timer(Duration::from_millis(25))
        .await;

    move_caret_to_text_position(
        &probe_tabs[0].editor_window,
        TextPosition {
            row: display_row,
            byte_column,
        },
        cx,
    )?;
    let rename_generation = 30;
    let (rename_buffer, rename_point, rename_buffer_id) = start_rename_request(
        &probe_tabs[0].editor_window,
        &services.project,
        rename_generation,
        event_sender.clone(),
        cx,
    )?;
    let rename_deadline = Instant::now() + Duration::from_secs(5);
    let rename_preparation = loop {
        match event_receiver.try_recv() {
            Ok(TerminalEvent::RenamePrepared {
                buffer_id,
                generation,
                result,
            }) if buffer_id == rename_buffer_id && generation == rename_generation => {
                break result.map_err(anyhow::Error::msg)?;
            }
            Ok(_) | Err(async_channel::TryRecvError::Empty) => {}
            Err(async_channel::TryRecvError::Closed) => {
                bail!("terminal rename event channel closed")
            }
        }
        ensure!(
            Instant::now() < rename_deadline,
            "terminal rename preparation was not published within 5 seconds"
        );
        cx.background_executor()
            .timer(Duration::from_millis(10))
            .await;
    };
    let mut rename_prompt = RenamePrompt::running(
        rename_buffer.clone(),
        rename_buffer_id,
        rename_generation,
        rename_point,
    );
    ensure!(rename_prompt.complete(
        rename_buffer_id,
        rename_generation,
        Ok(rename_preparation.clone())
    ));
    let rename_overlay_rows = rename_prompt
        .overlay()
        .rows
        .into_iter()
        .map(|row| row.text)
        .collect::<Vec<_>>();
    let rename_name = "renamed_fixture".to_owned();
    let rename_main_before_preview = buffer.read_with(cx, |buffer, _| buffer.text());
    let rename_peer_before_preview = peer_buffer
        .as_ref()
        .map(|(_, buffer)| buffer.read_with(cx, |buffer, _| buffer.text()));

    let (rejected_server_id, rejected_request) = request_rename_workspace_edit(
        &services.project,
        &rename_buffer,
        rename_point,
        rename_name.clone(),
        None,
        cx,
    )?;
    let rejected_edit = rejected_request.await?;
    let rejected_preview = create_rename_preview_tab(
        rename_buffer.clone(),
        rename_point,
        rename_name.clone(),
        rejected_server_id,
        rejected_edit,
        repository.root.canonical_path().to_path_buf(),
        Some(&repository),
        services,
        cx,
    )
    .await?;
    let rejected_preview_text = rejected_preview
        .editor_window
        .update(cx, |editor, _window, cx| editor.text(cx))?;
    rejected_preview
        .editor_window
        .update(cx, |_editor, window, _cx| window.remove_window())?;
    drop(rejected_preview);
    let rename_rejection_unchanged = buffer.read_with(cx, |buffer, _| buffer.text())
        == rename_main_before_preview
        && peer_buffer
            .as_ref()
            .map(|(_, buffer)| buffer.read_with(cx, |buffer, _| buffer.text()))
            == rename_peer_before_preview;
    ensure!(
        rename_rejection_unchanged,
        "rejecting the rename preview changed a source buffer"
    );

    let (preview_server_id, preview_request) = request_rename_workspace_edit(
        &services.project,
        &rename_buffer,
        rename_point,
        rename_name.clone(),
        None,
        cx,
    )?;
    let preview_edit = preview_request.await?;
    let rename_preview_tab = create_rename_preview_tab(
        rename_buffer.clone(),
        rename_point,
        rename_name.clone(),
        preview_server_id,
        preview_edit,
        repository.root.canonical_path().to_path_buf(),
        Some(&repository),
        services,
        cx,
    )
    .await?;
    let rename_preview_text = rename_preview_tab
        .editor_window
        .update(cx, |editor, _window, cx| editor.text(cx))?;
    let rename_pending = rename_preview_tab
        .multi_buffer
        .as_ref()
        .and_then(|multi_buffer| multi_buffer.pending_rename.clone())
        .context("rename preview tab has no pending transaction")?;
    let rename_preview_source_count = rename_preview_tab
        .multi_buffer
        .as_ref()
        .map(|multi_buffer| multi_buffer.source_buffers.len())
        .unwrap_or_default();
    let rename_preview_buffer_count = rename_preview_tab
        .multi_buffer
        .as_ref()
        .map(|multi_buffer| {
            multi_buffer
                .buffer
                .read_with(cx, |buffer, _| buffer.all_buffers_iter().count())
        })
        .unwrap_or_default();
    let rename_preview_read_only =
        rename_preview_tab
            .multi_buffer
            .as_ref()
            .is_some_and(|multi_buffer| {
                multi_buffer
                    .buffer
                    .read_with(cx, |buffer, _| buffer.capability() == Capability::ReadOnly)
            });
    validate_pending_rename_guards(&rename_pending, cx)?;
    let (_, confirmation_request) = request_rename_workspace_edit(
        &services.project,
        &rename_pending.origin_buffer,
        rename_pending.origin_point,
        rename_pending.new_name.clone(),
        Some(rename_pending.language_server_id),
        cx,
    )?;
    let confirmation_edit = confirmation_request.await?;
    let confirmation_plan =
        normalize_rename_workspace_edit(&confirmation_edit, &rename_pending.workspace_root)?;
    ensure!(
        confirmation_plan.signature == rename_pending.plan.signature,
        "fixture rename changed between preview and acceptance"
    );
    validate_pending_rename_guards(&rename_pending, cx)?;
    let rename_transaction = services.project.update(cx, |project, cx| {
        project.perform_rename(
            rename_pending.origin_buffer.clone(),
            rename_pending.origin_point,
            rename_name.clone(),
            cx,
        )
    });
    let rename_transaction = rename_transaction
        .await
        .context("perform fixture multi-buffer rename")?;
    let rename_buffer_count = rename_transaction.0.len();
    rename_preview_tab
        .editor_window
        .update(cx, |_editor, window, _cx| window.remove_window())?;
    let rename_main_after = buffer.read_with(cx, |buffer, _| buffer.text());
    let rename_peer_after = peer_buffer
        .as_ref()
        .map(|(_, buffer)| buffer.read_with(cx, |buffer, _| buffer.text()));
    let mut rename_history = ProjectEditHistory::default();
    ensure!(
        rename_history.push(rename_transaction) == rename_buffer_count,
        "rename history buffer count changed"
    );
    let rename_undo_count = rename_history.undo_latest(cx)?;
    let rename_main_after_undo = buffer.read_with(cx, |buffer, _| buffer.text());
    let rename_peer_after_undo = peer_buffer
        .as_ref()
        .map(|(_, buffer)| buffer.read_with(cx, |buffer, _| buffer.text()));
    let rename_redo_count = rename_history.redo_latest(cx)?;
    let rename_main_after_redo = buffer.read_with(cx, |buffer, _| buffer.text());
    rename_history.undo_latest(cx)?;
    cx.background_executor()
        .timer(Duration::from_millis(25))
        .await;

    move_caret_to_text_position(
        &probe_tabs[0].editor_window,
        TextPosition {
            row: display_row,
            byte_column,
        },
        cx,
    )?;
    let code_action_generation = 31;
    let (code_action_buffer, code_action_buffer_id) = start_code_actions_request(
        &probe_tabs[0].editor_window,
        &services.project,
        code_action_generation,
        event_sender.clone(),
        cx,
    )?;
    let code_action_deadline = Instant::now() + Duration::from_secs(5);
    let code_actions = loop {
        match event_receiver.try_recv() {
            Ok(TerminalEvent::CodeActionsFinished {
                buffer_id,
                generation,
                result,
            }) if buffer_id == code_action_buffer_id && generation == code_action_generation => {
                break result.map_err(anyhow::Error::msg)?;
            }
            Ok(_) | Err(async_channel::TryRecvError::Empty) => {}
            Err(async_channel::TryRecvError::Closed) => {
                bail!("terminal code-action event channel closed")
            }
        }
        ensure!(
            Instant::now() < code_action_deadline,
            "terminal code actions were not published within 5 seconds"
        );
        cx.background_executor()
            .timer(Duration::from_millis(10))
            .await;
    };
    let mut code_action_prompt = CodeActionsPrompt::running(
        code_action_buffer.clone(),
        code_action_buffer_id,
        code_action_generation,
    );
    ensure!(code_action_prompt.complete(
        code_action_buffer_id,
        code_action_generation,
        Ok(code_actions)
    ));
    let code_action_overlay_rows = code_action_prompt
        .overlay()
        .rows
        .into_iter()
        .map(|row| row.text)
        .collect::<Vec<_>>();
    let code_action = code_action_prompt
        .selected_action()
        .context("fixture produced no enabled code action")?;
    let applied_code_action_title = code_action_title(&code_action).to_owned();
    let applied_code_action_kind = code_action_kind(&code_action);
    let applied_code_action_preferred = code_action_preferred(&code_action);
    let code_action_transaction = services.project.update(cx, |project, cx| {
        project.apply_code_action(code_action_buffer, code_action, true, cx)
    });
    let code_action_transaction = code_action_transaction
        .await
        .context("apply fixture code action")?;
    let code_action_buffer_count = code_action_transaction.0.len();
    let code_action_main_after = buffer.read_with(cx, |buffer, _| buffer.text());
    let mut code_action_history = ProjectEditHistory::default();
    code_action_history.push(code_action_transaction);
    let code_action_undo_count = code_action_history.undo_latest(cx)?;
    let code_action_main_after_undo = buffer.read_with(cx, |buffer, _| buffer.text());
    cx.background_executor()
        .timer(Duration::from_millis(25))
        .await;

    let format_document_transaction =
        format_active_editor(&probe_tabs[0].editor_window, &services.project, false, cx)?
            .await
            .context("format fixture document")?;
    let format_document_buffer_count = format_document_transaction.0.len();
    let format_document_after = buffer.read_with(cx, |buffer, _| buffer.text());
    let mut format_history = ProjectEditHistory::default();
    format_history.push(format_document_transaction);
    let format_document_undo_count = if format_history.can_undo() {
        format_history.undo_latest(cx)?
    } else {
        0
    };
    let format_document_after_undo = buffer.read_with(cx, |buffer, _| buffer.text());
    cx.background_executor()
        .timer(Duration::from_millis(25))
        .await;

    probe_tabs[0]
        .editor_window
        .update(cx, |editor, window, cx| {
            let display = editor.display_snapshot(cx);
            let start = display.display_point_to_anchor(
                display.clip_point(DisplayPoint::new(DisplayRow(0), 0), Bias::Left),
                Bias::Left,
            );
            let end = display.display_point_to_anchor(
                display.clip_point(DisplayPoint::new(DisplayRow(2), 0), Bias::Right),
                Bias::Right,
            );
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                selections.select_anchor_ranges([start..end])
            });
        })?;
    let format_range_transaction =
        format_active_editor(&probe_tabs[0].editor_window, &services.project, true, cx)?
            .await
            .context("format fixture selection")?;
    let format_range_buffer_count = format_range_transaction.0.len();
    let format_range_after = buffer.read_with(cx, |buffer, _| buffer.text());
    let mut format_range_history = ProjectEditHistory::default();
    format_range_history.push(format_range_transaction);
    let format_range_undo_count = if format_range_history.can_undo() {
        format_range_history.undo_latest(cx)?
    } else {
        0
    };
    let format_range_after_undo = buffer.read_with(cx, |buffer, _| buffer.text());

    let terminal_locations_json = terminal_location_results
        .iter()
        .map(|(kind, items, overlay_rows)| {
            let key = match kind {
                LocationRequestKind::Definition => "definitions",
                LocationRequestKind::TypeDefinition => "type_definitions",
                LocationRequestKind::References => "references",
                LocationRequestKind::ProjectSymbols => "project_symbols",
            };
            (
                key.to_owned(),
                serde_json::json!({
                    "items": items.iter().map(|item| serde_json::json!({
                        "path": item.path,
                        "label": item.label,
                        "row": item.row,
                        "column": item.column,
                        "end_row": item.end_row,
                        "end_column": item.end_column,
                        "snippet": item.snippet,
                    })).collect::<Vec<_>>(),
                    "overlay_rows": overlay_rows,
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();

    let report = serde_json::json!({
        "project_entity": format!("{:?}", services.project.entity_id()),
        "buffer_entity": format!("{:?}", buffer.entity_id()),
        "language": language,
        "servers": statuses,
        "completions": completions,
        "hover": hovers,
        "diagnostics": {
            "errors": diagnostic_summary.error_count,
            "warnings": diagnostic_summary.warning_count,
        },
        "terminal_completion": {
            "items": terminal_items
                .iter()
                .map(|item| serde_json::json!({
                    "label": item.label,
                    "detail": item.detail,
                    "kind": item.kind,
                    "documentation": item.documentation,
                }))
                .collect::<Vec<_>>(),
            "text_after_apply": text_after_completion,
            "text_after_undo": text_after_completion_undo,
        },
        "terminal_hover": {
            "items": terminal_hover_items
                .iter()
                .map(|item| serde_json::json!({
                    "kind": item.kind,
                    "text": item.text,
                }))
                .collect::<Vec<_>>(),
            "overlay_rows": terminal_hover_overlay
                .rows
                .iter()
                .map(|row| row.text.clone())
                .collect::<Vec<_>>(),
        },
        "terminal_diagnostics": {
            "items": terminal_diagnostic_items
                .iter()
                .map(|item| serde_json::json!({
                    "path": item.path,
                    "label": item.label,
                    "row": item.row,
                    "column": item.column,
                    "severity": item.severity,
                    "message": item.message,
                    "source": item.source,
                }))
                .collect::<Vec<_>>(),
            "overlay_rows": terminal_diagnostics_overlay
                .rows
                .iter()
                .map(|row| row.text.clone())
                .collect::<Vec<_>>(),
            "navigation": diagnostic_navigation,
            "cursor": {
                "row": diagnostic_point.row,
                "column": diagnostic_point.column,
            },
        },
        "terminal_locations": terminal_locations_json,
        "terminal_multibuffer": {
            "title": "References",
            "target_count": reference_items.len(),
            "source_count": multibuffer_source_count,
            "snapshot_contains_main": multibuffer_text.contains("fn main"),
            "snapshot_contains_peer": multibuffer_text.contains("fixture_peer"),
            "changed_path": multibuffer_changed_label,
            "source_before": multibuffer_before.get(&changed_multibuffer_path),
            "source_after_edit": multibuffer_after_edit.get(&changed_multibuffer_path),
            "disk_after_save": multibuffer_disk_after_save,
            "source_after_undo": multibuffer_after_undo.get(&changed_multibuffer_path),
            "disk_after_restore": multibuffer_disk_after_restore,
        },
        "semantic_navigation": {
            "message": semantic_navigation,
            "cursor": {
                "row": semantic_point.row,
                "column": semantic_point.column,
            },
        },
        "terminal_edits": {
            "rename": {
                "preparation": {
                    "placeholder": rename_preparation.placeholder,
                    "start": rename_preparation.start,
                    "end": rename_preparation.end,
                },
                "overlay_rows": rename_overlay_rows,
                "preview": {
                    "read_only": rename_preview_read_only,
                    "source_count": rename_preview_source_count,
                    "buffer_count": rename_preview_buffer_count,
                    "edit_count": rename_pending.plan.edit_count,
                    "file_operation_count": rename_pending.plan.file_operation_count,
                    "signature": rename_pending.plan.signature,
                    "confirmation_signature_matches": confirmation_plan.signature
                        == rename_pending.plan.signature,
                    "contains_main": rename_preview_text.contains("src/main.rs"),
                    "contains_peer": rename_preview_text.contains("src/lib.rs"),
                    "contains_old_text": rename_preview_text.contains("stub_"),
                    "contains_new_text": rename_preview_text.contains("renamed_fixture"),
                    "rejected_contains_new_text": rejected_preview_text.contains("renamed_fixture"),
                    "rejection_unchanged": rename_rejection_unchanged,
                },
                "buffer_count": rename_buffer_count,
                "undo_buffer_count": rename_undo_count,
                "redo_buffer_count": rename_redo_count,
                "main_after": rename_main_after,
                "peer_after": rename_peer_after,
                "main_after_undo": rename_main_after_undo,
                "peer_after_undo": rename_peer_after_undo,
                "main_after_redo": rename_main_after_redo,
            },
            "code_action": {
                "title": applied_code_action_title,
                "kind": applied_code_action_kind,
                "preferred": applied_code_action_preferred,
                "overlay_rows": code_action_overlay_rows,
                "buffer_count": code_action_buffer_count,
                "undo_buffer_count": code_action_undo_count,
                "main_after": code_action_main_after,
                "main_after_undo": code_action_main_after_undo,
            },
            "format_document": {
                "buffer_count": format_document_buffer_count,
                "undo_buffer_count": format_document_undo_count,
                "after": format_document_after,
                "after_undo": format_document_after_undo,
            },
            "format_range": {
                "buffer_count": format_range_buffer_count,
                "undo_buffer_count": format_range_undo_count,
                "after": format_range_after,
                "after_undo": format_range_after_undo,
            },
        },
    });

    // Releasing the project-backed editor releases Zed's OpenLspBufferHandle.
    // A project-less keepalive window prevents GPUI from terminating when the
    // probe closes its only real editor, so didClose can reach the server before
    // the outer runner initiates shutdown.
    let _keepalive_window = cx.update(|cx| open_editor(buffer.clone(), cx))?;
    for tab in &probe_tabs {
        tab.editor_window
            .update(cx, |_editor, window, _cx| window.remove_window())?;
    }
    cx.background_executor()
        .timer(Duration::from_millis(25))
        .await;

    Ok(report)
}

async fn execute_repository_probe(
    probe: RepositoryProbe,
    services: &FileServices,
    cx: &mut gpui::AsyncApp,
) -> Result<Value> {
    match probe {
        RepositoryProbe::RootIdentity { root, inputs } => {
            root_identity_probe(&root, &inputs, services, cx).await
        }
        RepositoryProbe::OutsideTrace(path) => outside_trace_probe(&path, cx).await,
        RepositoryProbe::ProjectSearch { root, query } => {
            let repository = prepare_repository(&root, None, services, cx).await?;
            let output =
                collect_project_search(&repository, query, services, Vec::new(), cx).await?;
            Ok(project_search_json(&output))
        }
        RepositoryProbe::StaleResult(root) => stale_result_probe(&root, services, cx).await,
        RepositoryProbe::SearchFailure(root) => search_failure_probe(&root, services, cx).await,
    }
}

async fn collect_project_search(
    repository: &RepositorySession,
    query: String,
    services: &FileServices,
    open_buffers: Vec<Entity<Buffer>>,
    cx: &mut gpui::AsyncApp,
) -> Result<ProjectSearchOutput> {
    let mut scheduler = ProjectSearchScheduler::default();
    let session = scheduler.open_session()?;
    let mut prompt = ProjectSearchPrompt::new(session);
    let requested = prompt.request(query)?;
    let request = scheduler
        .request(requested, ProjectSearchChange::Paste)?
        .next
        .context("project search did not start")?;
    let command =
        start_zed_project_search_command(request, repository, services, open_buffers, cx)?;
    let output = command.completion.await.map_err(anyhow::Error::msg)?;
    let finished = scheduler.finish(command.request);
    ensure!(finished.was_active, "completed search had no active slot");
    ensure!(
        complete_project_search(&mut prompt, command.request, Ok(output.clone()))
            == CompletionDisposition::Published,
        "completed project search was unexpectedly stale"
    );
    Ok(output)
}

async fn root_identity_probe(
    root_path: &Path,
    root_inputs: &[RepositoryRootInput],
    services: &FileServices,
    cx: &mut gpui::AsyncApp,
) -> Result<Value> {
    let canonical_root = services
        .file_system
        .canonicalize(root_path)
        .await
        .with_context(|| format!("canonicalize probe root {}", root_path.display()))?;
    ensure!(
        !root_inputs.is_empty(),
        "root-identity probe requires at least one root input"
    );
    let mut root_identities = HashSet::new();
    let mut worktree_identities = HashSet::new();
    let mut root_input_results = Vec::with_capacity(root_inputs.len());
    let mut repositories = Vec::new();
    let mut directory_opened_as_file = false;
    for input in root_inputs {
        let arguments = input.argument.clone().into_iter().collect();
        let (paths, implicit_root) = resolve_startup_invocation(&input.cwd, arguments)?;
        ensure!(
            paths.len() == 1,
            "root input {} resolved to {} paths",
            input.id,
            paths.len()
        );
        let resolved_path = services
            .file_system
            .canonicalize(&paths[0])
            .await
            .with_context(|| format!("canonicalize root input {}", input.id))?;
        let mut startup = prepare_startup(paths, implicit_root, None, services, cx).await?;
        ensure!(
            startup.errors.is_empty(),
            "root spelling startup errors: {:?}",
            startup.errors
        );
        directory_opened_as_file |= startup.documents.iter().any(|document| {
            document_state(document, cx).path.as_deref() == Some(canonical_root.as_path())
        });
        let repository = startup
            .repository
            .take()
            .context("root spelling did not produce repository state")?;
        let repository_root = repository.root.canonical_path();
        ensure!(
            resolved_path == canonical_root && repository_root == canonical_root,
            "root input {} escaped canonical repository {}",
            input.id,
            canonical_root.display()
        );
        let worktree_id = repository
            .worktree
            .read_with(cx, |worktree, _| worktree.id().to_proto())
            .to_string();
        root_identities.insert(repository.root.clone());
        worktree_identities.insert(worktree_id.clone());
        root_input_results.push(serde_json::json!({
            "id": input.id.as_str(),
            "cwd": input.cwd.display().to_string(),
            "argument": input.argument.as_ref().map(|path| path.display().to_string()),
            "resolved_path": resolved_path.display().to_string(),
            "repository_root": repository_root.display().to_string(),
            "worktree_id": worktree_id,
        }));
        repositories.push(repository);
    }
    ensure!(
        repositories.len() == root_inputs.len(),
        "not all specified root inputs reached production startup"
    );
    ensure!(
        !directory_opened_as_file,
        "a repository directory was opened as a file"
    );
    ensure!(
        worktree_identities.len() == 1,
        "root inputs produced more than one Zed worktree identity"
    );
    let repository = repositories.swap_remove(0);

    let absolute_alias = canonical_root.join("src/日本 語.rs");
    let alias_paths = [
        ("src/日本 語.rs".to_owned(), PathBuf::from("src/日本 語.rs")),
        (
            "./src/日本 語.rs".to_owned(),
            PathBuf::from("./src/日本 語.rs"),
        ),
        (
            "src/../src/日本 語.rs".to_owned(),
            PathBuf::from("src/../src/日本 語.rs"),
        ),
        (
            "aliases/日本 語.rs".to_owned(),
            PathBuf::from("aliases/日本 語.rs"),
        ),
        (absolute_alias.display().to_string(), absolute_alias.clone()),
    ];
    let (redraw_sender, _redraw_receiver) = async_channel::bounded(64);
    let mut tabs = Vec::<DocumentTab>::new();
    let mut aliases = Vec::new();
    for (label, path) in alias_paths {
        let document = open_repository_document(&path, &repository, services, cx).await?;
        let buffer_id = document
            .buffer
            .read_with(cx, |buffer, _| buffer.remote_id().to_proto())
            .to_string();
        let tab_index = if let Some(index) = tabs
            .iter()
            .position(|tab| tab.document.buffer == document.buffer)
        {
            index
        } else {
            tabs.push(create_document_tab(
                document,
                services,
                redraw_sender.clone(),
                cx,
            )?);
            tabs.len() - 1
        };
        let tab_handle: AnyWindowHandle = tabs[tab_index].editor_window.into();
        let tab_id = format!("{:?}", tab_handle.window_id());
        aliases.push(serde_json::json!({
            "path": label,
            "buffer_id": buffer_id,
            "tab_id": tab_id,
        }));
    }

    let outside = canonical_root
        .parent()
        .context("repository root has no parent")?
        .join("outside-control.txt");
    let exclusion_queries = [
        ".git/repository-e2e-excluded.txt".to_owned(),
        "ignored/excluded.txt".to_owned(),
        "target/excluded.txt".to_owned(),
        outside.display().to_string(),
    ];
    let quick_open_excluded_results = exclusion_queries
        .iter()
        .map(|query| {
            let results = repository
                .index
                .quick_open(query, QUICK_OPEN_LIMIT)
                .into_iter()
                .filter_map(|matched| repository.index.file(matched.file_index()))
                .map(|file| file.relative_path().to_owned())
                .collect::<Vec<_>>();
            serde_json::json!({"query": query, "results": results})
        })
        .collect::<Vec<_>>();

    let excluded_search = collect_project_search(
        &repository,
        "E2E_EXCLUDED_SENTINEL".to_owned(),
        services,
        project_searchable_buffers(&tabs),
        cx,
    )
    .await?;
    let project_search_excluded_results =
        project_search_json(&excluded_search)["visible_results"].clone();

    let worktree_root_count = services.worktree_store.read_with(cx, |store, cx| {
        store
            .worktrees()
            .filter(|worktree| worktree.read(cx).abs_path().starts_with(&canonical_root))
            .count()
    });
    ensure!(
        worktree_root_count == worktree_identities.len(),
        "root subtree contains an unreported duplicate worktree"
    );
    Ok(serde_json::json!({
        "repository_root_count": root_identities.len(),
        "worktree_root_count": worktree_identities.len(),
        "root_inputs": root_input_results,
        "directory_opened_as_file": directory_opened_as_file,
        "aliases": aliases,
        "quick_open_excluded_results": quick_open_excluded_results,
        "project_search_excluded_results": project_search_excluded_results,
    }))
}

async fn outside_trace_probe(path: &Path, cx: &mut gpui::AsyncApp) -> Result<Value> {
    let absolute =
        std::path::absolute(path).with_context(|| format!("make {} absolute", path.display()))?;
    let canonical = std::fs::canonicalize(&absolute)
        .with_context(|| format!("resolve controlled outside file {}", absolute.display()))?;
    let expected_bytes = std::fs::read(&canonical)
        .with_context(|| format!("read controlled outside file {}", canonical.display()))?;

    let (recording, services) = cx.update(|cx| {
        let real: Arc<dyn Fs> = Arc::new(RealFs::new(None, cx.background_executor().clone()));
        let recording = ZecFs::isolated_recording(real);
        let services = file_services_with_fs(cx, recording.clone(), false);
        (recording, services)
    });
    recording.clear();

    let document = open_single_file_document(&canonical, &services, cx).await?;
    let (worktree, relative_path) = services
        .worktree_store
        .read_with(cx, |store, cx| store.find_worktree(&canonical, cx))
        .context("outside probe single-file worktree is missing")?;
    ensure!(
        relative_path.as_unix_str().is_empty(),
        "outside file was nested under a parent worktree"
    );
    let (worktree_root, is_single_file, scan_complete) = worktree.read_with(cx, |worktree, _| {
        (
            worktree.abs_path(),
            worktree.is_single_file(),
            worktree.as_local().map(|local| local.scan_complete()),
        )
    });
    ensure!(
        worktree_root.as_ref() == canonical,
        "outside worktree root {} differs from file {}",
        worktree_root.display(),
        canonical.display()
    );
    ensure!(is_single_file, "outside worktree is not single-file");
    scan_complete
        .context("outside worktree must be local")?
        .await;

    let opened_path = document_state(&document, cx)
        .path
        .context("outside probe buffer has no file")?;
    ensure!(
        opened_path == canonical,
        "outside probe buffer path {} differs from {}",
        opened_path.display(),
        canonical.display()
    );
    let opened_bytes = document
        .buffer
        .read_with(cx, |buffer, _| buffer.text().to_string().into_bytes());
    ensure!(
        opened_bytes == expected_bytes,
        "outside probe buffer bytes differ"
    );

    let accesses = recording.accesses();
    ensure!(!accesses.is_empty(), "outside filesystem trace is empty");
    let parent = canonical.parent();
    let read_dir_accesses = accesses
        .iter()
        .filter(|access| access.kind == FsPathKind::ReadDir)
        .collect::<Vec<_>>();
    let outside_parent_read_dir_count = read_dir_accesses
        .iter()
        .filter(|access| Some(access.path.as_path()) == parent)
        .count();
    let outside_sibling_read_dir_count = read_dir_accesses
        .iter()
        .filter(|access| Some(access.path.as_path()) != parent)
        .count();
    ensure!(
        read_dir_accesses.is_empty(),
        "single-file worktree issued read_dir at {:?}",
        read_dir_accesses
            .iter()
            .map(|access| access.path.as_path())
            .collect::<Vec<_>>()
    );
    let operations = classify_single_file_accesses(&accesses, &canonical)?;
    ensure!(
        operations.contains(&"open-self") && operations.contains(&"stat-self"),
        "outside trace must observe both open and stat: {operations:?}"
    );

    Ok(serde_json::json!({
        "opened_path": opened_path.display().to_string(),
        "outside_parent_read_dir_count": outside_parent_read_dir_count,
        "outside_sibling_read_dir_count": outside_sibling_read_dir_count,
        "operations": operations,
    }))
}

async fn stale_result_probe(
    root: &Path,
    services: &FileServices,
    cx: &mut gpui::AsyncApp,
) -> Result<Value> {
    let repository = prepare_repository(root, None, services, cx).await?;
    let mut scheduler = ProjectSearchScheduler::default();
    let session = scheduler.open_session()?;
    let mut prompt = ProjectSearchPrompt::new(session);

    let request_a = scheduler
        .request(
            prompt.request("E2E_STALE_A".to_owned())?,
            ProjectSearchChange::Paste,
        )?
        .next
        .context("query A did not start")?;
    let command_a =
        start_zed_project_search_command(request_a, &repository, services, Vec::new(), cx)?;
    let request_b = scheduler
        .request(
            prompt.request("E2E_STALE_B".to_owned())?,
            ProjectSearchChange::Paste,
        )?
        .next
        .context("query B did not start")?;
    let command_b =
        start_zed_project_search_command(request_b, &repository, services, Vec::new(), cx)?;

    let mut publish_log = Vec::new();
    let key_b = command_b.request;
    let output_b = command_b.completion.await.map_err(anyhow::Error::msg)?;
    ensure!(scheduler.finish(key_b).was_active, "query B lost its slot");
    if complete_project_search(&mut prompt, key_b, Ok(output_b)) == CompletionDisposition::Published
    {
        publish_log.push("B");
    }
    let key_a = command_a.request;
    let output_a = command_a.completion.await.map_err(anyhow::Error::msg)?;
    ensure!(scheduler.finish(key_a).was_active, "query A lost its slot");
    if complete_project_search(&mut prompt, key_a, Ok(output_a)) == CompletionDisposition::Published
    {
        publish_log.push("A");
    }

    let (final_query, final_path) = match prompt.reducer.state() {
        LatestSearchState::Ready { query, result, .. } => (
            query.clone(),
            result
                .matches
                .first()
                .context("query B produced no result")?
                .summary
                .path
                .to_string(),
        ),
        state => bail!("unexpected final stale-result state: {state:?}"),
    };
    Ok(serde_json::json!({
        "publish_log": publish_log,
        "final_query": final_query,
        "final_path": final_path,
    }))
}

fn probe_document_trace(document: &OpenDocument, tab_count: usize, cx: &gpui::AsyncApp) -> Value {
    let body = document
        .buffer
        .read_with(cx, |buffer, _| buffer.text().to_string());
    let state = document_state(document, cx);
    serde_json::json!({
        "tab_count": tab_count,
        "body_sha256": format!("{:x}", Sha256::digest(body.as_bytes())),
        "dirty": state.dirty,
    })
}

async fn search_failure_probe(
    root: &Path,
    services: &FileServices,
    cx: &mut gpui::AsyncApp,
) -> Result<Value> {
    let repository = prepare_repository(root, None, services, cx).await?;
    let control_file = repository
        .index
        .file_for_alias("README.md")
        .context("search-failure repository has no README.md")?;
    let project_path = control_file
        .project_path()
        .cloned()
        .context("README.md has no project path")?;
    let document =
        load_project_document(project_path, control_file.canonical_path(), services, cx).await?;
    let (redraw_sender, _redraw_receiver) = async_channel::bounded(64);
    let tabs = vec![create_document_tab(document, services, redraw_sender, cx)?];
    let before = probe_document_trace(&tabs[0].document, tabs.len(), cx);

    let mut scheduler = ProjectSearchScheduler::default();
    let session = scheduler.open_session()?;
    let mut prompt = ProjectSearchPrompt::new(session);
    let (provider_sender, provider_receiver) =
        async_channel::bounded::<std::result::Result<ProjectSearchOutput, String>>(1);
    let request = scheduler
        .request(
            prompt.request("E2E_SEARCH_FAILURE".to_owned())?,
            ProjectSearchChange::Paste,
        )?
        .next
        .context("controlled failing search did not start")?;
    let command = start_project_search_command_with(request, cx, move |_query, cx| {
        Ok(cx.spawn(async move |_cx| {
            provider_receiver
                .recv()
                .await
                .unwrap_or_else(|_| Err("controlled provider disconnected".to_owned()))
        }))
    })?;
    provider_sender
        .send(Err("EIO".to_owned()))
        .await
        .context("send controlled EIO")?;
    let key = command.request;
    let completion = command.completion.await;
    ensure!(
        scheduler.finish(key).was_active,
        "failed search lost its slot"
    );
    ensure!(
        complete_project_search(&mut prompt, key, completion) == CompletionDisposition::Published,
        "controlled EIO completion was unexpectedly stale"
    );
    let error = match prompt.reducer.state() {
        LatestSearchState::Failed { error, .. } => error.clone(),
        state => bail!("controlled EIO did not reach failed state: {state:?}"),
    };
    let rendered_status = tabs[0].editor_window.update(cx, |editor, window, cx| {
        capture_editor(
            editor,
            window,
            cx,
            Viewport::default(),
            false,
            None,
            Rect::new(0, 0, 120, 40),
            "repo",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(&prompt),
            None,
            None,
            None,
            None,
        )
        .snapshot
        .status
    })?;
    ensure!(
        rendered_status.contains("search failed: EIO"),
        "controlled EIO was not rendered in project-search status"
    );
    let after = probe_document_trace(&tabs[0].document, tabs.len(), cx);
    ensure!(before == after, "search failure mutated editor state");

    tabs[0].editor_window.update(cx, |editor, window, cx| {
        editor.select_all(&SelectAll, window, cx);
        editor.insert("E2E_SEARCH_FAILURE_CONTINUED", window, cx);
    })?;
    save_document(&tabs[0].document, services, cx).await?;
    let control_disk_token = services
        .file_system
        .load(control_file.canonical_path())
        .await?;
    let continued_edit_saved = control_disk_token == "E2E_SEARCH_FAILURE_CONTINUED"
        && !document_state(&tabs[0].document, cx).dirty;

    Ok(serde_json::json!({
        "error": error,
        "before": before,
        "after": after,
        "continued_edit_saved": continued_edit_saved,
        "control_disk_token": control_disk_token,
    }))
}
