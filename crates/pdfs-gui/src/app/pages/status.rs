use crate::*;

pub(crate) struct StatusState {
    /// Whether a [`Request::Status`] round-trip is already in flight, so the 2s
    /// refresh tick doesn't pile worker threads up on a slow/wedged daemon.
    pub(crate) status_inflight: Cell<bool>,
    /// Guards the [`Request::GetQueueStatus`] poll the same way: at most one
    /// in-flight at a time so a wedged daemon can't stack worker threads.
    pub(crate) transfers_inflight: Cell<bool>,
    /// Activity group + its current rows, hidden when no transfer is in flight.
    pub(crate) transfers_group: adw::PreferencesGroup,
    pub(crate) transfer_rows: RefCell<Vec<TransferRow>>,
    // Main page.
    pub(crate) account_row: adw::ActionRow,
    /// Read-only mount status line. The mount is driven by the systemd user
    /// service under the selected lifecycle policy, so this only reports.
    pub(crate) mount_row: adw::ActionRow,
    pub(crate) cache_bar: gtk4::ProgressBar,
    pub(crate) cache_label: gtk4::Label,
    /// Account-quota row + its bar/label. Hidden until the first reading.
    pub(crate) quota_row: adw::PreferencesRow,
    pub(crate) quota_bar: gtk4::ProgressBar,
    pub(crate) quota_label: gtk4::Label,
    /// One `AccountQuota` in flight at a time, and when it last succeeded — quota
    /// barely moves, so [`refresh_quota`] refetches on a long TTL, not every tick.
    pub(crate) quota_inflight: Cell<bool>,
    pub(crate) quota_checked_at: Cell<Option<Instant>>,
    /// Whether the daemon should remain available after the app closes.
    /// [`Self::settings_suppress`] guards programmatic changes.
    pub(crate) background_service_row: gtk4::Switch,
    /// Cache-budget editor (GiB). Populated once from config; user edits drive a
    /// `SetCacheBudget` round-trip. Guarded by [`Self::settings_suppress`].
    pub(crate) budget_row: gtk4::SpinButton,
    /// Debounce + serialization state for cache-limit edits. Only the newest
    /// value is written after rapid scroll/typing, and at most one config write
    /// is in flight so an older reply cannot overwrite a newer choice.
    pub(crate) budget_source: RefCell<Option<glib::SourceId>>,
    pub(crate) budget_inflight: Cell<bool>,
    pub(crate) budget_pending: Cell<Option<u64>>,
    /// Shows where the primary mount lives; the folder itself is managed on the
    /// Locations page, which owns every local path.
    pub(crate) mountpoint_row: adw::ActionRow,
    /// Set while a settings widget is being populated programmatically, so its
    /// change handler skips the IPC/systemd side effect.
    pub(crate) settings_suppress: Cell<bool>,
    /// The mount state the *last* desktop notification reported, so a flap only
    /// notifies on the edge. `None` until the first status reply, so a cold start
    /// doesn't announce "disconnected" before the service has had a chance to come
    /// up.
    pub(crate) notified_mounted: Cell<Option<bool>>,
    /// How many transfers were in flight on the previous poll. A drop to zero is
    /// what "sync complete" means; there's no completion event on the wire.
    pub(crate) active_transfers: Cell<usize>,
}

/// One rendered row in the Activity group: a description over a progress bar.
/// Retained so [`repaint_transfers`] can update the bar and label in place each
/// tick when the active set is unchanged, instead of rebuilding.
pub(crate) struct TransferRow {
    pub(crate) row: adw::PreferencesRow,
    pub(crate) label: gtk4::Label,
    pub(crate) bar: gtk4::ProgressBar,
}

/// What one Activity row should say this tick, and how far along it is —
/// `None` meaning "no total known", which the bar shows by pulsing. Jobs and
/// transfers both render to this, so the group is one list in the order the
/// daemon reports: the jobs that frame the work, then the files moving under it.
pub(crate) struct ActivityLine {
    pub(crate) text: String,
    pub(crate) fraction: Option<f64>,
}

/// Widgets the settings page hands back for the refresh loop and action wiring.
pub(crate) struct MainWidgets {
    pub(crate) account_row: adw::ActionRow,
    pub(crate) mount_row: adw::ActionRow,
    /// Account-quota row; hidden until `refresh_quota` gets a reading.
    pub(crate) quota_row: adw::PreferencesRow,
    pub(crate) quota_bar: gtk4::ProgressBar,
    pub(crate) quota_label: gtk4::Label,
    pub(crate) cache_bar: gtk4::ProgressBar,
    pub(crate) cache_label: gtk4::Label,
    pub(crate) logout_button: gtk4::Button,
    /// Whether the daemon should remain available after the app closes.
    pub(crate) background_service_row: gtk4::Switch,
    /// Cache soft-cap editor, in GiB; `0` = unlimited.
    pub(crate) budget_row: gtk4::SpinButton,
    /// Purges all unpinned cached content.
    pub(crate) purge_button: gtk4::Button,
    /// Shows the active mountpoint; Locations is a separate row below it.
    pub(crate) mountpoint_row: adw::ActionRow,
    pub(crate) locations_button: gtk4::Button,
}

const SETTINGS_METRIC_SPACING: i32 = 6;
const SETTINGS_METRIC_VERTICAL_MARGIN: i32 = 8;
const SETTINGS_METRIC_HORIZONTAL_MARGIN: i32 = 12;

fn settings_group(title: &str, description: Option<&str>) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title(title).build();
    group.set_description(description);
    group
}

fn settings_row(title: &str, subtitle: Option<&str>) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).build();
    if let Some(subtitle) = subtitle {
        row.set_subtitle(subtitle);
    }
    row
}

fn settings_icon_button(icon: &str, tooltip: &str, action: Option<&str>) -> gtk4::Button {
    let button = gtk4::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .valign(gtk4::Align::Center)
        .build();
    button.add_css_class("flat");
    button.set_action_name(action);
    button
}

fn settings_disclosure_row(
    title: &str,
    subtitle: &str,
    icon: &str,
    tooltip: &str,
    action: Option<&str>,
) -> (adw::ActionRow, gtk4::Button) {
    let row = settings_row(title, Some(subtitle));
    row.add_prefix(&gtk4::Image::from_icon_name(icon));
    let button = settings_icon_button("go-next-symbolic", tooltip, action);
    row.add_suffix(&button);
    row.set_activatable_widget(Some(&button));
    (row, button)
}

/// A shared full-width usage/progress row. Account quota, local cache and live
/// transfers all use this exact spacing and secondary-text treatment.
fn settings_metric_row() -> (adw::PreferencesRow, gtk4::ProgressBar, gtk4::Label) {
    let content = gtk4::Box::new(gtk4::Orientation::Vertical, SETTINGS_METRIC_SPACING);
    content.set_margin_top(SETTINGS_METRIC_VERTICAL_MARGIN);
    content.set_margin_bottom(SETTINGS_METRIC_VERTICAL_MARGIN);
    content.set_margin_start(SETTINGS_METRIC_HORIZONTAL_MARGIN);
    content.set_margin_end(SETTINGS_METRIC_HORIZONTAL_MARGIN);
    let label = gtk4::Label::builder()
        .halign(gtk4::Align::Start)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .build();
    label.add_css_class("caption");
    label.add_css_class("dim-label");
    let bar = gtk4::ProgressBar::new();
    content.append(&label);
    content.append(&bar);
    let row = adw::PreferencesRow::builder()
        .activatable(false)
        .child(&content)
        .build();
    (row, bar, label)
}

/// The logged-in Settings page uses `AdwPreferencesPage` as its single layout
/// authority. It supplies the clamp, scrolling, group spacing, insets and native
/// title/subtitle typography instead of duplicating those values per section.
pub(crate) fn build_main_page() -> (gtk4::Widget, MainWidgets) {
    let account_group = settings_group(
        "Account",
        Some("Your Proton account and storage shared across Proton products."),
    );
    let account_row = settings_row("Not signed in", None);
    account_row.add_prefix(&adw::Avatar::new(40, None, true));
    let logout_button = gtk4::Button::builder()
        .label("Sign out")
        .valign(gtk4::Align::Center)
        .build();
    logout_button.add_css_class("flat");
    account_row.add_suffix(&logout_button);
    account_group.add(&account_row);

    let (quota_row, quota_bar, quota_label) = settings_metric_row();
    quota_row.set_visible(false);
    account_group.add(&quota_row);

    let drive_group = settings_group(
        "Drive",
        Some("Connection, background service and local locations."),
    );
    let mount_row = settings_row("Connection", Some("Not connected"));
    drive_group.add(&mount_row);

    let background_service_setting_row = settings_row(
        "Keep Drive running in background",
        Some("Keep mounts, sync, tray actions and Drive search available after closing the app."),
    );
    let background_service_row = gtk4::Switch::builder().valign(gtk4::Align::Center).build();
    background_service_setting_row.add_suffix(&background_service_row);
    background_service_setting_row.set_activatable_widget(Some(&background_service_row));
    drive_group.add(&background_service_setting_row);

    let (mountpoint_row, locations_button) = settings_disclosure_row(
        "Locations",
        "Manage the mountpoint and synced folders.",
        "drive-harddisk-symbolic",
        "Open Locations",
        None,
    );
    drive_group.add(&mountpoint_row);

    let storage_group = settings_group(
        "Local storage",
        Some("Cache for offline copies and recently opened files."),
    );
    let (usage_row, cache_bar, cache_label) = settings_metric_row();
    storage_group.add(&usage_row);

    let budget_adj = gtk4::Adjustment::new(0.0, 0.0, 1024.0, 0.5, 1.0, 0.0);
    let budget_setting_row = settings_row(
        "Cache limit (GiB)",
        Some("Maximum local cache size; 0 means unlimited."),
    );
    let budget_row = gtk4::SpinButton::new(Some(&budget_adj), 0.5, 1);
    budget_row.set_valign(gtk4::Align::Center);
    budget_setting_row.add_suffix(&budget_row);
    budget_setting_row.set_activatable_widget(Some(&budget_row));
    storage_group.add(&budget_setting_row);

    let purge_row = settings_row(
        "Clear cache",
        Some("Remove cached files while keeping offline copies."),
    );
    let purge_button = gtk4::Button::builder()
        .label("Clear")
        .valign(gtk4::Align::Center)
        .build();
    purge_button.add_css_class("destructive-action");
    purge_row.add_suffix(&purge_button);
    storage_group.add(&purge_row);

    let application_group = settings_group("Application", None);
    let (shortcuts_row, _) = settings_disclosure_row(
        "Keyboard shortcuts",
        "View available keyboard commands.",
        "preferences-desktop-keyboard-shortcuts-symbolic",
        "Open keyboard shortcuts",
        Some("win.shortcuts"),
    );
    application_group.add(&shortcuts_row);
    let (about_row, _) = settings_disclosure_row(
        "About Proton Drive",
        "View version, credits, and application information.",
        "help-about-symbolic",
        "Open About Proton Drive",
        Some("win.about"),
    );
    application_group.add(&about_row);

    let diagnostics_group = settings_group(
        "Diagnostics",
        Some("Information useful for troubleshooting."),
    );
    diagnostics_group.add(&settings_row(
        "Version",
        Some(pdfs_core::config::APP_VERSION),
    ));
    diagnostics_group.add(&settings_row(
        "User agent",
        Some(pdfs_core::config::USER_AGENT),
    ));

    let page = adw::PreferencesPage::new();
    for group in [
        &account_group,
        &drive_group,
        &storage_group,
        &application_group,
        &diagnostics_group,
    ] {
        page.add(group);
    }

    (
        page.upcast(),
        MainWidgets {
            account_row,
            mount_row,
            quota_row,
            quota_bar,
            quota_label,
            cache_bar,
            cache_label,
            logout_button,
            background_service_row,
            budget_row,
            purge_button,
            mountpoint_row,
            locations_button,
        },
    )
}

/// Bytes per GiB, for the cache-budget editor's unit conversion.
pub(crate) const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// Wire the Settings-page controls: the cache-budget editor, the purge button,
/// the background-service switch and the mountpoint chooser. Initial widget
/// state is read once from config here (the refresh loop owns only the live
/// mount + cache-usage read-out), with `settings_suppress` set around the
/// programmatic populate so the change handlers don't fire on it.
pub(crate) fn wire_settings(
    ui: &Rc<Ui>,
    purge_button: &gtk4::Button,
    locations_button: &gtk4::Button,
) {
    let config = ui.dirs.load_config();

    // Populate from persisted config, suppressed.
    ui.status.settings_suppress.set(true);
    ui.status
        .budget_row
        .set_value(config.resolved_cache_budget() as f64 / GIB);
    ui.status
        .mountpoint_row
        .set_subtitle(&ui.dirs.resolved_mountpoint(&config).display().to_string());
    ui.status
        .background_service_row
        .set_active(config.resolved_keep_service_running());
    ui.status.settings_suppress.set(false);

    // Cache budget: a user edit applies the new soft cap on the daemon (which
    // also persists it to config). 0 GiB = unlimited.
    const CACHE_LIMIT_DEBOUNCE: Duration = Duration::from_millis(300);
    let ui_budget = ui.clone();
    ui.status.budget_row.connect_value_notify(move |row| {
        if ui_budget.status.settings_suppress.get() {
            return;
        }
        let bytes = (row.value() * GIB).round() as u64;
        ui_budget.status.budget_pending.set(Some(bytes));
        if let Some(source) = ui_budget.status.budget_source.borrow_mut().take() {
            source.remove();
        }
        let ui_flush = ui_budget.clone();
        let source = glib::timeout_add_local_once(CACHE_LIMIT_DEBOUNCE, move || {
            ui_flush.status.budget_source.borrow_mut().take();
            if let Some(bytes) = ui_flush.status.budget_pending.take() {
                apply_cache_budget(&ui_flush, bytes);
            }
        });
        *ui_budget.status.budget_source.borrow_mut() = Some(source);
    });

    // Clear: confirm, then drop all cached content that is not an offline copy.
    let ui_purge = ui.clone();
    purge_button.connect_clicked(move |_| {
        let ui = ui_purge.clone();
        let dialog = adw::MessageDialog::builder()
            .heading("Clear local cache?")
            .body("Cached files will be removed. Offline copies will stay on this device.")
            .build();
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("purge", "Clear");
        dialog.set_response_appearance("purge", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        dialog.connect_response(None, move |_, resp| {
            if resp == "purge" {
                settings_request(
                    &ui,
                    Request::PurgeCache,
                    "Cache cleared",
                    "Couldn't clear cache",
                );
            }
        });
        dialog.set_transient_for(ui_window(&ui_purge).as_ref());
        dialog.present();
    });

    // Persist the lifecycle choice first, then apply the systemd policy on a
    // worker. The switch is insensitive until systemctl finishes, preventing
    // rapid toggles from racing enable/disable/start operations.
    let ui_background = ui.clone();
    ui.status
        .background_service_row
        .connect_active_notify(move |row| {
            if ui_background.status.settings_suppress.get() {
                return;
            }

            let keep_running = row.is_active();
            let mut config = ui_background.dirs.load_config();
            config.keep_service_running = Some(keep_running);
            if let Err(error) = ui_background.dirs.save_config(&config) {
                ui_background.status.settings_suppress.set(true);
                row.set_active(!keep_running);
                ui_background.status.settings_suppress.set(false);
                toast_error(
                    &ui_background,
                    "Couldn't save background setting",
                    &error.to_string(),
                );
                return;
            }

            row.set_sensitive(false);
            ui_background.busy_begin();
            let app_hold = hold_application();
            let (tx, rx) = async_channel::bounded(1);
            std::thread::spawn(move || {
                let _ = tx.send_blocking(service::start_for_app(keep_running));
            });

            let ui = ui_background.clone();
            let row = row.clone();
            glib::spawn_future_local(async move {
                let applied = rx.recv().await.unwrap_or(false);
                ui.busy_end();
                row.set_sensitive(true);
                if applied {
                    toast(
                        &ui,
                        if keep_running {
                            "Drive will keep running in background"
                        } else {
                            "Drive will stop when the app closes"
                        },
                    );
                } else {
                    toast_error(
                        &ui,
                        "Couldn't update the background service",
                        "The preference was saved and will be retried when the app next starts.",
                    );
                }
                drop(app_hold);
            });
        });

    // Mountpoint: the chooser lives on the Locations page, next to every other
    // local path; this button is the way there.
    let ui_mp = ui.clone();
    locations_button.connect_clicked(move |_| ui_mp.stack.set_visible_child_name("locations"));
}

/// Start the daemon for a signed-in app without blocking GTK. The application
/// hold closes the open/close race: if the window is immediately dismissed, the
/// start policy finishes before the shutdown hook applies the matching stop
/// policy.
pub(crate) fn start_service_for_app(ui: &Rc<Ui>) {
    if ui.session.borrow().is_none() {
        return;
    }
    let keep_running = ui.dirs.load_config().resolved_keep_service_running();
    let app_hold = hold_application();
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send_blocking(service::start_for_app(keep_running));
    });
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        if !rx.recv().await.unwrap_or(false) {
            toast_error(
                &ui,
                "Couldn't start the Drive service",
                "Check the user service and try again from Files.",
            );
        }
        drop(app_hold);
    });
}

fn apply_cache_budget(ui: &Rc<Ui>, bytes: u64) {
    if ui.status.budget_inflight.replace(true) {
        ui.status.budget_pending.set(Some(bytes));
        return;
    }
    ui.busy_begin();
    let rx = spawn_request(ui.dirs.control_socket(), Request::SetCacheBudget { bytes });
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        ui.status.budget_inflight.set(false);
        if let Some(next) = ui.status.budget_pending.take() {
            apply_cache_budget(&ui, next);
            return;
        }
        match result {
            Ok(Ok(Response::Ok { .. })) => toast(&ui, "Cache limit updated"),
            Ok(Ok(Response::Error { message, kind })) => {
                toast_failure(&ui, "Couldn't set cache limit", &message, kind)
            }
            _ => toast_error(
                &ui,
                "Couldn't set cache limit",
                "The mount service didn't respond.",
            ),
        }
    });
}

/// Run a settings control-socket round-trip (budget / purge) on a worker thread,
/// confirming with `done` or reporting the daemon's error under `failed`. Unlike
/// [`run_mutation`] there's no browser reload; the next refresh tick repaints the
/// cache read-out.
pub(crate) fn settings_request(
    ui: &Rc<Ui>,
    req: Request,
    done: &'static str,
    failed: &'static str,
) {
    ui.busy_begin();
    let rx = spawn_request(ui.dirs.control_socket(), req);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        match result {
            Ok(Ok(Response::Ok { .. })) => toast(&ui, done),
            Ok(Ok(Response::Error { message, kind })) => toast_failure(&ui, failed, &message, kind),
            _ => toast_error(&ui, failed, "The mount service didn't respond."),
        }
    });
}

/// Prompt for a new mountpoint folder, persist it to config, and offer to restart
/// the mount service so the daemon picks it up.
pub(crate) fn prompt_mountpoint(ui: &Rc<Ui>) {
    let win = ui_window(ui);
    let ui = ui.clone();
    choose_folder(win.as_ref(), "Choose mountpoint folder", move |path| {
        let path_str = path.display().to_string();

        // Persist the choice to config so the next mount uses it.
        let mut config = ui.dirs.load_config();
        config.mountpoint = Some(path_str.clone());
        if let Err(e) = ui.dirs.save_config(&config) {
            toast_error(&ui, "Couldn't save mountpoint", &e.to_string());
            return;
        }
        ui.status.mountpoint_row.set_subtitle(&path_str);
        // The Locations page shows the same path as a row title; its cached rows
        // are now stale whether or not it is the visible page.
        ui.locations.loaded_at.set(None);
        if ui.stack.visible_child_name().as_deref() == Some("locations") {
            load_locations(&ui);
        }

        // The daemon only reads the mountpoint at mount time, so offer a restart.
        let confirm = adw::MessageDialog::builder()
            .heading("Restart to apply")
            .body(format!(
                "The mountpoint is now “{path_str}”. Restart the Drive mount to use it?"
            ))
            .build();
        confirm.add_response("later", "Later");
        confirm.add_response("restart", "Restart now");
        confirm.set_response_appearance("restart", adw::ResponseAppearance::Suggested);
        confirm.set_default_response(Some("restart"));
        confirm.set_close_response("later");
        confirm.connect_response(None, |_, resp| {
            if resp == "restart" {
                service::restart();
            }
        });
        confirm.set_transient_for(ui_window(&ui).as_ref());
        confirm.present();
    });
}

/// Connect the Files/Photos "Retry" buttons (shown by [`browser_unreachable`] /
/// [`gallery_unreachable`] when the mount is down): restart the systemd unit and
/// reload the page.
pub(crate) fn wire_retry(ui: &Rc<Ui>) {
    let ui_browser = ui.clone();
    ui.browser.retry.clone().connect_clicked(move |_| {
        service::restart();
        reload_listing(&ui_browser);
    });
    let ui_gallery = ui.clone();
    ui.gallery.retry.clone().connect_clicked(move |_| {
        service::restart();
        load_gallery(&ui_gallery, false);
    });
    let ui_trash = ui.clone();
    ui.trash.retry.clone().connect_clicked(move |_| {
        service::restart();
        load_trash(&ui_trash);
    });
}

/// Repaint the window from the cached login identity, then kick an async mount-
/// status fetch. Runs on the 2s tick: the identity check is instant (no keyring),
/// and the status round-trip is offloaded to a worker so the main loop never
/// blocks on a slow or wedged daemon.
pub(crate) fn refresh(ui: &Rc<Ui>) {
    // Login identity decides which page is shown. Read the cached session — set
    // at startup and on login/logout — never the keyring.
    {
        let session = ui.session.borrow();
        match session.as_ref() {
            Some(s) => {
                // Only pull the user onto a destination when they're sitting on the
                // login page; otherwise leave whichever page they navigated to.
                if ui.stack.visible_child_name().as_deref() == Some("login") {
                    ui.stack.set_visible_child_name("browser");
                }
                ui.nav.set_visible(true);
                ui.status.account_row.set_title(&s.username);
                ui.status.account_row.set_subtitle("Proton account");
            }
            None => {
                ui.stack.set_visible_child_name("login");
                // Hiding the navigation leaves the login page as the only
                // reachable content while signed out.
                ui.nav.set_visible(false);
                return;
            }
        }
    }

    refresh_status(ui);
    refresh_transfers(ui);
    // Both of these pages show work as it happens, so they follow the tick while
    // they are on screen. Every other page loads on navigation only.
    match ui.stack.visible_child_name().as_deref() {
        Some("main" | "browser") => refresh_quota(ui),
        Some("locations") => refresh_locations(ui),
        Some("activity") => refresh_activity(ui),
        _ => {}
    }
}

/// How long a quota reading stays fresh. Account storage barely moves, so the
/// active-page tick refetches it only this often rather than every 2s.
const QUOTA_TTL: Duration = Duration::from_secs(60);

/// Fetch the account quota (if the last reading is stale) and paint the Account
/// storage group and the Files status bar. Runs while either surface is on
/// screen. A failed fetch leaves the last good reading in place.
pub(crate) fn refresh_quota(ui: &Rc<Ui>) {
    if ui.status.quota_inflight.get() {
        return;
    }
    if let Some(at) = ui.status.quota_checked_at.get()
        && at.elapsed() < QUOTA_TTL
    {
        return;
    }
    ui.status.quota_inflight.set(true);
    let rx = spawn_request(ui.dirs.control_socket(), Request::AccountQuota);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.status.quota_inflight.set(false);
        if let Ok(Ok(Response::AccountQuota {
            max_space,
            used_space,
        })) = result
        {
            ui.status.quota_checked_at.set(Some(Instant::now()));
            paint_account_quota(&ui, max_space, used_space);
            ui.status.quota_row.set_visible(true);
        } else if ui.status.quota_checked_at.get().is_none() {
            // Match Dolphin: capacity information does not occupy the bar until
            // the backing observer (the Proton API here) has real figures.
            ui.browser.quota_box.set_visible(false);
        }
    });
}

fn paint_account_quota(ui: &Rc<Ui>, max_space: i64, used_space: i64) {
    let (fraction, text) = quota_display(max_space, used_space);
    ui.status.quota_bar.set_fraction(fraction);
    ui.status
        .quota_label
        .set_text(&format!("Account storage · {text}"));
    if let Some((fraction, free_text, tooltip)) = quota_status_display(max_space, used_space) {
        ui.browser.quota.set_fraction(fraction);
        ui.browser.quota.set_tooltip_text(Some(&tooltip));
        ui.browser.quota_text.set_label(&free_text);
        ui.browser.quota_text.set_tooltip_text(Some(&tooltip));
        ui.browser.quota_box.set_visible(true);
    } else {
        ui.browser.quota_box.set_visible(false);
    }
}

fn quota_display(max_space: i64, used_space: i64) -> (f64, String) {
    let used = used_space.max(0) as u64;
    if max_space <= 0 {
        return (0.0, format!("{} used", human_bytes(used)));
    }
    let total = max_space as u64;
    let fraction = (used as f64 / total as f64).clamp(0.0, 1.0);
    let pct = (fraction * 100.0).round() as u64;
    (
        fraction,
        format!(
            "{} of {} used ({pct}%)",
            human_bytes(used),
            human_bytes(total)
        ),
    )
}

/// Dolphin's status bar shows a bare capacity bar followed by “X free”; the
/// full free/total/percentage sentence is a tooltip rather than inline bar text.
fn quota_status_display(max_space: i64, used_space: i64) -> Option<(f64, String, String)> {
    if max_space <= 0 {
        return None;
    }
    let total = max_space as u64;
    let used = (used_space.max(0) as u64).min(total);
    let free = total.saturating_sub(used);
    let fraction = used as f64 / total as f64;
    let pct = (fraction * 100.0).round() as u64;
    Some((
        fraction,
        format!("{} free", human_bytes(free)),
        format!(
            "{} free out of {} ({pct}% used)",
            human_bytes(free),
            human_bytes(total)
        ),
    ))
}

/// Record the mount state seen by the last status poll: gate every control that
/// needs a live daemon, and notify the desktop when the state actually flips.
///
/// The gating is the point — without it, New Folder / Upload stay clickable while
/// the mount is down and each click buys a round-trip that can only fail. A greyed
/// control says so up front.
pub(crate) fn set_mounted(ui: &Rc<Ui>, mounted: bool) {
    *ui.mounted.borrow_mut() = mounted;
    ui.browser.new_folder.set_sensitive(mounted);
    ui.browser.upload.set_sensitive(mounted);
    ui.browser.upload_folder.set_sensitive(mounted);
    ui.browser
        .build_thumbnails
        .set_sensitive(mounted && !ui.browser.thumbnail_build_running.get());
    ui.gallery.upload.set_sensitive(mounted);
    // Only notify on a real edge, and never for the first reading: at startup the
    // service is usually still coming up, and "disconnected" would be a lie.
    if ui.status.notified_mounted.get() == Some(mounted) {
        return;
    }
    let first = ui.status.notified_mounted.replace(Some(mounted)).is_none();
    if first {
        return;
    }
    if mounted {
        notify(
            "mount-state",
            "Proton Drive connected",
            "Your Drive is mounted and available.",
        );
    } else {
        notify(
            "mount-state",
            "Proton Drive disconnected",
            "The mount service stopped. Files aren't available until it restarts.",
        );
    }
}

/// Poll the daemon's in-flight transfers on a worker thread and repaint the
/// Activity group. Independently inflight-guarded from [`refresh_status`] so the
/// two cheap polls on the 2s tick don't gate each other. The group hides itself
/// when nothing is moving, so an idle account shows no Activity section.
pub(crate) fn refresh_transfers(ui: &Rc<Ui>) {
    if ui.status.transfers_inflight.get() {
        return;
    }
    ui.status.transfers_inflight.set(true);
    let rx = spawn_request(ui.dirs.control_socket(), Request::GetQueueStatus);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.status.transfers_inflight.set(false);
        match result {
            Ok(Ok(Response::Transfers { items, jobs })) => repaint_transfers(&ui, &items, &jobs),
            // Daemon unreachable or odd reply: clear the section rather than
            // leave stale progress bars frozen on screen.
            _ => repaint_transfers(&ui, &[], &[]),
        }
    });
}

/// Render the Activity group from a work snapshot: the daemon's jobs (scans,
/// batch counts, the local index) above the files moving under them. Rebuilds the
/// rows only when the set changes (count differs); on the common steady tick it
/// updates each bar's fraction and the label in place, so progress animates
/// without flicker. Hides the whole group when the daemon is idle.
pub(crate) fn repaint_transfers(ui: &Rc<Ui>, items: &[TransferItem], jobs: &[JobItem]) {
    let lines: Vec<ActivityLine> = jobs
        .iter()
        .map(job_line)
        .chain(items.iter().map(transfer_line))
        .collect();

    // The wire carries in-flight transfers only, with no completion event: the
    // count falling to zero is what "the sync finished" looks like from here.
    // Jobs are deliberately not counted — a bulk upload retires its scan job and
    // starts its upload job mid-flight, which is not a thing finishing.
    let previous = ui.status.active_transfers.replace(items.len());
    if items.is_empty() && previous > 0 {
        // Uploads and remote downloads can change account usage. Expire the
        // cached reading so the next active-page tick asks Proton again.
        ui.status.quota_checked_at.set(None);
        let files = if previous == 1 {
            "1 file".to_string()
        } else {
            format!("{previous} files")
        };
        notify(
            "sync-complete",
            "Sync complete",
            &format!("{files} finished transferring."),
        );
        // A just-finished batch may have added files (bulk upload) the current
        // listing doesn't show yet; refresh whichever listing is on screen.
        reload_listing(ui);
    }

    if lines.is_empty() {
        if !ui.status.transfer_rows.borrow().is_empty() {
            for tr in ui.status.transfer_rows.borrow_mut().drain(..) {
                ui.status.transfers_group.remove(&tr.row);
            }
        }
        ui.status.transfers_group.set_visible(false);
        return;
    }

    ui.status.transfers_group.set_visible(true);

    // Rebuild rows only when the count changes; otherwise reuse them in place.
    if ui.status.transfer_rows.borrow().len() != lines.len() {
        for tr in ui.status.transfer_rows.borrow_mut().drain(..) {
            ui.status.transfers_group.remove(&tr.row);
        }
        for _ in &lines {
            let (row, bar, label) = settings_metric_row();
            ui.status.transfers_group.add(&row);
            ui.status
                .transfer_rows
                .borrow_mut()
                .push(TransferRow { row, label, bar });
        }
    }

    for (line, tr) in lines.iter().zip(ui.status.transfer_rows.borrow().iter()) {
        tr.label.set_text(&line.text);
        match line.fraction {
            Some(f) => tr.bar.set_fraction(f),
            // No total to divide by: pulse so the bar still reads as "working".
            None => tr.bar.pulse(),
        }
    }
}

/// One Activity row for a daemon job: its title, plus whatever it can say about
/// where it is — a count when it has one, else what it is currently chewing on.
pub(crate) fn job_line(j: &JobItem) -> ActivityLine {
    let text = match (j.total > 0, j.detail.is_empty()) {
        (true, true) => format!("{} — {} of {}", j.title, j.done, j.total),
        (true, false) => format!("{} — {} ({} of {})", j.title, j.detail, j.done, j.total),
        (false, true) => format!("{}…", j.title),
        (false, false) => format!("{} — {}…", j.title, j.detail),
    };
    ActivityLine {
        text,
        fraction: (j.total > 0).then(|| (j.done as f64 / j.total as f64).min(1.0)),
    }
}

/// One Activity row for a file in flight: which way it's going, how far, how fast.
pub(crate) fn transfer_line(t: &TransferItem) -> ActivityLine {
    let arrow = match t.direction {
        TransferDirection::Download => "↓",
        TransferDirection::Upload => "↑",
    };
    if t.bytes_total == 0 {
        ActivityLine {
            text: format!(
                "{arrow} {} — {} ({}/s)",
                t.name,
                human_bytes(t.bytes_completed),
                human_bytes(t.speed_bytes_sec),
            ),
            fraction: None,
        }
    } else {
        ActivityLine {
            text: format!(
                "{arrow} {} — {} of {} ({}/s)",
                t.name,
                human_bytes(t.bytes_completed),
                human_bytes(t.bytes_total),
                human_bytes(t.speed_bytes_sec),
            ),
            fraction: Some((t.bytes_completed as f64 / t.bytes_total as f64).min(1.0)),
        }
    }
}

/// Fetch mount status + bounded cache stats from the daemon on a worker thread
/// and repaint the connection and cache rows. Full offline-copy paths are never
/// part of this two-second poll; they are available through the explicit
/// `ListPins` request used by Quick Search and the CLI.
pub(crate) fn refresh_status(ui: &Rc<Ui>) {
    if ui.status.status_inflight.get() {
        return;
    }
    ui.status.status_inflight.set(true);
    let rx = spawn_request(ui.dirs.control_socket(), Request::Status);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.status.status_inflight.set(false);
        match result {
            Ok(Ok(Response::Status {
                mountpoint,
                pinned,
                used,
                budget,
                online,
                pending_uploads,
                pending_changes,
                ..
            })) => {
                set_mounted(&ui, true);
                // The queue is the more useful thing to say when it has anything
                // in it: it is why a file that looks saved is not on the remote
                // yet, and offline is usually the reason it is still queued.
                let queued = pending_summary(pending_uploads, pending_changes);
                ui.status.mount_row.set_subtitle(&match (online, queued) {
                    (true, None) => format!("Connected at {mountpoint}"),
                    (true, Some(q)) => format!("Connected at {mountpoint} — {q}"),
                    (false, None) => {
                        format!("Connected at {mountpoint} — offline, cached files only")
                    }
                    (false, Some(q)) => format!("Connected at {mountpoint} — offline, {q}"),
                });
                let fraction = if budget == 0 {
                    0.0
                } else {
                    (used as f64 / budget as f64).min(1.0)
                };
                ui.status.cache_bar.set_fraction(fraction);
                let offline = match pinned {
                    1 => "1 offline item".to_string(),
                    count => format!("{count} offline items"),
                };
                ui.status.cache_label.set_text(&if budget == 0 {
                    format!("{offline} · {} used · no cache limit", human_bytes(used))
                } else {
                    format!(
                        "{offline} · {} of {} used",
                        human_bytes(used),
                        human_bytes(budget)
                    )
                });
            }
            // Daemon unreachable (still starting, or down): report not-mounted
            // but leave the last-known cache read-out so the page does not
            // flicker on a transient failure.
            _ => {
                set_mounted(&ui, false);
                ui.status.mount_row.set_subtitle("Not connected");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{quota_display, quota_status_display};

    #[test]
    fn quota_display_reports_used_total_and_percentage() {
        let gib = 1024_i64.pow(3);
        let (fraction, text) = quota_display(4 * gib, gib);
        assert!((fraction - 0.25).abs() < f64::EPSILON);
        assert_eq!(text, "1.0 GiB of 4.0 GiB used (25%)");
    }

    #[test]
    fn quota_display_clamps_bad_api_values() {
        assert_eq!(quota_display(0, -1), (0.0, "0 B used".to_string()));
        assert_eq!(quota_display(100, 150).0, 1.0);
    }

    #[test]
    fn quota_status_display_matches_dolphin_wording() {
        let gib = 1024_i64.pow(3);
        assert_eq!(
            quota_status_display(4 * gib, gib),
            Some((
                0.25,
                "3.0 GiB free".to_string(),
                "3.0 GiB free out of 4.0 GiB (25% used)".to_string()
            ))
        );
        assert_eq!(quota_status_display(0, 0), None);
    }
}
