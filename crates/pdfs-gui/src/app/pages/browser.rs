use crate::activation::{DriveActivation, drive_activation, mounted_path, mounted_target_rel};
use crate::*;

pub(crate) struct BrowserState {
    // Files (browser) page.
    /// Shared model behind the grid and column views; repopulated per directory.
    pub(crate) model: gio::ListStore,
    pub(crate) back: gtk4::Button,
    /// Clickable breadcrumb trail (a button per path segment); rebuilt per load
    /// by [`repaint_crumb`] so each ancestor folder navigates on click.
    pub(crate) crumb: gtk4::Box,
    /// Swaps the Files content area between the grid/list views and the status
    /// page below; see [`browser_status`].
    pub(crate) content: gtk4::Stack,
    /// The currently selected browser presentation (grid or list).
    pub(crate) view_stack: gtk4::Stack,
    /// The Files empty/loading/error surface, shown in place of the views.
    pub(crate) status: adw::StatusPage,
    /// Sits in [`Self::status`]; shown when a load failed because the mount
    /// service is down (not merely starting); restarts the service and reloads.
    pub(crate) retry: gtk4::Button,
    /// Mountpoint-relative path the browser is showing (empty = root).
    pub(crate) path: RefCell<String>,
    /// Debounced full-text search box in the browser header.
    pub(crate) search: gtk4::SearchEntry,
    /// The folder-level actions, insensitive while the mount is down: without a
    /// daemon they can only fail, and a greyed button says so before the click.
    pub(crate) new_folder: gtk4::Button,
    pub(crate) upload: gtk4::Button,
    pub(crate) upload_folder: gtk4::Button,
    /// Starts a daemon-side recursive local-thumbnail build for [`Self::path`].
    pub(crate) build_thumbnails: gtk4::Button,
    pub(crate) thumbnail_build_row: gtk4::Box,
    pub(crate) thumbnail_progress: gtk4::ProgressBar,
    pub(crate) thumbnail_status: gtk4::Label,
    pub(crate) thumbnail_poll: RefCell<Option<glib::SourceId>>,
    pub(crate) thumbnail_build_running: Cell<bool>,
    /// Pending debounce timer for the search box; replaced on every keystroke so
    /// only the last pause actually fires a [`Request::Search`].
    pub(crate) search_source: RefCell<Option<glib::SourceId>>,
    /// Identity of the newest folder/search request. Paths and queries can be
    /// requested repeatedly, so comparing their text alone cannot reject an
    /// older response that finishes after a manual refresh.
    pub(crate) load_generation: Cell<u64>,
    /// Dolphin-style bottom status bar: current listing counts, the icon-grid
    /// zoom value, and Proton account storage usage.
    pub(crate) summary: gtk4::Label,
    pub(crate) zoom: gtk4::Scale,
    pub(crate) grid_thumbnail_size: Cell<i32>,
    /// Weak references to realised grid cells. Zoom resizes only these visible,
    /// recycled surfaces instead of invalidating the whole list model.
    pub(crate) grid_tiles: RefCell<Vec<(glib::WeakRef<gtk4::Overlay>, glib::WeakRef<gtk4::Label>)>>,
    pub(crate) quota_box: gtk4::Box,
    pub(crate) quota: gtk4::ProgressBar,
    pub(crate) quota_text: gtk4::Label,
    pub(crate) grid_selection: gtk4::SingleSelection,
    pub(crate) list_selection: gtk4::SingleSelection,
}

/// Idle pause after the last keystroke before a search query is sent, so typing
/// doesn't fire a request per character.
pub(crate) const SEARCH_DEBOUNCE: Duration = Duration::from_millis(250);

/// Cap on search hits requested from the daemon.
pub(crate) const SEARCH_LIMIT: usize = 200;

/// Browser grid thumbnail size controlled by the bottom zoom slider. The
/// default preserves the former fixed 72 px presentation.
pub(crate) const GRID_THUMB_MIN: i32 = 48;
pub(crate) const GRID_THUMB_MAX: i32 = 144;
pub(crate) const GRID_THUMB_DEFAULT: i32 = 72;
pub(crate) const GRID_THUMB_STEP: i32 = 8;

/// The Files page: a Nautilus-style file manager. A back/breadcrumb header with
/// a grid/list view toggle sits over a [`gtk4::Stack`] that swaps between an
/// **icon grid** ([`gtk4::GridView`]) and a **column list** ([`gtk4::ColumnView`]
/// with Name / Size / Modified columns). Both views are driven by one shared
/// [`gio::ListStore`] of [`BoxedAnyObject`]-wrapped [`DirEntry`]s, so a directory
/// load repopulates the model once and both views update.
///
/// The factories that render entries — and the columns — need the [`Ui`] handle
/// for activation and the right-click menu, so they're installed later in
/// [`wire_browser`]; this builder only assembles the empty widgets.
///
/// Empty / loading / error outcomes aren't a label under the header: the whole
/// content area swaps to a centred [`adw::StatusPage`] (see [`browser_status`]),
/// so "this folder is empty" and "the mount is down" read as first-class states
/// rather than a stray line above a blank grid.
pub(crate) struct BrowserWidgets {
    pub(crate) model: gio::ListStore,
    pub(crate) back: gtk4::Button,
    pub(crate) crumb: gtk4::Box,
    pub(crate) grid: gtk4::GridView,
    pub(crate) column_view: gtk4::ColumnView,
    /// Swaps the content area between the grid/list views and the status page.
    pub(crate) content: gtk4::Stack,
    pub(crate) view_stack: gtk4::Stack,
    /// The empty/loading/error surface shown in place of the views.
    pub(crate) status: adw::StatusPage,
    /// Sits in the status page; shown only when the mount service is down.
    pub(crate) retry: gtk4::Button,
    pub(crate) search: gtk4::SearchEntry,
    pub(crate) new_folder: gtk4::Button,
    pub(crate) upload: gtk4::Button,
    pub(crate) upload_folder: gtk4::Button,
    pub(crate) build_thumbnails: gtk4::Button,
    pub(crate) thumbnail_build_row: gtk4::Box,
    pub(crate) thumbnail_progress: gtk4::ProgressBar,
    pub(crate) thumbnail_status: gtk4::Label,
    pub(crate) summary: gtk4::Label,
    pub(crate) zoom: gtk4::Scale,
    pub(crate) quota_box: gtk4::Box,
    pub(crate) quota: gtk4::ProgressBar,
    pub(crate) quota_text: gtk4::Label,
    pub(crate) refresh: gtk4::Button,
    /// The two selection models, so keyboard actions can re-read the entry the
    /// user has highlighted in either presentation.
    pub(crate) grid_selection: gtk4::SingleSelection,
    pub(crate) list_selection: gtk4::SingleSelection,
}

pub(crate) fn build_browser_page() -> (gtk4::Widget, BrowserWidgets) {
    let model = gio::ListStore::new::<BoxedAnyObject>();

    let back = gtk4::Button::builder()
        .icon_name("go-previous-symbolic")
        .tooltip_text("Up one folder")
        .valign(gtk4::Align::Center)
        .sensitive(false)
        .build();
    back.add_css_class("flat");
    // Clickable breadcrumb trail; `repaint_crumb` fills it per load. Wrapped in a
    // horizontally-scrolling viewport so a deep path can't shove the search box
    // and view toggles off the right edge.
    let crumb = gtk4::Box::new(gtk4::Orientation::Horizontal, 2);
    crumb.set_valign(gtk4::Align::Center);
    let crumb_scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::External)
        .vscrollbar_policy(gtk4::PolicyType::Never)
        .hexpand(true)
        .child(&crumb)
        .build();

    // Folder-level actions: create a subfolder / upload a file into the current
    // directory. Wired in `wire_browser_actions`.
    let new_folder = gtk4::Button::builder()
        .icon_name("folder-new-symbolic")
        .tooltip_text("New folder")
        .valign(gtk4::Align::Center)
        .build();
    new_folder.add_css_class("flat");
    let upload = gtk4::Button::builder()
        .icon_name("pdfs-cloud-upload-symbolic")
        .tooltip_text("Upload files")
        .valign(gtk4::Align::Center)
        .build();
    upload.add_css_class("flat");
    let upload_folder = gtk4::Button::builder()
        .icon_name("pdfs-folder-upload-symbolic")
        .tooltip_text("Upload folder")
        .valign(gtk4::Align::Center)
        .build();
    upload_folder.add_css_class("flat");
    let build_thumbnails = gtk4::Button::builder()
        .icon_name("image-x-generic-symbolic")
        .tooltip_text("Build thumbnails in this folder and its subfolders")
        .valign(gtk4::Align::Center)
        .build();
    build_thumbnails.add_css_class("flat");

    // Linked grid/list toggle, top-right, Nautilus-style.
    let grid_toggle = gtk4::ToggleButton::builder()
        .icon_name("view-grid-symbolic")
        .tooltip_text("Grid view")
        .active(true)
        .build();
    let list_toggle = gtk4::ToggleButton::builder()
        .icon_name("view-list-symbolic")
        .tooltip_text("List view")
        .build();
    list_toggle.set_group(Some(&grid_toggle));
    let toggles = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    toggles.add_css_class("linked");
    toggles.append(&grid_toggle);
    toggles.append(&list_toggle);

    let search = gtk4::SearchEntry::builder()
        .placeholder_text("Search Drive")
        .valign(gtk4::Align::Center)
        .build();
    search.set_width_chars(18);

    let refresh = refresh_button();

    let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    header.append(&back);
    header.append(&crumb_scroll);
    header.append(&refresh);
    header.append(&build_thumbnails);
    header.append(&new_folder);
    header.append(&upload);
    header.append(&upload_folder);
    header.append(&search);
    header.append(&toggles);

    let thumbnail_status = gtk4::Label::builder()
        .halign(gtk4::Align::Start)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .build();
    thumbnail_status.add_css_class("caption");
    let thumbnail_progress = gtk4::ProgressBar::builder().hexpand(true).build();
    let thumbnail_build_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
    thumbnail_build_row.append(&thumbnail_status);
    thumbnail_build_row.append(&thumbnail_progress);
    thumbnail_build_row.set_visible(false);

    // Empty / loading / error surface, shown in place of the views.
    let retry = gtk4::Button::builder()
        .label("Retry")
        .halign(gtk4::Align::Center)
        .build();
    retry.add_css_class("pill");
    retry.add_css_class("suggested-action");
    retry.set_visible(false);
    let status = adw::StatusPage::builder()
        .icon_name("folder-symbolic")
        .vexpand(true)
        .child(&retry)
        .build();
    status.add_css_class("compact");

    // Icon grid.
    let grid_selection = gtk4::SingleSelection::builder()
        .model(&model)
        .autoselect(false)
        .can_unselect(true)
        .build();
    let grid = gtk4::GridView::builder()
        .model(&grid_selection)
        .min_columns(2)
        .max_columns(10)
        .build();
    grid.add_css_class("file-grid");
    let grid_scroll = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .child(&grid)
        .build();

    // Column list.
    let list_selection = gtk4::SingleSelection::builder()
        .model(&model)
        .autoselect(false)
        .can_unselect(true)
        .build();
    let column_view = gtk4::ColumnView::builder().model(&list_selection).build();
    column_view.add_css_class("data-table");
    let column_scroll = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .child(&column_view)
        .build();

    // Stack swapped by the toggle buttons.
    let view_stack = gtk4::Stack::new();
    view_stack.set_vexpand(true);
    view_stack.add_named(&grid_scroll, Some("grid"));
    view_stack.add_named(&column_scroll, Some("list"));
    let vs = view_stack.clone();
    grid_toggle.connect_toggled(move |b| {
        if b.is_active() {
            vs.set_visible_child_name("grid");
        }
    });
    let vs = view_stack.clone();
    list_toggle.connect_toggled(move |b| {
        if b.is_active() {
            vs.set_visible_child_name("list");
        }
    });

    // Outer stack: the views, or the status page when there's nothing to show.
    let content = gtk4::Stack::new();
    content.set_vexpand(true);
    content.set_transition_type(gtk4::StackTransitionType::Crossfade);
    content.add_named(&view_stack, Some("views"));
    content.add_named(&status, Some("status"));

    // Mirror DolphinStatusBar's full-width order: contextual text (stretch 1),
    // "Zoom:", its slider, then capacity information. Both visible troughs use
    // the same CSS dimensions; widgets are 4 px apart, with Dolphin's 6/0/2/0
    // outer margins.
    let summary = gtk4::Label::builder()
        .label("Loading…")
        .halign(gtk4::Align::Start)
        .valign(gtk4::Align::Center)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .hexpand(true)
        .build();
    let zoom_label = gtk4::Label::new(Some("Zoom:"));
    zoom_label.set_valign(gtk4::Align::Center);
    let zoom = gtk4::Scale::with_range(
        gtk4::Orientation::Horizontal,
        f64::from(GRID_THUMB_MIN),
        f64::from(GRID_THUMB_MAX),
        f64::from(GRID_THUMB_STEP),
    );
    zoom.set_value(f64::from(GRID_THUMB_DEFAULT));
    zoom.set_draw_value(false);
    zoom.set_valign(gtk4::Align::Center);
    zoom.set_tooltip_text(Some("Size: 72 pixels"));
    zoom.add_css_class("browser-status-meter");

    let quota = gtk4::ProgressBar::new();
    quota.set_hexpand(false);
    quota.set_valign(gtk4::Align::Center);
    quota.set_tooltip_text(Some("Proton account storage"));
    quota.add_css_class("browser-status-meter");
    let quota_text = gtk4::Label::builder()
        .label("Loading…")
        .halign(gtk4::Align::Start)
        .valign(gtk4::Align::Center)
        .margin_end(6)
        .tooltip_text("Proton account storage")
        .build();
    let quota_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
    quota_box.set_valign(gtk4::Align::Center);
    quota_box.append(&quota);
    quota_box.append(&quota_text);
    // Dolphin hides the capacity widget until its observer has real figures.
    quota_box.set_visible(false);

    let status_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
    status_bar.add_css_class("browser-statusbar");
    status_bar.set_margin_top(0);
    status_bar.set_margin_bottom(0);
    status_bar.set_margin_start(6);
    status_bar.set_margin_end(2);
    status_bar.append(&summary);
    status_bar.append(&zoom_label);
    status_bar.append(&zoom);
    status_bar.append(&quota_box);

    let zoom_label_for_view = zoom_label.clone();
    let zoom_for_view = zoom.clone();
    let content_for_view = content.clone();
    view_stack.connect_visible_child_name_notify(move |stack| {
        let visible = content_for_view.visible_child_name().as_deref() == Some("views")
            && stack.visible_child_name().as_deref() == Some("grid");
        zoom_label_for_view.set_visible(visible);
        zoom_for_view.set_visible(visible);
    });
    let zoom_label_for_content = zoom_label.clone();
    let zoom_for_content = zoom.clone();
    let view_for_content = view_stack.clone();
    content.connect_visible_child_name_notify(move |stack| {
        let visible = stack.visible_child_name().as_deref() == Some("views")
            && view_for_content.visible_child_name().as_deref() == Some("grid");
        zoom_label_for_content.set_visible(visible);
        zoom_for_content.set_visible(visible);
    });

    let body = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    body.set_vexpand(true);
    body.set_margin_top(12);
    body.set_margin_bottom(6);
    body.set_margin_start(12);
    body.set_margin_end(12);
    body.append(&header);
    body.append(&thumbnail_build_row);
    body.append(&content);

    let page = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    page.append(&body);
    page.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
    page.append(&status_bar);

    (
        page.upcast(),
        BrowserWidgets {
            model,
            back,
            crumb,
            grid,
            column_view,
            content,
            view_stack,
            status,
            retry,
            search,
            new_folder,
            upload,
            upload_folder,
            build_thumbnails,
            thumbnail_build_row,
            thumbnail_progress,
            thumbnail_status,
            summary,
            zoom,
            quota_box,
            quota,
            quota_text,
            refresh,
            grid_selection,
            list_selection,
        },
    )
}

/// Swap the Files content area to the status page, with a Retry button only when
/// the failure is one the user can act on.
pub(crate) fn browser_status(ui: &Rc<Ui>, icon: &str, title: &str, description: &str, retry: bool) {
    ui.browser.status.set_icon_name(Some(icon));
    ui.browser.status.set_title(title);
    ui.browser.status.set_description(Some(description));
    ui.browser.retry.set_visible(retry);
    ui.browser.content.set_visible_child_name("status");
}

/// Swap the Files content area back to the grid/list views.
pub(crate) fn browser_views(ui: &Rc<Ui>) {
    ui.browser.content.set_visible_child_name("views");
}

/// The entry highlighted in whichever browser view is on screen, if any.
pub(crate) fn selected_entry(ui: &Rc<Ui>) -> Option<DirEntry> {
    let selection = if ui.browser.view_stack.visible_child_name().as_deref() == Some("list") {
        &ui.browser.list_selection
    } else {
        &ui.browser.grid_selection
    };
    entry_at(selection.model().as_ref(), selection.selected())
}

/// Install the entry factories, columns, activation handlers and the back
/// button. Split out from [`build_browser_page`] because every renderer needs
/// the [`Ui`] handle to open entries and raise the context menu.
pub(crate) fn wire_browser(ui: &Rc<Ui>, grid: &gtk4::GridView, column_view: &gtk4::ColumnView) {
    // Back: pop one path segment and reload.
    let ui_back = ui.clone();
    ui.browser.back.clone().connect_clicked(move |_| {
        clear_browser_search(&ui_back);
        {
            let mut path = ui_back.browser.path.borrow_mut();
            *path = match path.rfind('/') {
                Some(i) => path[..i].to_string(),
                None => String::new(),
            };
        }
        load_browser(&ui_back);
    });

    // One menu is parented to the stable outer box instead of to a recycled
    // list item. Do not parent it directly to `content`: GTK 4.8's GtkStack
    // accessibility bookkeeping does not support unmanaged popover children
    // and crashes in `gtk_popover_popdown` after repeated menu activations.
    let context_host = ui
        .browser
        .content
        .parent()
        .expect("browser content must have its outer box");
    let context_menu = BrowserContextMenu::new(ui, &context_host);

    // Resize only realised grid cells. Emitting a whole-model `items_changed`
    // notification for every slider step made GTK repeatedly tear down and bind
    // selection/list state while the pointer was still moving, which could starve
    // the main loop on image-heavy folders.
    let zoom = ui.browser.zoom.clone();
    let ui_zoom = ui.clone();
    let grid_zoom = grid.clone();
    zoom.connect_value_changed(move |scale| {
        let size = (scale.value().round() as i32).clamp(GRID_THUMB_MIN, GRID_THUMB_MAX);
        scale.set_tooltip_text(Some(&if size == 1 {
            "Size: 1 pixel".to_string()
        } else {
            format!("Size: {size} pixels")
        }));
        if ui_zoom.browser.grid_thumbnail_size.replace(size) == size {
            return;
        }
        ui_zoom
            .browser
            .grid_tiles
            .borrow_mut()
            .retain(|(thumbnail_ref, label_ref)| {
                let (Some(thumbnail), Some(label)) = (thumbnail_ref.upgrade(), label_ref.upgrade())
                else {
                    return false;
                };
                resize_grid_tile(&thumbnail, &label, size);
                true
            });
        grid_zoom.queue_resize();
    });

    // Grid tiles: a big icon over an ellipsized name, with a right-click menu.
    let factory = gtk4::SignalListItemFactory::new();
    factory.connect_setup({
        let ui = ui.clone();
        let context_menu = context_menu.clone();
        move |_, item| {
            let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
            let size = ui.browser.grid_thumbnail_size.get();
            let thumbnail = file_thumbnail_widget(size, grid_fallback_size(size));
            // Put the badge inside the thumbnail surface. Wrapping both in
            // another overlay would align it to the whole grid cell instead.
            let badge = gtk4::Image::builder()
                .pixel_size(18)
                .halign(gtk4::Align::End)
                .valign(gtk4::Align::Start)
                .margin_top(2)
                .margin_end(2)
                .build();
            badge.add_css_class("file-badge");
            thumbnail.add_overlay(&badge);
            // `WordChar` rather than the default `Word`: a name with no spaces
            // offers no word-break opportunity, so word wrapping cannot break it
            // at all and the label asks for its full natural width instead —
            // one tile stretches to the width of the window and the grid
            // collapses to a single column. Allowing a mid-word break is what
            // keeps the two-line-then-ellipsis budget below enforceable for
            // *every* name rather than only the ones that happen to have spaces.
            let label = gtk4::Label::builder()
                .ellipsize(gtk4::pango::EllipsizeMode::End)
                .justify(gtk4::Justification::Center)
                .max_width_chars(13)
                .width_chars(13)
                .wrap(true)
                .wrap_mode(gtk4::pango::WrapMode::WordChar)
                .lines(2)
                .build();
            ui.browser
                .grid_tiles
                .borrow_mut()
                .push((thumbnail.downgrade(), label.downgrade()));
            let tile = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
            tile.add_css_class("file-tile");
            tile.append(&thumbnail);
            tile.append(&label);
            attach_context_menu(&ui, item, &tile, &context_menu);
            attach_drag(&ui, item, &tile);
            attach_drop(&ui, item, &tile);
            item.set_child(Some(&tile));
        }
    });
    factory.connect_bind({
        let ui = ui.clone();
        move |_, item| {
            let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
            let tile = item.child().and_downcast::<gtk4::Box>().unwrap();
            let thumbnail = tile.first_child().and_downcast::<gtk4::Overlay>().unwrap();
            let badge = thumbnail
                .last_child()
                .and_downcast::<gtk4::Image>()
                .unwrap();
            let label = thumbnail
                .next_sibling()
                .and_downcast::<gtk4::Label>()
                .unwrap();
            let obj = item.item().and_downcast::<BoxedAnyObject>().unwrap();
            let entry = obj.borrow::<DirEntry>();
            let size = ui.browser.grid_thumbnail_size.get();
            resize_grid_tile(&thumbnail, &label, size);
            bind_file_thumbnail(&ui, &thumbnail, &entry, false);
            label.set_label(&entry.name);
            // The tile shows at most two lines of it, so the full name has to be
            // reachable somehow.
            label.set_tooltip_text(Some(&entry.name));
            apply_badge(&badge, &entry);
        }
    });
    grid.set_factory(Some(&factory));

    let ui_grid = ui.clone();
    grid.connect_activate(move |grid, pos| {
        if let Some(entry) = entry_at(grid.model().as_ref(), pos) {
            activate_entry(&ui_grid, &entry);
        }
    });

    // Column list: Name (icon + label, right-clickable), Size, Modified.
    column_view.append_column(&name_column(ui, &context_menu));
    column_view.append_column(&text_column("Size", |e| {
        if e.is_dir {
            "—".to_string()
        } else {
            human_bytes(e.size)
        }
    }));
    column_view.append_column(&text_column("Modified", |e| format_modified(e.modified)));

    let ui_col = ui.clone();
    column_view.connect_activate(move |view, pos| {
        if let Some(entry) = entry_at(view.model().as_ref(), pos) {
            activate_entry(&ui_col, &entry);
        }
    });
}

fn resize_grid_tile(thumbnail: &gtk4::Overlay, label: &gtk4::Label, size: i32) {
    resize_file_thumbnail(thumbnail, size, grid_fallback_size(size));
    let name_width = (size / 6 + 1).clamp(8, 24);
    label.set_width_chars(name_width);
    label.set_max_width_chars(name_width);
}

fn grid_fallback_size(thumbnail_size: i32) -> i32 {
    (thumbnail_size * 8 / 9).clamp(24, thumbnail_size)
}

/// Build the Name column: a small thumbnail with its local-state badge overlaid,
/// followed by the name and the same right-click menu the grid tiles carry.
fn name_column(ui: &Rc<Ui>, context_menu: &Rc<BrowserContextMenu>) -> gtk4::ColumnViewColumn {
    let factory = gtk4::SignalListItemFactory::new();
    factory.connect_setup({
        let ui = ui.clone();
        let context_menu = context_menu.clone();
        move |_, item| {
            let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
            let thumbnail = file_thumbnail_widget(28, 16);
            let badge = gtk4::Image::builder()
                .pixel_size(14)
                .halign(gtk4::Align::End)
                .valign(gtk4::Align::Start)
                .build();
            badge.add_css_class("file-badge");
            thumbnail.add_overlay(&badge);
            // Ellipsized so the Name column can be *narrower* than its longest
            // name. Without it the label's minimum width is the whole string,
            // the column inherits that minimum, and one long name pushes Size
            // and Modified off the right edge of the window for every row.
            let label = gtk4::Label::builder()
                .halign(gtk4::Align::Start)
                .ellipsize(gtk4::pango::EllipsizeMode::End)
                .build();
            let cell = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            cell.append(&thumbnail);
            cell.append(&label);
            attach_context_menu(&ui, item, &cell, &context_menu);
            attach_drag(&ui, item, &cell);
            attach_drop(&ui, item, &cell);
            item.set_child(Some(&cell));
        }
    });
    factory.connect_bind({
        let ui = ui.clone();
        move |_, item| {
            let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
            let cell = item.child().and_downcast::<gtk4::Box>().unwrap();
            let thumbnail = cell.first_child().and_downcast::<gtk4::Overlay>().unwrap();
            let badge = thumbnail
                .last_child()
                .and_downcast::<gtk4::Image>()
                .unwrap();
            let label = thumbnail
                .next_sibling()
                .and_downcast::<gtk4::Label>()
                .unwrap();
            let obj = item.item().and_downcast::<BoxedAnyObject>().unwrap();
            let entry = obj.borrow::<DirEntry>();
            bind_file_thumbnail(&ui, &thumbnail, &entry, true);
            label.set_label(&entry.name);
            label.set_tooltip_text(Some(&entry.name));
            apply_badge(&badge, &entry);
        }
    });
    let column = gtk4::ColumnViewColumn::new(Some("Name"), Some(factory));
    column.set_expand(true);
    column
}

/// Build a trailing text column whose cell text is derived from each [`DirEntry`]
/// by `render`.
pub(crate) fn text_column(
    title: &str,
    render: impl Fn(&DirEntry) -> String + 'static,
) -> gtk4::ColumnViewColumn {
    let factory = gtk4::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
        let label = gtk4::Label::builder().halign(gtk4::Align::Start).build();
        label.add_css_class("dim-label");
        item.set_child(Some(&label));
    });
    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
        let label = item.child().and_downcast::<gtk4::Label>().unwrap();
        let obj = item.item().and_downcast::<BoxedAnyObject>().unwrap();
        let entry = obj.borrow::<DirEntry>();
        label.set_label(&render(&entry));
    });
    gtk4::ColumnViewColumn::new(Some(title), Some(factory))
}

#[derive(Clone, Copy)]
enum ContextCommand {
    Play,
    Open,
    Versions,
    Rename,
    Move,
    Share,
    Trash,
}

struct PendingContextCommand {
    command: ContextCommand,
    entry: DirEntry,
}

/// A single native menu shared by both browser views. Its parent is the stable
/// browser content widget, never a recycled row whose rebind could interrupt a
/// popover close and strand GTK's input grab.
struct BrowserContextMenu {
    popover: gtk4::PopoverMenu,
    parent: gtk4::Widget,
    primary: gio::Menu,
    current: Rc<RefCell<Option<DirEntry>>>,
    play: gio::SimpleAction,
    offline: gio::SimpleAction,
    versions: gio::SimpleAction,
}

impl BrowserContextMenu {
    fn new(ui: &Rc<Ui>, parent: &impl IsA<gtk4::Widget>) -> Rc<Self> {
        let primary = gio::Menu::new();
        primary.append_item(&context_menu_item(
            "Play (stream)",
            "context.play",
            Some("media-playback-start-symbolic"),
            true,
        ));
        primary.append_item(&context_menu_item(
            "Open",
            "context.open",
            Some("document-open-symbolic"),
            false,
        ));
        primary.append_item(&context_menu_item(
            "Offline copy",
            "context.offline",
            Some("non-starred-symbolic"),
            false,
        ));
        primary.append_item(&context_menu_item(
            "Versions…",
            "context.versions",
            Some("document-properties-symbolic"),
            true,
        ));

        let organise = gio::Menu::new();
        organise.append_item(&context_menu_item(
            "Rename…",
            "context.rename",
            Some("document-edit-symbolic"),
            false,
        ));
        organise.append_item(&context_menu_item(
            "Move…",
            "context.move",
            Some("folder-move-symbolic"),
            false,
        ));

        let sharing = gio::Menu::new();
        sharing.append_item(&context_menu_item(
            "Share…",
            "context.share",
            Some("emblem-shared-symbolic"),
            false,
        ));
        sharing.append_item(&context_menu_item(
            "Move to Trash",
            "context.trash",
            Some("user-trash-symbolic"),
            false,
        ));

        let model = gio::Menu::new();
        model.append_section(None, &primary);
        model.append_section(None, &organise);
        model.append_section(None, &sharing);

        let parent = parent.as_ref().clone();
        let popover = gtk4::PopoverMenu::from_model(Some(&model));
        popover.set_has_arrow(false);
        popover.set_position(gtk4::PositionType::Bottom);
        popover.set_parent(&parent);

        let current = Rc::new(RefCell::new(None::<DirEntry>));
        let pending = Rc::new(RefCell::new(None::<PendingContextCommand>));
        let actions = gio::SimpleActionGroup::new();

        let play = context_action("play", ContextCommand::Play, &current, &pending);
        let open = context_action("open", ContextCommand::Open, &current, &pending);
        // This is deliberately stateless. A stateful boolean action makes GTK
        // render a checkbox; the outline/filled star alone represents the state.
        let offline = offline_context_action("offline", ui, &current);
        let versions = context_action("versions", ContextCommand::Versions, &current, &pending);
        let rename = context_action("rename", ContextCommand::Rename, &current, &pending);
        let move_it = context_action("move", ContextCommand::Move, &current, &pending);
        let share = context_action("share", ContextCommand::Share, &current, &pending);
        let trash = context_action("trash", ContextCommand::Trash, &current, &pending);
        for action in [
            &play, &open, &offline, &versions, &rename, &move_it, &share, &trash,
        ] {
            actions.add_action(action);
        }
        parent.insert_action_group("context", Some(&actions));

        let ui_closed = ui.clone();
        popover.connect_closed(move |_| {
            let Some(pending) = pending.borrow_mut().take() else {
                return;
            };
            let ui = ui_closed.clone();
            glib::idle_add_local_once(move || run_context_command(&ui, pending));
        });

        show_native_menu_icons(popover.upcast_ref());

        Rc::new(Self {
            popover,
            parent,
            primary,
            current,
            play,
            offline,
            versions,
        })
    }

    fn popup(&self, ui: &Rc<Ui>, entry: DirEntry, anchor: &gtk4::Widget, x: f64, y: f64) {
        self.current.replace(Some(entry.clone()));

        let connected = *ui.mounted.borrow();
        let rel = entry_rel(ui, &entry);
        let changing = ui
            .offline_changing
            .borrow()
            .contains(&offline_identity(&entry, &rel));
        self.play
            .set_enabled(connected && is_streamable_media_entry(&entry));
        self.offline.set_enabled(connected && !changing);
        self.versions.set_enabled(connected && !entry.is_dir);
        // Keep one native row at all times. It remains visible (and turns gray)
        // when disconnected or while the request is running.
        let (offline_label, offline_icon) = offline_menu_presentation(entry.pinned);
        self.primary.remove(2);
        self.primary.insert_item(
            2,
            &context_menu_item(offline_label, "context.offline", Some(offline_icon), false),
        );

        let point = anchor
            .compute_point(
                &self.parent,
                &gtk4::graphene::Point::new(x as f32, y as f32),
            )
            .unwrap_or_else(|| gtk4::graphene::Point::new(x as f32, y as f32));
        self.popover
            .set_pointing_to(Some(&gtk4::gdk::Rectangle::new(
                point.x() as i32,
                point.y() as i32,
                1,
                1,
            )));
        self.popover.popup();
        show_native_menu_icons(self.popover.upcast_ref());
    }
}

/// Label and icon for the single offline menu row. Entry kind deliberately
/// does not participate: folders support the same recursive offline operation
/// as files.
fn offline_menu_presentation(pinned: bool) -> (&'static str, &'static str) {
    if pinned {
        ("Remove offline copy", "starred-symbolic")
    } else {
        ("Offline copy", "non-starred-symbolic")
    }
}

/// Add the shared native menu gesture to one recycled browser cell.
fn attach_context_menu(
    ui: &Rc<Ui>,
    item: &gtk4::ListItem,
    anchor: &gtk4::Box,
    context_menu: &Rc<BrowserContextMenu>,
) {
    let gesture = gtk4::GestureClick::new();
    gesture.set_button(gtk4::gdk::BUTTON_SECONDARY);
    let item = item.downgrade();
    let weak_anchor = anchor.downgrade();
    let context_menu = context_menu.clone();
    let ui = ui.clone();
    gesture.connect_pressed(move |_, _, x, y| {
        let Some(entry) = item.upgrade().and_then(|item| context_entry(&item)) else {
            return;
        };
        let Some(anchor) = weak_anchor.upgrade() else {
            return;
        };
        context_menu.popup(&ui, entry, anchor.upcast_ref(), x, y);
    });
    anchor.add_controller(gesture);
}

fn context_menu_item(
    label: &str,
    action: &str,
    icon: Option<&str>,
    hide_when_disabled: bool,
) -> gio::MenuItem {
    let item = gio::MenuItem::new(Some(label), Some(action));
    if let Some(icon) = icon {
        // Ordinary vertical GtkPopoverMenu sections consume the regular `icon`
        // attribute. GTK creates an image child for it but hides that image when
        // the model button also has text; `show_native_menu_icons` restores it.
        item.set_icon(&gio::ThemedIcon::new(icon));
    }
    if hide_when_disabled {
        item.set_attribute_value("hidden-when", Some(&"action-disabled".to_variant()));
    }
    item
}

fn show_native_menu_icons(root: &gtk4::Widget) {
    if root.type_().name() == "GtkModelButton" {
        let mut child = root.first_child();
        while let Some(widget) = child {
            if let Some(image) = widget.downcast_ref::<gtk4::Image>() {
                image.set_margin_end(8);
                image.set_visible(true);
            }
            child = widget.next_sibling();
        }
    }
    let mut child = root.first_child();
    while let Some(widget) = child {
        show_native_menu_icons(&widget);
        child = widget.next_sibling();
    }
}

/// Offline state is optimistic and visible immediately, without waiting for the
/// popover's close animation. The shared popover has a stable parent, so rebinding
/// the clicked browser row here cannot invalidate the menu.
fn offline_context_action(
    name: &str,
    ui: &Rc<Ui>,
    current: &Rc<RefCell<Option<DirEntry>>>,
) -> gio::SimpleAction {
    let action = gio::SimpleAction::new(name, None);
    let current = current.clone();
    let ui = ui.clone();
    action.connect_activate(move |_, _| {
        let Some(entry) = current.borrow().clone() else {
            return;
        };
        toggle_pin(&ui, &entry);
    });
    action
}

fn context_action(
    name: &str,
    command: ContextCommand,
    current: &Rc<RefCell<Option<DirEntry>>>,
    pending: &Rc<RefCell<Option<PendingContextCommand>>>,
) -> gio::SimpleAction {
    let action = gio::SimpleAction::new(name, None);
    let current = current.clone();
    let pending = pending.clone();
    action.connect_activate(move |_, _| {
        let Some(entry) = current.borrow().clone() else {
            return;
        };
        pending.replace(Some(PendingContextCommand { command, entry }));
    });
    action
}

fn run_context_command(ui: &Rc<Ui>, pending: PendingContextCommand) {
    let entry = pending.entry;
    match pending.command {
        ContextCommand::Play => stream_entry(ui, &entry),
        ContextCommand::Open if is_streamable_media_entry(&entry) => download_and_open(ui, &entry),
        ContextCommand::Open => activate_entry(ui, &entry),
        ContextCommand::Versions => open_versions_dialog(ui, &entry),
        ContextCommand::Rename => prompt_rename(ui, &entry),
        ContextCommand::Move => prompt_move(ui, &entry),
        ContextCommand::Share => open_share_dialog(ui, &entry),
        ContextCommand::Trash => prompt_delete(ui, &entry),
    }
}

fn context_entry(item: &gtk4::ListItem) -> Option<DirEntry> {
    let obj = item.item().and_downcast::<BoxedAnyObject>()?;
    let entry = obj.borrow::<DirEntry>().clone();
    Some(entry)
}
/// Fetch the [`DirEntry`] backing the model item at `pos`, if any.
pub(crate) fn entry_at(model: Option<&impl IsA<gio::ListModel>>, pos: u32) -> Option<DirEntry> {
    let obj = model?.item(pos).and_downcast::<BoxedAnyObject>()?;
    let entry = obj.borrow::<DirEntry>().clone();
    Some(entry)
}

/// Whether an entry can be opened through FUSE instead of fully materialized.
pub(crate) fn is_streamable_media_entry(entry: &DirEntry) -> bool {
    drive_activation(&entry.name, entry.is_dir) == DriveActivation::MountedMedia
}

/// Stream media straight from the mount, no download. Drive folders *are* part
/// of the FUSE mount, so a player pointed at `<mountpoint>/<rel>` reads the file
/// through the daemon's ranged reader — 4 MB blocks fetched on demand as it
/// seeks and buffers — instead of waiting for the whole file to land like
/// [`Request::OpenFile`] does. This is the point of the feature: a 2 GB HEVC
/// `.mkv` starts playing in seconds.
pub(crate) fn stream_entry(ui: &Rc<Ui>, entry: &DirEntry) {
    let rel = entry_rel(ui, entry);
    let mountpoint = ui.dirs.resolved_mountpoint(&ui.dirs.load_config());
    let abs = mounted_path(&mountpoint, &rel);
    let Some(path) = abs.to_str() else {
        toast_error(
            ui,
            "Couldn't play media",
            "The file path isn't valid UTF-8.",
        );
        return;
    };
    toast(ui, &format!("Streaming “{}”…", entry.name));
    play_external(path);
}

/// Open an entry the Nautilus way: folders descend, media streams from the mount,
/// other files download-and-open.
pub(crate) fn activate_entry(ui: &Rc<Ui>, entry: &DirEntry) {
    let rel = entry_rel(ui, entry);
    if entry.is_dir {
        // Descending into a search hit: clear the query so the folder listing
        // isn't immediately re-masked by a stale search.
        if !entry.path.is_empty() {
            clear_browser_search(ui);
        }
        *ui.browser.path.borrow_mut() = rel;
        load_browser(ui);
    } else if drive_activation(&entry.name, entry.is_dir) == DriveActivation::MountedMedia {
        // Media streams rather than downloads: that is exactly the "play it,
        // don't fetch the whole thing" behaviour this is for.
        stream_entry(ui, entry);
    } else {
        download_and_open(ui, entry);
    }
}

/// Hand a file to the user's default application: through the mount when it is
/// there, otherwise downloaded into the cache first. The open path behind both
/// a plain double-click and the context menu's "Open" (including for a video,
/// when the user wants their default application rather than the player).
///
/// The mount comes first because the cache blob is a *copy* keyed by content
/// hash — an application saving into it writes where Drive will never look, and
/// the cache may evict it afterwards. See
/// [`mounted_target`](crate::activation::mounted_target).
pub(crate) fn download_and_open(ui: &Rc<Ui>, entry: &DirEntry) {
    let rel = entry_rel(ui, entry);
    let mountpoint = ui.dirs.resolved_mountpoint(&ui.dirs.load_config());
    if let Some(path) = mounted_target_rel(&mountpoint, &rel) {
        open_path(&path.to_string_lossy());
        return;
    }
    // Ignore a repeat activation of a file already downloading, so an impatient
    // double-click doesn't kick off a second round-trip.
    if !ui.opening.borrow_mut().insert(rel.clone()) {
        return;
    }
    ui.busy_begin();
    let name = entry.name.clone();
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::OpenFile {
            path: rel.clone(),
            uid: Some(entry.uid.clone()),
        },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        ui.opening.borrow_mut().remove(&rel);
        match result {
            // A cache blob is named by content hash: match the open rules
            // against the Drive name the user clicked, not that path.
            Ok(Ok(Response::FilePath { path })) => open_named_path(&path, &name),
            Ok(Ok(Response::Error { message, kind })) => {
                toast_failure(&ui, "Couldn't open file", &message, kind)
            }
            _ => toast_error(
                &ui,
                "Couldn't open file",
                "The mount service didn't respond.",
            ),
        }
    });
}

/// Pin or unpin an entry through the daemon. The visible model is updated in
/// place before the request starts, so its badge changes immediately without a
/// folder reload or another network round trip.
pub(crate) fn toggle_pin(ui: &Rc<Ui>, entry: &DirEntry) {
    let rel = entry_rel(ui, entry);
    let identity = offline_identity(entry, &rel);
    if !ui.offline_changing.borrow_mut().insert(identity.clone()) {
        return;
    }
    let pinned = entry.pinned;
    let desired = !pinned;
    // Optimistic feedback also makes a newly opened context menu reflect the
    // request immediately. A failed request restores the exact prior state.
    set_browser_offline_state(
        ui,
        entry,
        desired,
        if desired { entry.cached } else { false },
    );
    let req = if entry.pinned {
        Request::Unpin { path: rel }
    } else {
        Request::Pin { path: rel }
    };
    let rx = spawn_request(ui.dirs.control_socket(), req);
    let name = entry.name.clone();
    let original = entry.clone();
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.offline_changing.borrow_mut().remove(&identity);
        match result {
            Ok(Ok(Response::Error { message, kind })) => {
                set_browser_offline_state(&ui, &original, original.pinned, original.cached);
                toast_failure(&ui, "Couldn't change offline state", &message, kind);
            }
            Ok(Ok(_)) => {
                // A successful pin has materialised the file; a successful
                // unpin evicts its local copy. Folders don't render a cache badge.
                set_browser_offline_state(&ui, &original, desired, desired);
                toast(
                    &ui,
                    &if pinned {
                        format!("“{name}” is no longer kept offline")
                    } else {
                        format!("“{name}” is now available offline")
                    },
                );
            }
            _ => {
                set_browser_offline_state(&ui, &original, original.pinned, original.cached);
                toast_error(
                    &ui,
                    "Couldn't change offline state",
                    "The mount service didn't respond.",
                );
            }
        }
    });
}

fn offline_identity(entry: &DirEntry, rel: &str) -> String {
    if entry.uid.is_empty() {
        rel.to_string()
    } else {
        entry.uid.clone()
    }
}

fn set_browser_offline_state(ui: &Rc<Ui>, target: &DirEntry, pinned: bool, cached: bool) {
    for position in 0..ui.browser.model.n_items() {
        let Some(obj) = ui
            .browser
            .model
            .item(position)
            .and_downcast::<BoxedAnyObject>()
        else {
            continue;
        };
        let matches = {
            let entry = obj.borrow::<DirEntry>();
            if target.uid.is_empty() {
                entry.uid.is_empty() && entry.name == target.name && entry.path == target.path
            } else {
                entry.uid == target.uid
            }
        };
        if !matches {
            continue;
        }
        let mut updated = obj.borrow::<DirEntry>().clone();
        updated.pinned = pinned;
        updated.cached = cached;
        obj.replace(updated);
        // The object identity did not change, so explicitly make the views
        // rebind this one row and repaint its badge.
        ui.browser.model.items_changed(position, 1, 1);
    }
}

/// Join the entry name onto the current browser directory to get its
/// mountpoint-relative path.
pub(crate) fn entry_rel(ui: &Rc<Ui>, entry: &DirEntry) -> String {
    // Search hits carry an absolute (mountpoint-relative) path since they can
    // live anywhere; plain listing entries derive it from the current folder.
    if !entry.path.is_empty() {
        return entry.path.clone();
    }
    let base = ui.browser.path.borrow();
    if base.is_empty() {
        entry.name.clone()
    } else {
        format!("{base}/{}", entry.name)
    }
}

/// Rebuild the clickable breadcrumb trail for the mountpoint-relative `path`. The
/// root is always present ("Proton Drive"); each segment becomes a flat button
/// that navigates to that ancestor, except the last (the current folder), shown
/// as a plain heading label.
pub(crate) fn repaint_crumb(ui: &Rc<Ui>, path: &str) {
    while let Some(child) = ui.browser.crumb.first_child() {
        ui.browser.crumb.remove(&child);
    }
    let segments: Vec<&str> = if path.is_empty() {
        Vec::new()
    } else {
        path.split('/').collect()
    };
    ui.browser
        .crumb
        .append(&crumb_node(ui, "Proton Drive", "", segments.is_empty()));
    let mut acc = String::new();
    for (i, seg) in segments.iter().enumerate() {
        let sep = gtk4::Label::new(Some("›"));
        sep.add_css_class("dim-label");
        ui.browser.crumb.append(&sep);
        acc = if acc.is_empty() {
            seg.to_string()
        } else {
            format!("{acc}/{seg}")
        };
        let current = i == segments.len() - 1;
        ui.browser.crumb.append(&crumb_node(ui, seg, &acc, current));
    }
}

/// One breadcrumb segment: a plain heading label for the current folder, or a
/// flat button that navigates to `target` (clearing any active search first).
pub(crate) fn crumb_node(ui: &Rc<Ui>, label: &str, target: &str, current: bool) -> gtk4::Widget {
    if current {
        let l = gtk4::Label::builder()
            .label(label)
            .ellipsize(gtk4::pango::EllipsizeMode::Start)
            .build();
        l.add_css_class("heading");
        return l.upcast();
    }
    let button = gtk4::Button::builder().label(label).build();
    button.add_css_class("flat");
    let ui = ui.clone();
    let target = target.to_string();
    button.connect_clicked(move |_| {
        clear_browser_search(&ui);
        *ui.browser.path.borrow_mut() = target.clone();
        load_browser(&ui);
    });
    button.upcast()
}

/// Wire the browser header's New-folder, Upload-files and Upload-folder buttons.
pub(crate) fn wire_browser_actions(
    ui: &Rc<Ui>,
    new_folder: &gtk4::Button,
    upload: &gtk4::Button,
    upload_folder: &gtk4::Button,
    build_thumbnails: &gtk4::Button,
) {
    let ui_nf = ui.clone();
    new_folder.connect_clicked(move |_| prompt_new_folder(&ui_nf));
    let ui_up = ui.clone();
    upload.connect_clicked(move |_| prompt_upload(&ui_up));
    let ui_uf = ui.clone();
    upload_folder.connect_clicked(move |_| prompt_upload_folder(&ui_uf));
    let ui_thumbs = ui.clone();
    build_thumbnails.connect_clicked(move |_| start_thumbnail_build(&ui_thumbs));
}

const THUMBNAIL_BUILD_POLL: Duration = Duration::from_millis(500);

/// Start the daemon's deliberate recursive job. This first abandons only the
/// opportunistic work for visible rows; the explicit build has a separate job
/// class and continues if the user navigates while watching its progress.
fn start_thumbnail_build(ui: &Rc<Ui>) {
    if !*ui.mounted.borrow() {
        toast_error(
            ui,
            "Couldn't build thumbnails",
            "Proton Drive isn't connected.",
        );
        return;
    }
    cancel_file_thumbnails(ui);
    ui.browser.thumbnail_build_running.set(true);
    ui.browser.build_thumbnails.set_sensitive(false);
    ui.browser.thumbnail_build_row.set_visible(true);
    ui.browser.thumbnail_progress.set_fraction(0.0);
    ui.browser
        .thumbnail_status
        .set_label("Starting thumbnail build…");

    let path = ui.browser.path.borrow().clone();
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::StartThumbnailBuild { path },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        match rx.recv().await {
            Ok(Ok(Response::ThumbnailBuild { status })) => {
                repaint_thumbnail_build(&ui, &status);
            }
            Ok(Ok(Response::Error { message, kind })) => {
                thumbnail_build_failed(&ui, &message);
                toast_failure(&ui, "Couldn't build thumbnails", &message, kind);
            }
            _ => {
                let message = "The mount service didn't respond.";
                thumbnail_build_failed(&ui, message);
                toast_error(&ui, "Couldn't build thumbnails", message);
            }
        }
    });
}

fn schedule_thumbnail_build_poll(ui: &Rc<Ui>) {
    if let Some(source) = ui.browser.thumbnail_poll.borrow_mut().take() {
        source.remove();
    }
    let ui_poll = ui.clone();
    let source = glib::timeout_add_local_once(THUMBNAIL_BUILD_POLL, move || {
        ui_poll.browser.thumbnail_poll.borrow_mut().take();
        let rx = spawn_request(ui_poll.dirs.control_socket(), Request::ThumbnailBuildStatus);
        let ui_result = ui_poll.clone();
        glib::spawn_future_local(async move {
            match rx.recv().await {
                Ok(Ok(Response::ThumbnailBuild { status })) => {
                    repaint_thumbnail_build(&ui_result, &status)
                }
                Ok(Ok(Response::Error { message, .. })) => {
                    thumbnail_build_failed(&ui_result, &message)
                }
                _ => thumbnail_build_failed(
                    &ui_result,
                    "The mount service stopped reporting thumbnail progress.",
                ),
            }
        });
    });
    *ui.browser.thumbnail_poll.borrow_mut() = Some(source);
}

fn repaint_thumbnail_build(ui: &Rc<Ui>, status: &ThumbnailBuildStatus) {
    ui.browser.thumbnail_build_row.set_visible(true);
    let root = if status.path.is_empty() {
        "Proton Drive"
    } else {
        &status.path
    };
    let text = if status.scanning {
        ui.browser.thumbnail_progress.pulse();
        format!(
            "Scanning {root}… {} folders, {} images",
            status.folders_scanned, status.images_found
        )
    } else if status.running {
        let fraction = if status.images_found == 0 {
            0.0
        } else {
            status.completed as f64 / status.images_found as f64
        };
        ui.browser
            .thumbnail_progress
            .set_fraction(fraction.clamp(0.0, 1.0));
        format!(
            "Building thumbnails in {root}… {} of {}",
            status.completed, status.images_found
        )
    } else {
        ui.browser.thumbnail_progress.set_fraction(1.0);
        let available = status.completed.saturating_sub(status.failed);
        match status.failed {
            0 => format!("Thumbnails ready for {available} images in {root}"),
            failed => {
                format!("Thumbnails ready for {available} images in {root}; {failed} unavailable")
            }
        }
    };
    let text = match status.message.as_deref() {
        Some(message) => format!("{text}. {message}"),
        None => text,
    };
    ui.browser.thumbnail_status.set_label(&text);
    ui.browser.thumbnail_status.set_tooltip_text(Some(&text));

    if status.running {
        ui.browser.thumbnail_build_running.set(true);
        ui.browser.build_thumbnails.set_sensitive(false);
        schedule_thumbnail_build_poll(ui);
    } else {
        ui.browser.thumbnail_build_running.set(false);
        ui.browser
            .build_thumbnails
            .set_sensitive(*ui.mounted.borrow());
        if ui.stack.visible_child_name().as_deref() == Some("browser") {
            reload_listing(ui);
        }
    }
}

fn thumbnail_build_failed(ui: &Rc<Ui>, message: &str) {
    ui.browser.thumbnail_build_running.set(false);
    ui.browser
        .build_thumbnails
        .set_sensitive(*ui.mounted.borrow());
    ui.browser.thumbnail_build_row.set_visible(true);
    ui.browser.thumbnail_progress.set_fraction(0.0);
    ui.browser.thumbnail_status.set_label(message);
    ui.browser.thumbnail_status.set_tooltip_text(Some(message));
}

/// Send a mutating request (rename / move / delete / mkdir / upload, or a trash
/// restore / purge) on a worker thread, then reload the listing it changed and
/// confirm with a toast, or report the daemon's error in one. `done` is the
/// past-tense confirmation ("Renamed to “x”"); `failed` names the attempt
/// ("Couldn't rename").
pub(crate) fn run_mutation(ui: &Rc<Ui>, req: Request, done: String, failed: &'static str) {
    if !*ui.mounted.borrow() {
        toast_error(ui, failed, "Proton Drive isn't connected.");
        return;
    }
    ui.busy_begin();
    let rx = spawn_request(ui.dirs.control_socket(), req);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        match result {
            Ok(Ok(Response::Ok { .. })) => {
                // The listing the mutation changed is stale now; reload it, then
                // confirm, so the toast lands over the updated view.
                ui.status.quota_checked_at.set(None);
                reload_listing(&ui);
                toast(&ui, &done);
            }
            Ok(Ok(Response::Error { message, kind })) => toast_failure(&ui, failed, &message, kind),
            _ => toast_error(&ui, failed, "The mount service didn't respond."),
        }
    });
}

/// Reload the listing a completed mutation invalidated: whichever of the two
/// listing pages is on screen, since that is the one the action was raised from.
pub(crate) fn reload_listing(ui: &Rc<Ui>) {
    match ui.stack.visible_child_name().as_deref() {
        Some("browser") => {
            let query = ui.browser.search.text().trim().to_string();
            if query.is_empty() {
                load_browser(ui);
            } else {
                run_search(ui, &query);
            }
        }
        Some("trash") => load_trash(ui),
        _ => {}
    }
}

/// Prompt for a new name and rename the entry through the daemon.
pub(crate) fn prompt_rename(ui: &Rc<Ui>, entry: &DirEntry) {
    let parent = ui_window(ui);
    let rel = entry_rel(ui, entry);
    let original = entry.name.clone();
    let dialog = adw::MessageDialog::builder()
        .heading("Rename")
        .body(format!("Rename “{original}”."))
        .build();
    let group = adw::PreferencesGroup::new();
    let row = adw::EntryRow::builder()
        .title("New name")
        .activates_default(true)
        .build();
    row.set_text(&original);
    group.add(&row);
    dialog.set_extra_child(Some(&group));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("confirm", "Rename");
    dialog.set_response_appearance("confirm", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("confirm"));
    dialog.set_close_response("cancel");

    let ui = ui.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp != "confirm" {
            return;
        }
        let new_name = row.text().trim().to_string();
        if new_name.is_empty() || new_name == original {
            return;
        }
        let done = format!("Renamed to “{new_name}”");
        run_mutation(
            &ui,
            Request::Rename {
                path: rel.clone(),
                new_name,
            },
            done,
            "Couldn't rename",
        );
    });
    dialog.set_transient_for(parent.as_ref());
    dialog.present();
}

/// Prompt for a destination folder (mountpoint-relative, empty = Drive root) and
/// move the entry there through the daemon.
pub(crate) fn prompt_move(ui: &Rc<Ui>, entry: &DirEntry) {
    let parent = ui_window(ui);
    let rel = entry_rel(ui, entry);
    let name = entry.name.clone();
    let dialog = adw::MessageDialog::builder()
        .heading("Move")
        .body(format!(
            "Move “{}” into another folder. Enter its path from the Drive root \
             (leave blank for the root).",
            entry.name
        ))
        .build();
    let group = adw::PreferencesGroup::new();
    let row = adw::EntryRow::builder()
        .title("Destination folder")
        .activates_default(true)
        .build();
    group.add(&row);
    dialog.set_extra_child(Some(&group));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("confirm", "Move");
    dialog.set_response_appearance("confirm", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("confirm"));
    dialog.set_close_response("cancel");

    let ui = ui.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp != "confirm" {
            return;
        }
        let new_parent = row.text().trim().trim_matches('/').to_string();
        let done = match new_parent.as_str() {
            "" => format!("Moved “{name}” to Proton Drive"),
            dest => format!("Moved “{name}” to “{dest}”"),
        };
        run_mutation(
            &ui,
            Request::Move {
                path: rel.clone(),
                new_parent,
            },
            done,
            "Couldn't move",
        );
    });
    dialog.set_transient_for(parent.as_ref());
    dialog.present();
}

/// Confirm and move the entry to Trash through the daemon.
pub(crate) fn prompt_delete(ui: &Rc<Ui>, entry: &DirEntry) {
    let win = ui_window(ui);
    let rel = entry_rel(ui, entry);
    let name = entry.name.clone();
    let dialog = adw::MessageDialog::builder()
        .heading("Move to Trash")
        .body(format!("Move “{}” to Trash?", entry.name))
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("trash", "Move to Trash");
    dialog.set_response_appearance("trash", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    let ui = ui.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp == "trash" {
            run_mutation(
                &ui,
                Request::Delete { path: rel.clone() },
                format!("Moved “{name}” to Trash"),
                "Couldn't move to Trash",
            );
        }
    });
    dialog.set_transient_for(win.as_ref());
    dialog.present();
}

/// Prompt for a folder name and create it under the current browser directory.
pub(crate) fn prompt_new_folder(ui: &Rc<Ui>) {
    let win = ui_window(ui);
    let parent = ui.browser.path.borrow().clone();
    let dialog = adw::MessageDialog::builder()
        .heading("New folder")
        .body("Create a folder in the current directory.")
        .build();
    let group = adw::PreferencesGroup::new();
    let row = adw::EntryRow::builder()
        .title("Folder name")
        .activates_default(true)
        .build();
    group.add(&row);
    dialog.set_extra_child(Some(&group));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("confirm", "Create");
    dialog.set_response_appearance("confirm", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("confirm"));
    dialog.set_close_response("cancel");

    let ui = ui.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp != "confirm" {
            return;
        }
        let name = row.text().trim().to_string();
        if name.is_empty() {
            return;
        }
        let done = format!("Created “{name}”");
        run_mutation(
            &ui,
            Request::CreateFolder {
                parent: parent.clone(),
                name,
            },
            done,
            "Couldn't create folder",
        );
    });
    dialog.set_transient_for(win.as_ref());
    dialog.present();
}

/// Pick one or more local files and upload them into the current browser
/// directory. The daemon streams them from disk itself, so nothing is read into
/// the GUI — even a large multi-file selection.
pub(crate) fn prompt_upload(ui: &Rc<Ui>) {
    let win = ui_window(ui);
    let ui = ui.clone();
    choose_files(win.as_ref(), "Upload Files", move |files| {
        let sources: Vec<String> = files
            .into_iter()
            .filter_map(|p| p.to_str().map(str::to_string))
            .collect();
        start_upload(&ui, sources);
    });
}

/// Pick a local folder and upload it — with its whole subtree — into the current
/// browser directory. The daemon recreates the directory structure remotely.
pub(crate) fn prompt_upload_folder(ui: &Rc<Ui>) {
    let win = ui_window(ui);
    let ui = ui.clone();
    choose_folder(win.as_ref(), "Upload Folder", move |path| {
        let Some(path) = path.to_str().map(str::to_string) else {
            return;
        };
        start_upload(&ui, vec![path]);
    });
}

/// Hand a set of local source paths to the daemon for background bulk upload.
/// The daemon acks at once and does the work off-socket, so we confirm the
/// hand-off with a toast; the Activity group then shows live progress and the
/// listing refreshes itself when the transfers finish (see [`repaint_transfers`]).
pub(crate) fn start_upload(ui: &Rc<Ui>, sources: Vec<String>) {
    if sources.is_empty() {
        return;
    }
    if !*ui.mounted.borrow() {
        toast_error(ui, "Couldn't upload", "Proton Drive isn't connected.");
        return;
    }
    let parent = ui.browser.path.borrow().clone();
    let n = sources.len();
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::UploadPaths { parent, sources },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        match rx.recv().await {
            Ok(Ok(Response::Ok { .. })) => {
                let what = if n == 1 {
                    "Uploading…".to_string()
                } else {
                    format!("Uploading {n} items…")
                };
                toast(&ui, &what);
            }
            Ok(Ok(Response::Error { message, kind })) => {
                toast_failure(&ui, "Couldn't upload", &message, kind)
            }
            _ => toast_error(&ui, "Couldn't upload", "The mount service didn't respond."),
        }
    });
}

/// The local-state badge for an entry: pinned files get a star, merely cached
/// files a check, and the ordinary cloud-only state gets no visual noise.
/// Folders carry no per-file cache state and are unbadged too.
pub(crate) fn badge_for(entry: &DirEntry) -> Option<(&'static str, &'static str)> {
    if entry.is_dir {
        return None;
    }
    if entry.pinned {
        Some(("starred-symbolic", "badge-pinned"))
    } else if entry.cached {
        Some(("emblem-ok-symbolic", "badge-cached"))
    } else {
        None
    }
}

/// Paint `badge` to reflect the entry's sync state (see [`badge_for`]). Clears any
/// prior colour class first, since list factories recycle cells.
pub(crate) fn apply_badge(badge: &gtk4::Image, entry: &DirEntry) {
    for class in ["badge-pinned", "badge-cached"] {
        badge.remove_css_class(class);
    }
    match badge_for(entry) {
        Some((icon, class)) => {
            badge.set_icon_name(Some(icon));
            badge.add_css_class(class);
            badge.set_visible(true);
        }
        None => badge.set_visible(false),
    }
}

/// Make a browser cell draggable, carrying the bound entry's mountpoint-relative
/// path as the drag payload. Reads the entry live at drag time (via the captured
/// [`gtk4::ListItem`]) so a recycled cell drags whatever it currently shows.
pub(crate) fn attach_drag(ui: &Rc<Ui>, item: &gtk4::ListItem, anchor: &gtk4::Box) {
    let source = gtk4::DragSource::new();
    source.set_actions(gtk4::gdk::DragAction::MOVE);
    let ui = ui.clone();
    let item = item.clone();
    source.connect_prepare(move |_, _, _| {
        let obj = item.item().and_downcast::<BoxedAnyObject>()?;
        let rel = entry_rel(&ui, &obj.borrow::<DirEntry>());
        Some(gtk4::gdk::ContentProvider::for_value(&glib::Value::from(
            rel.as_str(),
        )))
    });
    anchor.add_controller(source);
}

/// Make a browser cell a drop target: dropping a dragged path onto a *folder* cell
/// moves the source into it through the daemon. Drops onto files, onto the item
/// itself, or that would move a folder into its own subtree are rejected.
pub(crate) fn attach_drop(ui: &Rc<Ui>, item: &gtk4::ListItem, anchor: &gtk4::Box) {
    let target = gtk4::DropTarget::new(glib::types::Type::STRING, gtk4::gdk::DragAction::MOVE);
    let ui = ui.clone();
    let item = item.clone();
    target.connect_drop(move |_, value, _, _| {
        let Some(obj) = item.item().and_downcast::<BoxedAnyObject>() else {
            return false;
        };
        let dest = obj.borrow::<DirEntry>();
        if !dest.is_dir {
            return false;
        }
        let Ok(src) = value.get::<String>() else {
            return false;
        };
        let dest_path = entry_rel(&ui, &dest);
        // No-op onto self, and never move a folder into itself or a descendant.
        if src == dest_path || dest_path.starts_with(&format!("{src}/")) {
            return false;
        }
        let done = format!("Moved into “{}”", dest.name);
        run_mutation(
            &ui,
            Request::Move {
                path: src,
                new_parent: dest_path,
            },
            done,
            "Couldn't move",
        );
        true
    });
    anchor.add_controller(target);
}

/// Pick a freedesktop icon base name for an entry from its kind / extension.
/// Callers append `-symbolic` for the column view's small icons.
pub(crate) fn icon_base_for(entry: &DirEntry) -> &'static str {
    if entry.is_dir {
        return "folder";
    }
    let ext = entry
        .name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "avif" | "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" | "heic" | "heif"
        | "tif" | "tiff" | "arw" | "cr2" | "cr3" | "crw" | "dng" | "k25" | "kdc" | "mrw"
        | "nef" | "nrw" | "orf" | "pef" | "raf" | "rw" | "rw2" | "sr2" | "srf" | "x3f" => {
            "image-x-generic"
        }
        "mp4" | "mkv" | "mov" | "avi" | "webm" | "m4v" => "video-x-generic",
        "mp3" | "flac" | "wav" | "ogg" | "opus" | "m4a" => "audio-x-generic",
        "pdf" | "doc" | "docx" | "odt" => "x-office-document",
        "xls" | "xlsx" | "ods" | "csv" => "x-office-spreadsheet",
        "ppt" | "pptx" | "odp" => "x-office-presentation",
        "zip" | "tar" | "gz" | "xz" | "bz2" | "7z" | "rar" => "package-x-generic",
        _ => "text-x-generic",
    }
}

/// Format an epoch-seconds modification time as a short local date.
pub(crate) fn format_modified(secs: i64) -> String {
    match glib::DateTime::from_unix_local(secs) {
        Ok(dt) => dt
            .format("%-d %b %Y")
            .map(|s| s.to_string())
            .unwrap_or_default(),
        Err(_) => String::new(),
    }
}

/// Request the current browser directory from the daemon and repaint both views.
pub(crate) fn load_browser(ui: &Rc<Ui>) {
    cancel_file_thumbnails(ui);
    let generation = ui.browser.load_generation.get().wrapping_add(1);
    ui.browser.load_generation.set(generation);
    let path = ui.browser.path.borrow().clone();
    repaint_crumb(ui, &path);
    ui.browser.back.set_sensitive(!path.is_empty());
    ui.browser.summary.set_label("Loading…");

    // Drop the previous folder's rows up front: a slow reply must not leave stale
    // entries visible, where clicking one would open with a wrong relative path.
    ui.browser.model.remove_all();
    browser_status(
        ui,
        "folder-symbolic",
        "Loading…",
        "Reading this folder.",
        false,
    );

    ui.busy_begin();
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::ListDir { path: path.clone() },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        // The user may have navigated on while this folder was loading. A stale
        // out-of-order reply must not repaint rows for a folder we've left, or
        // the breadcrumb and the grid would disagree.
        if ui.browser.load_generation.get() != generation || *ui.browser.path.borrow() != path {
            return;
        }
        match result {
            Ok(Ok(Response::Entries { entries })) => repaint_browser(&ui, &entries),
            Ok(Ok(Response::Error { message, kind })) => browser_failed(&ui, &message, kind),
            Ok(Ok(_)) => browser_failed(
                &ui,
                "Unexpected reply from the mount service.",
                ErrorKind::Internal,
            ),
            Ok(Err(_)) | Err(_) => browser_unreachable(&ui),
        }
    });
}

/// Clear the model and show the daemon's error on the status page. Used for
/// in-band failures (a bad path, a permission error) — the mount is up, so Retry
/// (which restarts the service) wouldn't help and isn't offered.
pub(crate) fn browser_failed(ui: &Rc<Ui>, message: &str, kind: ErrorKind) {
    ui.browser.model.remove_all();
    ui.browser.summary.set_label("Folder unavailable");
    browser_status(
        ui,
        "dialog-warning-symbolic",
        error_headline(kind, "Couldn't open this folder"),
        message,
        // Offer Retry only where repeating the request could actually work.
        // A folder that is gone stays gone however many times it is asked for.
        kind.retryable(),
    );
}

/// The daemon didn't answer. Distinguish *still starting* (auto-retry, no
/// button) from *down* (actionable error + Retry), so a cold start self-heals
/// once the systemd mount comes up but a real failure stays visible.
pub(crate) fn browser_unreachable(ui: &Rc<Ui>) {
    if service::is_failed() || !service::is_active() {
        ui.browser.summary.set_label("Not connected");
        browser_status(
            ui,
            "network-offline-symbolic",
            "Not connected",
            "The Proton Drive mount service isn't running.",
            true,
        );
        return;
    }
    ui.browser.summary.set_label("Connecting…");
    browser_status(
        ui,
        "folder-remote-symbolic",
        "Connecting…",
        "Waiting for the Proton Drive mount service to come up.",
        false,
    );
    let ui = ui.clone();
    glib::timeout_add_local_once(CONNECT_RETRY_INTERVAL, move || {
        // Only keep polling while the Files page is the one on screen.
        if ui.stack.visible_child_name().as_deref() == Some("browser") {
            reload_listing(&ui);
        }
    });
}

/// Repopulate the shared model — folders first, then case-insensitive by name —
/// which refreshes both the grid and the column list.
pub(crate) fn repaint_browser(ui: &Rc<Ui>, entries: &[DirEntry]) {
    ui.browser.model.remove_all();
    ui.browser.summary.set_label(&listing_summary(
        entries.iter().filter(|entry| !entry.is_dir).count(),
        entries.iter().filter(|entry| entry.is_dir).count(),
        entries
            .iter()
            .filter(|entry| !entry.is_dir)
            .map(|entry| entry.size)
            .sum(),
    ));
    if entries.is_empty() {
        browser_status(
            ui,
            "folder-open-symbolic",
            "This folder is empty",
            "Upload a file or create a folder to get started.",
            false,
        );
        return;
    }
    browser_views(ui);

    let mut sorted = entries.to_vec();
    sorted.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    for entry in sorted {
        ui.browser.model.append(&BoxedAnyObject::new(entry));
    }
}

/// Wire the browser header's search box: debounce keystrokes, then either run a
/// search or — when cleared — restore the current directory listing.
pub(crate) fn wire_search(ui: &Rc<Ui>) {
    let ui_s = ui.clone();
    ui.browser.search.connect_search_changed(move |_| {
        // Replace any pending debounce so only the last keystroke's pause fires.
        if let Some(src) = ui_s.browser.search_source.borrow_mut().take() {
            src.remove();
        }
        let ui_t = ui_s.clone();
        let src = glib::timeout_add_local_once(SEARCH_DEBOUNCE, move || {
            ui_t.browser.search_source.borrow_mut().take();
            let query = ui_t.browser.search.text().trim().to_string();
            if query.is_empty() {
                load_browser(&ui_t);
            } else {
                run_search(&ui_t, &query);
            }
        });
        *ui_s.browser.search_source.borrow_mut() = Some(src);
    });
}

fn clear_browser_search(ui: &Rc<Ui>) {
    ui.browser.search.set_text("");
    if let Some(source) = ui.browser.search_source.borrow_mut().take() {
        source.remove();
    }
}

/// Send a [`Request::Search`] to the daemon and render the hits in the browser
/// views, reusing the same row model so click-to-open and pin work unchanged
/// (each hit carries its full path; see [`entry_rel`]).
pub(crate) fn run_search(ui: &Rc<Ui>, query: &str) {
    cancel_file_thumbnails(ui);
    let generation = ui.browser.load_generation.get().wrapping_add(1);
    ui.browser.load_generation.set(generation);
    ui.browser.model.remove_all();
    ui.browser.summary.set_label("Searching…");
    browser_status(
        ui,
        "system-search-symbolic",
        "Searching…",
        &format!("Looking for “{query}”."),
        false,
    );

    ui.busy_begin();
    let query = query.to_string();
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::Search {
            query: query.clone(),
            limit: SEARCH_LIMIT,
        },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.busy_end();
        // The box may have been cleared or typed past while the reply was in
        // flight; if the query no longer matches, a fresher load/search already
        // owns the model — drop this stale, possibly out-of-order result.
        if ui.browser.load_generation.get() != generation
            || ui.browser.search.text().trim() != query
        {
            return;
        }
        match result {
            Ok(Ok(Response::SearchResults { hits })) => repaint_search(&ui, &hits),
            Ok(Ok(Response::Error { message, kind })) => browser_failed(&ui, &message, kind),
            Ok(Ok(_)) => browser_failed(
                &ui,
                "Unexpected reply from the mount service.",
                ErrorKind::Internal,
            ),
            Ok(Err(_)) | Err(_) => browser_unreachable(&ui),
        }
    });
}

/// Repopulate the model with search hits — folders first, then by name — mapping
/// each [`SearchHit`] to a path-carrying [`DirEntry`] the existing renderers and
/// handlers already understand.
pub(crate) fn repaint_search(ui: &Rc<Ui>, hits: &[SearchHit]) {
    ui.browser.model.remove_all();
    let counts = listing_summary(
        hits.iter().filter(|hit| !hit.is_dir).count(),
        hits.iter().filter(|hit| hit.is_dir).count(),
        hits.iter()
            .filter(|hit| !hit.is_dir)
            .map(|hit| hit.size)
            .sum(),
    );
    ui.browser
        .summary
        .set_label(&format!("{counts} — search results"));
    if hits.is_empty() {
        browser_status(
            ui,
            "system-search-symbolic",
            "No matches",
            "No files or folders match that search.",
            false,
        );
        return;
    }
    browser_views(ui);

    let mut entries: Vec<DirEntry> = hits
        .iter()
        .map(|h| DirEntry {
            name: h.name.clone(),
            is_dir: h.is_dir,
            size: h.size,
            modified: h.modified,
            pinned: h.pinned,
            // Search hits don't carry cache state; the badge shows in listings.
            cached: false,
            uid: h.uid.clone(),
            path: h.path.clone(),
            role: String::new(),
        })
        .collect();
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    for entry in entries {
        ui.browser.model.append(&BoxedAnyObject::new(entry));
    }
}

fn listing_summary(files: usize, folders: usize, file_bytes: u64) -> String {
    let file_word = if files == 1 { "file" } else { "files" };
    let folder_word = if folders == 1 { "folder" } else { "folders" };
    match (folders, files) {
        (0, 0) => "0 folders, 0 files".to_string(),
        (_, 0) => format!("{folders} {folder_word}"),
        (0, _) => format!("{files} {file_word} ({})", human_bytes(file_bytes)),
        _ => format!(
            "{folders} {folder_word}, {files} {file_word} ({})",
            human_bytes(file_bytes)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{listing_summary, offline_menu_presentation};

    #[test]
    fn offline_menu_has_one_action_for_each_state() {
        assert_eq!(
            offline_menu_presentation(false),
            ("Offline copy", "non-starred-symbolic")
        );
        assert_eq!(
            offline_menu_presentation(true),
            ("Remove offline copy", "starred-symbolic")
        );
    }

    #[test]
    fn listing_summary_matches_dolphin_order_and_wording() {
        assert_eq!(listing_summary(0, 0, 0), "0 folders, 0 files");
        assert_eq!(listing_summary(0, 2, 0), "2 folders");
        assert_eq!(listing_summary(1, 0, 1024), "1 file (1.0 KiB)");
        assert_eq!(listing_summary(3, 1, 3072), "1 folder, 3 files (3.0 KiB)");
    }
}
