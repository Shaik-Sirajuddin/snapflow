use i_slint_backend_testing::ElementHandle;
use panel_rust::{
    AgentCatalogEntry, ChatPanel, DropdownEntry, LocalTerminalItem, McpServerOption,
    MessageItem, PendingRequestItem, ProfileOption, RemoteSessionOption,
    TerminalItem, ThreadItem,
};
use slint::platform::{Key, WindowEvent};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

#[test]
fn primary_chat_controls_are_addressable_and_invoke_their_callbacks() {
    i_slint_backend_testing::init_no_event_loop();

    let panel = ChatPanel::new().expect("construct chat panel");
    let new_thread_count = Rc::new(Cell::new(0));
    let settings_count = Rc::new(Cell::new(0));
    let settings_save_count = Rc::new(Cell::new(0));
    let settings_close_count = Rc::new(Cell::new(0));
    let sent_text = Rc::new(Cell::new(String::new()));
    let approval_count = Rc::new(Cell::new(0));
    let rejection_count = Rc::new(Cell::new(0));
    let load_older_count = Rc::new(Cell::new(0));
    let expanded_terminal = Rc::new(Cell::new(String::new()));
    let terminal_overlay_close_count = Rc::new(Cell::new(0));
    let closed_local_terminal_count = Rc::new(Cell::new(0));

    {
        let new_thread_count = new_thread_count.clone();
        panel.on_new_thread_requested(move || new_thread_count.set(new_thread_count.get() + 1));
    }
    {
        let settings_count = settings_count.clone();
        panel.on_settings_requested(move || settings_count.set(settings_count.get() + 1));
    }
    {
        let settings_save_count = settings_save_count.clone();
        panel.on_settings_save(move || settings_save_count.set(settings_save_count.get() + 1));
    }
    {
        let settings_close_count = settings_close_count.clone();
        panel.on_settings_close(move || settings_close_count.set(settings_close_count.get() + 1));
    }
    {
        let sent_text = sent_text.clone();
        let panel_weak = panel.as_weak();
        panel.on_send_requested(move || {
            let panel = panel_weak.upgrade().expect("panel alive during callback");
            sent_text.set(panel.get_compose_text().to_string());
        });
    }
    {
        let approval_count = approval_count.clone();
        panel.on_approve_request(move || approval_count.set(approval_count.get() + 1));
    }
    {
        let rejection_count = rejection_count.clone();
        panel.on_reject_request(move || rejection_count.set(rejection_count.get() + 1));
    }
    {
        let load_older_count = load_older_count.clone();
        let panel_weak = panel.as_weak();
        panel.on_load_older_requested(move || {
            load_older_count.set(load_older_count.get() + 1);
            panel_weak
                .upgrade()
                .expect("panel alive during page callback")
                .set_loading_older_messages(false);
        });
    }
    {
        let expanded_terminal = expanded_terminal.clone();
        panel.on_expand_terminal(move |id| expanded_terminal.set(id.to_string()));
    }
    {
        let terminal_overlay_close_count = terminal_overlay_close_count.clone();
        panel.on_close_terminal_overlay(move || {
            terminal_overlay_close_count.set(terminal_overlay_close_count.get() + 1)
        });
    }
    {
        let closed_local_terminal_count = closed_local_terminal_count.clone();
        panel.on_local_terminal_close_requested(move || {
            closed_local_terminal_count.set(closed_local_terminal_count.get() + 1)
        });
    }

    let expand_sidebar =
        ElementHandle::find_by_accessible_label(&panel, "Expand thread sidebar")
            .next()
            .expect("sidebar expansion control must be accessible");
    assert_eq!(
        expand_sidebar.id().as_deref(),
        Some("Sidebar::sidebar-toggle")
    );
    expand_sidebar.invoke_accessible_default_action();
    assert!(panel.get_sidebar_expanded());

    let new_thread = ElementHandle::find_by_accessible_label(&panel, "New thread")
        .next()
        .expect("new-thread control must be accessible");
    assert_eq!(new_thread.id().as_deref(), Some("Sidebar::new-thread-touch"));
    new_thread.invoke_accessible_default_action();
    assert_eq!(new_thread_count.get(), 1);

    let settings = ElementHandle::find_by_accessible_label(&panel, "Open chat settings")
        .next()
        .expect("settings control must be accessible");
    assert_eq!(settings.id().as_deref(), Some("ChatArea::settings-button"));
    settings.invoke_accessible_default_action();
    assert_eq!(settings_count.get(), 1);

    panel.set_settings_open(true);
    // The "Recoverable Sessions" section (settings_sheet.slint) adds
    // its own always-present heading regardless of list content, same
    // "unconditional section header" convention the pre-existing
    // "Agents" heading already established -- this pushed the sheet's
    // total content height just past this headless window's small
    // default size, hiding `save-settings-button` below the Flickable's
    // clipped/laid-out region (see `settings_and_capability_controls_
    // are_addressable_and_dispatch_typed_values`'s own comment on this
    // exact Slint-testing gotcha). Same fix: grow the window before
    // looking up anything inside the sheet.
    panel.window().set_size(slint::LogicalSize::new(900.0, 1000.0));
    let save_settings = ElementHandle::find_by_accessible_label(&panel, "Save chat settings")
        .next()
        .expect("settings save control must be accessible");
    assert_eq!(
        save_settings.id().as_deref(),
        Some("SettingsSheet::save-settings-button")
    );
    save_settings.invoke_accessible_default_action();
    assert_eq!(settings_save_count.get(), 1);

    let close_settings = ElementHandle::find_by_accessible_label(&panel, "Close chat settings")
        .next()
        .expect("settings close control must be accessible");
    close_settings.invoke_accessible_default_action();
    assert_eq!(settings_close_count.get(), 1);

    panel.set_settings_open(true);
    assert!(!panel.get_background_default());
    let background_default =
        ElementHandle::find_by_accessible_label(&panel, "Toggle background session default")
            .next()
            .expect("background default control must be accessible");
    assert_eq!(
        background_default.id().as_deref(),
        Some("SettingsSheet::background-default-toggle")
    );
    background_default.invoke_accessible_default_action();
    assert!(panel.get_background_default());

    let background_override =
        ElementHandle::find_by_accessible_label(&panel, "Toggle background session override")
            .next()
            .expect("background override control must be accessible");
    assert_eq!(
        background_override.id().as_deref(),
        Some("SettingsSheet::background-override-toggle")
    );
    background_override.invoke_accessible_default_action();
    assert!(panel.get_background_override_set());
    assert!(panel.get_background_override());

    let background_override_value = ElementHandle::find_by_accessible_label(
        &panel,
        "Toggle selected chat background session",
    )
    .next()
    .expect("background override value control must be accessible");
    assert_eq!(
        background_override_value.id().as_deref(),
        Some("SettingsSheet::background-override-value-toggle")
    );
    background_override_value.invoke_accessible_default_action();
    assert!(!panel.get_background_override());
    panel.set_settings_open(false);

    panel.set_compose_text("render a title card".into());
    let send = ElementHandle::find_by_accessible_label(&panel, "Send message")
        .next()
        .expect("send control must be accessible");
    assert_eq!(send.id().as_deref(), Some("ChatArea::send-stop-button"));
    send.invoke_accessible_default_action();
    assert_eq!(sent_text.take(), "render a title card");

    let compose = ElementHandle::find_by_accessible_label(&panel, "Compose message")
        .next()
        .expect("compose input must be accessible");
    assert_eq!(compose.id().as_deref(), Some("ChatArea::compose"));
    compose.invoke_accessible_default_action();
    assert!(panel.get_compose_has_focus(), "composer should accept focus");

    // A streamed transcript projection changes the message model beneath the
    // composer. It must never focus the new message/card and interrupt input.
    panel.set_messages(ModelRc::new(VecModel::from(vec![MessageItem {
        kind: "agent".into(),
        text: "streamed response".into(),
        status: "streaming".into(),
        expanded: false,
        index: 0,
        raw_input: "".into(),
        raw_output: "".into(),
        queued: false,
        can_edit: false,
        sending: false,
    }])));
    assert!(
        panel.get_compose_has_focus(),
        "streamed message updates must not steal composer focus"
    );

    panel.set_has_older_messages(true);
    let load_older = ElementHandle::find_by_accessible_label(&panel, "Load older messages")
        .next()
        .expect("older-page control must be accessible");
    assert_eq!(
        load_older.id().as_deref(),
        Some("ChatArea::load-older-button")
    );
    load_older.invoke_accessible_default_action();
    assert_eq!(load_older_count.get(), 1);
    assert!(
        !panel.get_loading_older_messages(),
        "page-load guard must reset after its Rust callback completes"
    );

    panel.set_pending_request(PendingRequestItem {
        active: true,
        method: "terminal/create".into(),
        relay_id: "relay-1".into(),
        summary: "Run a render command".into(),
        supported: true,
        title: "Terminal request".into(),
    });
    let approve = ElementHandle::find_by_accessible_label(&panel, "Approve request")
        .next()
        .expect("approve control must be accessible");
    assert_eq!(
        approve.id().as_deref(),
        Some("PermissionCard::approve-request-button")
    );
    approve.invoke_accessible_default_action();
    assert_eq!(approval_count.get(), 1);

    let reject = ElementHandle::find_by_accessible_label(&panel, "Reject request")
        .next()
        .expect("reject control must be accessible");
    reject.invoke_accessible_default_action();
    assert_eq!(rejection_count.get(), 1);

    panel.set_terminals(ModelRc::new(VecModel::from(vec![TerminalItem {
        terminal_id: "build-42".into(),
        output: "building\n".into(),
        truncated: false,
        has_exited: false,
        exit_code: 0,
    }])));
    let expand_terminal = ElementHandle::find_by_accessible_label(&panel, "Expand terminal build-42")
        .next()
        .expect("terminal expand control must be accessible");
    assert_eq!(expand_terminal.id().as_deref(), Some("TerminalCard::terminal-expand"));
    expand_terminal.invoke_accessible_default_action();
    assert_eq!(expanded_terminal.take(), "build-42");

    panel.set_expanded_terminal(TerminalItem {
        terminal_id: "build-42".into(),
        output: "building\n".into(),
        truncated: false,
        has_exited: false,
        exit_code: 0,
    });
    let close_terminal_overlay =
        ElementHandle::find_by_accessible_label(&panel, "Close terminal overlay")
            .next()
            .expect("terminal overlay close control must be accessible");
    assert_eq!(
        close_terminal_overlay.id().as_deref(),
        Some("TerminalOverlay::terminal-overlay-close")
    );
    close_terminal_overlay.invoke_accessible_default_action();
    assert_eq!(terminal_overlay_close_count.get(), 1);
    assert!(
        panel.get_compose_has_focus(),
        "closing an agent terminal overlay must restore composer focus"
    );

    panel.set_local_terminal(LocalTerminalItem {
        open: true,
        screen_text: "$ ".into(),
        cols: 80,
        rows: 24,
        cursor_row: 0,
        cursor_col: 2,
        has_exited: false,
    });
    let expand_local_terminal =
        ElementHandle::find_by_accessible_label(&panel, "Expand local terminal")
            .next()
            .expect("local terminal expand control must be accessible");
    assert_eq!(
        expand_local_terminal.id().as_deref(),
        Some("LocalTerminalCard::local-terminal-expand")
    );
    expand_local_terminal.invoke_accessible_default_action();
    assert!(panel.get_local_terminal_overlay_open());
    let close_local_overlay =
        ElementHandle::find_by_accessible_label(&panel, "Close local terminal overlay")
            .next()
            .expect("local terminal overlay close control must be accessible");
    assert_eq!(
        close_local_overlay.id().as_deref(),
        Some("LocalTerminalOverlay::local-terminal-overlay-close")
    );
    close_local_overlay.invoke_accessible_default_action();
    assert!(!panel.get_local_terminal_overlay_open());
    assert!(
        panel.get_compose_has_focus(),
        "closing a local terminal overlay must restore composer focus"
    );

    let close_local_terminal =
        ElementHandle::find_by_accessible_label(&panel, "Close local terminal")
            .next()
            .expect("local terminal close control must be accessible");
    assert_eq!(
        close_local_terminal.id().as_deref(),
        Some("LocalTerminalCard::local-terminal-kill")
    );
    close_local_terminal.invoke_accessible_default_action();
    assert_eq!(closed_local_terminal_count.get(), 1);
}

#[test]
fn local_terminal_focus_receives_keyboard_input_without_stealing_the_composer() {
    i_slint_backend_testing::init_no_event_loop();

    let panel = ChatPanel::new().expect("construct chat panel");
    let terminal_keys = Rc::new(RefCell::new(Vec::<String>::new()));
    {
        let terminal_keys = terminal_keys.clone();
        panel.on_local_terminal_key_input(move |text| {
            terminal_keys.borrow_mut().push(text.to_string());
        });
    }

    panel.set_local_terminal(LocalTerminalItem {
        open: true,
        screen_text: "$ ".into(),
        cols: 80,
        rows: 24,
        cursor_row: 0,
        cursor_col: 2,
        has_exited: false,
    });

    let terminal_focus = ElementHandle::find_by_accessible_label(&panel, "Focus local terminal")
        .next()
        .expect("local terminal focus action must be accessible");
    terminal_focus.invoke_accessible_default_action();
    assert!(
        panel.get_local_terminal_has_focus(),
        "the local terminal must become the active Slint keyboard target"
    );
    assert!(
        !panel.get_compose_has_focus(),
        "terminal focus must replace, rather than compete with, composer focus"
    );

    panel
        .window()
        .dispatch_event(WindowEvent::KeyPressed { text: "x".into() });
    panel.window().dispatch_event(WindowEvent::KeyPressed {
        text: SharedString::from(Key::Return),
    });
    panel.window().dispatch_event(WindowEvent::KeyPressed {
        text: SharedString::from(Key::LeftArrow),
    });

    let terminal_keys = terminal_keys.borrow();
    assert_eq!(terminal_keys[0], "x");
    assert_eq!(terminal_keys[1], "\n");
    assert_eq!(
        terminal_keys[2],
        SharedString::from(Key::LeftArrow).to_string(),
        "arrow keys must reach the local terminal instead of being treated as host navigation"
    );
}

#[test]
fn settings_and_capability_controls_are_addressable_and_dispatch_typed_values() {
    i_slint_backend_testing::init_no_event_loop();

    let panel = ChatPanel::new().expect("construct chat panel");
    let mode_selection = Rc::new(RefCell::new(Vec::<String>::new()));
    let config_selection = Rc::new(RefCell::new(Vec::<(String, String)>::new()));
    let created_mcp = Rc::new(RefCell::new(Vec::<(String, String)>::new()));
    let removed_mcp = Rc::new(RefCell::new(Vec::<String>::new()));
    let installed_agents = Rc::new(RefCell::new(Vec::<String>::new()));

    {
        let mode_selection = mode_selection.clone();
        panel.on_mode_selected(move |mode_id| mode_selection.borrow_mut().push(mode_id.to_string()));
    }
    {
        let config_selection = config_selection.clone();
        panel.on_config_option_selected(move |option_id, value| {
            config_selection
                .borrow_mut()
                .push((option_id.to_string(), value.to_string()));
        });
    }
    {
        let created_mcp = created_mcp.clone();
        panel.on_mcp_server_create(move |name, command| {
            created_mcp
                .borrow_mut()
                .push((name.to_string(), command.to_string()));
        });
    }
    {
        let removed_mcp = removed_mcp.clone();
        panel.on_mcp_server_delete(move |name| removed_mcp.borrow_mut().push(name.to_string()));
    }
    {
        let installed_agents = installed_agents.clone();
        panel.on_agent_install_requested(move |agent_id| {
            installed_agents.borrow_mut().push(agent_id.to_string());
        });
    }
    let created_profile = Rc::new(RefCell::new(Vec::<(String, String, bool, bool)>::new()));
    let deleted_profile = Rc::new(RefCell::new(Vec::<String>::new()));
    {
        let created_profile = created_profile.clone();
        panel.on_profile_create(move |name, agent_id, terminal_enabled, fs_enabled| {
            created_profile.borrow_mut().push((
                name.to_string(),
                agent_id.to_string(),
                terminal_enabled,
                fs_enabled,
            ));
        });
    }
    {
        let deleted_profile = deleted_profile.clone();
        panel.on_profile_delete(move |name| deleted_profile.borrow_mut().push(name.to_string()));
    }
    let attached_recovery = Rc::new(RefCell::new(Vec::<(String, String, String)>::new()));
    {
        let attached_recovery = attached_recovery.clone();
        panel.on_recover_session_attach(move |session_id, provider, title| {
            attached_recovery.borrow_mut().push((
                session_id.to_string(),
                provider.to_string(),
                title.to_string(),
            ));
        });
    }

    panel.set_mode_trigger_label("Ask".into());
    panel.set_mode_dropdown_entries(ModelRc::new(VecModel::from(vec![DropdownEntry {
        id: "plan".into(),
        label: "Plan".into(),
        value: "".into(),
        is_header: false,
        is_current: false,
    }])));
    panel.set_config_dropdown_entries(ModelRc::new(VecModel::from(vec![
        DropdownEntry {
            id: "reasoning".into(),
            label: "Reasoning".into(),
            value: "".into(),
            is_header: true,
            is_current: false,
        },
        DropdownEntry {
            id: "reasoning".into(),
            label: "High".into(),
            value: "high".into(),
            is_header: false,
            is_current: false,
        },
    ])));

    // The mode selector is a dropdown now: open it (its trigger is labelled
    // by the current mode), then pick "Plan".
    let mode_trigger = ElementHandle::find_by_accessible_label(&panel, "Ask")
        .next()
        .expect("mode selector trigger must be accessible");
    mode_trigger.invoke_accessible_default_action();
    let select_mode = ElementHandle::find_by_accessible_label(&panel, "Plan")
        .next()
        .expect("mode option must be accessible once the dropdown is open");
    select_mode.invoke_accessible_default_action();
    assert_eq!(&*mode_selection.borrow(), &["plan"]);

    // Same for the model/config selector -- open the "Config" trigger, then
    // pick the "High" value row.
    let config_trigger = ElementHandle::find_by_accessible_label(&panel, "Config")
        .next()
        .expect("model selector trigger must be accessible");
    config_trigger.invoke_accessible_default_action();
    let select_config = ElementHandle::find_by_accessible_label(&panel, "High")
        .next()
        .expect("config option must be accessible once the dropdown is open");
    select_config.invoke_accessible_default_action();
    assert_eq!(
        &*config_selection.borrow(),
        &[("reasoning".to_owned(), "high".to_owned())]
    );

    panel.set_available_profiles(ModelRc::new(VecModel::from(vec![ProfileOption {
        name: "codex-tools".into(),
        agent_id: "codex".into(),
        terminal_enabled: true,
        fs_enabled: true,
    }])));
    panel.set_available_mcp_servers(ModelRc::new(VecModel::from(vec![McpServerOption {
        name: "media-fs".into(),
        command: "node server.js".into(),
    }])));
    panel.set_agent_catalog(ModelRc::new(VecModel::from(vec![AgentCatalogEntry {
        id: "claude".into(),
        name: "Claude".into(),
        version: "1.0".into(),
        status: "not installed".into(),
    }])));
    panel.set_recoverable_sessions(ModelRc::new(VecModel::from(vec![RemoteSessionOption {
        session_id: "orphan-session-1".into(),
        provider: "codex".into(),
        title: "Fix export pipeline".into(),
        updated_at: "2026-07-16T10:00:00Z".into(),
    }])));
    panel.set_settings_open(true);
    // The settings sheet's own `Flickable` clips its content to the
    // window's height; `i-slint-backend-testing`'s `find_by_accessible_
    // label` in turn only sees elements within that clipped, currently
    // laid-out region -- not merely "logically in the tree but scrolled
    // out of view". This test populates profiles/MCP servers/agents
    // (making the sheet's real content taller than this headless
    // window's small default size), so the window is explicitly grown
    // here to fit everything without needing to also simulate scrolling
    // partway through the assertions below.
    panel.window().set_size(slint::LogicalSize::new(900.0, 1600.0));

    let profile = ElementHandle::find_by_accessible_label(&panel, "Select profile codex-tools")
        .next()
        .expect("profile chip must be accessible");
    assert_eq!(profile.id().as_deref(), Some("SettingsSheet::profile-chip"));
    profile.invoke_accessible_default_action();
    assert_eq!(panel.get_default_profile(), "codex-tools");

    // Profile delete is a two-step armed-confirm affordance on the same
    // chip row -- "Remove" arms it (doesn't call back yet), "Cancel"
    // disarms without ever calling back, and only "Confirm" actually
    // fires `profile-delete`.
    let remove_profile =
        ElementHandle::find_by_accessible_label(&panel, "Remove profile codex-tools")
            .next()
            .expect("profile remove control must be accessible");
    assert_eq!(
        remove_profile.id().as_deref(),
        Some("SettingsSheet::profile-delete-button")
    );
    remove_profile.invoke_accessible_default_action();
    assert!(deleted_profile.borrow().is_empty());

    let cancel_profile_delete =
        ElementHandle::find_by_accessible_label(&panel, "Cancel delete profile codex-tools")
            .next()
            .expect("profile delete-cancel control must be accessible");
    cancel_profile_delete.invoke_accessible_default_action();
    assert!(deleted_profile.borrow().is_empty());
    assert!(
        ElementHandle::find_by_accessible_label(&panel, "Confirm delete profile codex-tools")
            .next()
            .is_none(),
        "cancelling must disarm the confirm control, not leave it showing"
    );

    let remove_profile =
        ElementHandle::find_by_accessible_label(&panel, "Remove profile codex-tools")
            .next()
            .expect("profile remove control must be accessible again after cancel");
    remove_profile.invoke_accessible_default_action();
    let confirm_profile_delete =
        ElementHandle::find_by_accessible_label(&panel, "Confirm delete profile codex-tools")
            .next()
            .expect("profile delete-confirm control must be accessible once armed");
    assert_eq!(
        confirm_profile_delete.id().as_deref(),
        Some("SettingsSheet::profile-delete-confirm-button")
    );
    confirm_profile_delete.invoke_accessible_default_action();
    assert_eq!(&*deleted_profile.borrow(), &["codex-tools".to_owned()]);

    let profile_name_input = ElementHandle::find_by_accessible_label(&panel, "New profile name")
        .next()
        .expect("profile name input must be accessible");
    profile_name_input.invoke_accessible_default_action();
    for key in "media-agent".chars() {
        panel
            .window()
            .dispatch_event(WindowEvent::KeyPressed { text: key.to_string().into() });
    }
    let profile_terminal_toggle =
        ElementHandle::find_by_accessible_label(&panel, "Toggle new profile terminal access")
            .next()
            .expect("profile terminal-access toggle must be accessible");
    profile_terminal_toggle.invoke_accessible_default_action();
    let profile_add_button = ElementHandle::find_by_accessible_label(&panel, "Add profile")
        .next()
        .expect("profile add control must be accessible");
    assert_eq!(
        profile_add_button.id().as_deref(),
        Some("SettingsSheet::profile-add-button")
    );
    profile_add_button.invoke_accessible_default_action();
    assert_eq!(
        &*created_profile.borrow(),
        &[("media-agent".to_owned(), String::new(), true, false)]
    );

    let remove_mcp =
        ElementHandle::find_by_accessible_label(&panel, "Remove MCP server media-fs")
            .next()
            .expect("MCP delete must be accessible");
    assert_eq!(
        remove_mcp.id().as_deref(),
        Some("SettingsSheet::mcp-server-delete-button")
    );
    remove_mcp.invoke_accessible_default_action();
    assert_eq!(&*removed_mcp.borrow(), &["media-fs"]);

    let install_agent = ElementHandle::find_by_accessible_label(&panel, "Install agent Claude")
        .next()
        .expect("agent install must be accessible");
    assert_eq!(
        install_agent.id().as_deref(),
        Some("SettingsSheet::agent-install-button")
    );
    install_agent.invoke_accessible_default_action();
    assert_eq!(&*installed_agents.borrow(), &["claude"]);

    let mcp_name = ElementHandle::find_by_accessible_label(&panel, "New MCP server name")
        .next()
        .expect("MCP name input must be accessible");
    assert_eq!(
        mcp_name.id().as_deref(),
        Some("SettingsSheet::new-mcp-name-input")
    );
    mcp_name.invoke_accessible_default_action();
    for key in "review".chars() {
        panel
            .window()
            .dispatch_event(WindowEvent::KeyPressed { text: key.to_string().into() });
    }

    let mcp_command =
        ElementHandle::find_by_accessible_label(&panel, "New MCP server command")
            .next()
            .expect("MCP command input must be accessible");
    mcp_command.invoke_accessible_default_action();
    for key in "node server.js".chars() {
        panel
            .window()
            .dispatch_event(WindowEvent::KeyPressed { text: key.to_string().into() });
    }

    let add_mcp = ElementHandle::find_by_accessible_label(&panel, "Add MCP server")
        .next()
        .expect("MCP create control must be accessible");
    assert_eq!(
        add_mcp.id().as_deref(),
        Some("SettingsSheet::mcp-server-add-button")
    );
    add_mcp.invoke_accessible_default_action();
    assert_eq!(
        &*created_mcp.borrow(),
        &[("review".to_owned(), "node server.js".to_owned())]
    );

    let recover_attach = ElementHandle::find_by_accessible_label(
        &panel,
        "Attach recovered session orphan-session-1",
    )
    .next()
    .expect("recovery attach control must be accessible");
    assert_eq!(
        recover_attach.id().as_deref(),
        Some("SettingsSheet::recover-session-button")
    );
    recover_attach.invoke_accessible_default_action();
    assert_eq!(
        &*attached_recovery.borrow(),
        &[(
            "orphan-session-1".to_owned(),
            "codex".to_owned(),
            "Fix export pipeline".to_owned()
        )]
    );
}

#[test]
fn connection_status_is_exposed_to_accessibility() {
    i_slint_backend_testing::init_no_event_loop();

    let panel = ChatPanel::new().expect("construct chat panel");
    panel.set_compact(false);
    panel.set_connection_status("HTTP fallback - approvals unavailable".into());

    // `accessible-role: text` doesn't support `accessible-value` (only
    // certain roles do) -- the status is folded into `accessible-label`
    // itself instead, per that established convention (see
    // `chat_area.slint`'s connection-status element).
    let status =
        ElementHandle::find_by_accessible_label(&panel, "Connection status: HTTP fallback - approvals unavailable")
            .next()
            .expect("connection state must be exposed");
    assert!(status
        .accessible_label()
        .as_deref()
        .is_some_and(|label| label.contains("HTTP fallback - approvals unavailable")));
}

/// Coverage Matrix `session/close`/`session/delete` row -- sidebar's
/// per-thread two-step arm/confirm close/delete controls. Real
/// interaction coverage (accessible labels, click, confirm/cancel),
/// same discipline as this file's other component tests: proves the
/// UI wiring, not the gateway call itself (that's
/// `gateway_actor_e2e_test.rs::close_then_delete_session_round_trip_
/// through_a_real_gateway`'s job).
#[test]
fn sidebar_thread_close_and_delete_controls_are_addressable_and_two_step_confirmed() {
    i_slint_backend_testing::init_no_event_loop();

    let panel = ChatPanel::new().expect("construct chat panel");
    panel.set_sidebar_expanded(true);
    panel.set_threads(ModelRc::new(VecModel::from(vec![ThreadItem {
        name: "Fix timeline crash".into(),
        status: "idle".into(),
        busy: false,
        open: true,
        background: false,
        description: "".into(),
        closed: false,
        provider: "".into(),
        model: "".into(),
    }])));

    let closed_index = Rc::new(Cell::new(-1i32));
    let deleted_index = Rc::new(Cell::new(-1i32));
    {
        let closed_index = closed_index.clone();
        panel.on_thread_close_requested(move |i| closed_index.set(i));
    }
    {
        let deleted_index = deleted_index.clone();
        panel.on_thread_delete_requested(move |i| deleted_index.set(i));
    }

    // An open thread shows only the close (arm) control -- no delete
    // control, and no confirm/cancel pair, until armed.
    assert!(
        ElementHandle::find_by_accessible_label(&panel, "Delete thread Fix timeline crash")
            .next()
            .is_none(),
        "an open thread must not show a delete control"
    );
    let close_arm =
        ElementHandle::find_by_accessible_label(&panel, "Close thread Fix timeline crash")
            .next()
            .expect("close-arm control must be accessible on an open thread");

    close_arm.invoke_accessible_default_action();
    // Armed: confirm/cancel pair now accessible, the plain arm control
    // is gone (replaced, not merely covered).
    assert!(
        ElementHandle::find_by_accessible_label(&panel, "Close thread Fix timeline crash")
            .next()
            .is_none(),
        "the arm control must disappear once armed"
    );
    let cancel_close = ElementHandle::find_by_accessible_label(
        &panel,
        "Cancel close thread Fix timeline crash",
    )
    .next()
    .expect("cancel-close control must be accessible once armed");
    cancel_close.invoke_accessible_default_action();
    assert_eq!(closed_index.get(), -1, "cancel must not fire the callback");
    // Cancelling re-shows the plain arm control.
    ElementHandle::find_by_accessible_label(&panel, "Close thread Fix timeline crash")
        .next()
        .expect("arm control must reappear after cancel");

    // Real arm -> confirm round trip.
    let close_arm =
        ElementHandle::find_by_accessible_label(&panel, "Close thread Fix timeline crash")
            .next()
            .expect("close-arm control must still be accessible");
    close_arm.invoke_accessible_default_action();
    let confirm_close = ElementHandle::find_by_accessible_label(
        &panel,
        "Confirm close thread Fix timeline crash",
    )
    .next()
    .expect("confirm-close control must be accessible once armed");
    confirm_close.invoke_accessible_default_action();
    assert_eq!(
        closed_index.get(),
        0,
        "confirming close must fire thread-close-requested(0)"
    );

    // Once the bridge reports the thread closed (Rust re-reads
    // `AgentBridge::thread_closed` and rebuilds the model -- simulated
    // here by setting `closed: true` directly, matching what
    // `refresh_threads_model` would push), the row swaps to a delete
    // control instead of a close control.
    panel.set_threads(ModelRc::new(VecModel::from(vec![ThreadItem {
        name: "Fix timeline crash".into(),
        status: "closed".into(),
        busy: false,
        open: true,
        background: false,
        description: "".into(),
        closed: true,
        provider: "".into(),
        model: "".into(),
    }])));
    assert!(
        ElementHandle::find_by_accessible_label(&panel, "Close thread Fix timeline crash")
            .next()
            .is_none(),
        "a closed thread must not show a close control"
    );
    let delete_arm =
        ElementHandle::find_by_accessible_label(&panel, "Delete thread Fix timeline crash")
            .next()
            .expect("delete-arm control must be accessible on a closed thread");
    delete_arm.invoke_accessible_default_action();
    let confirm_delete = ElementHandle::find_by_accessible_label(
        &panel,
        "Confirm delete thread Fix timeline crash",
    )
    .next()
    .expect("confirm-delete control must be accessible once armed");
    confirm_delete.invoke_accessible_default_action();
    assert_eq!(
        deleted_index.get(),
        0,
        "confirming delete must fire thread-delete-requested(0)"
    );
}
