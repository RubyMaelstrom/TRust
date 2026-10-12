//! The page's context menu: browser chrome the secondary button or the
//! menu key opens over the page, as COMMAND is. The page decides only
//! whether it shows, through `contextmenu` (Pointer Events
//! #maybe-show-context-menu); Shift+secondary button bypasses the page.
//! Its items act on the browser, never on the DOM. Over a media element it
//! offers the external player (mpv), where HTML #attr-media-controls places
//! such user agent features ("the media element's context menu").
//!
//! Opening, highlighting and closing it repaint only the menu: the
//! composed page stays as it is (`DesktopApp::draw_partial`).

use super::*;

use trust::render::{ContextMenuGeometry, ContextMenuIcon, ContextMenuItem, paint_context_menu};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MenuAction {
    /// Play the media element the menu was opened on in mpv.
    OpenInMpv,
    /// Open the menu's link in a new background tab.
    OpenLinkInNewTab,
    /// Copy the menu's link address.
    CopyLink,
    Copy,
    Reload,
    OpenCommand,
    KillPage,
    AddBookmark,
    RemoveBookmark,
}

pub(super) struct PageContextMenu {
    pub(super) actions: Vec<MenuAction>,
    pub(super) items: Vec<ContextMenuItem>,
    pub(super) geometry: ContextMenuGeometry,
    pub(super) highlighted: Option<usize>,
    /// The item the pointer pressed; its release over it activates it.
    pub(super) pressed: Option<usize>,
    /// The page it acts on, as COMMAND names it.
    pub(super) title: String,
    /// The hyperlink the menu was opened on, and its address.
    pub(super) link: Option<(Link, String)>,
    /// The external playback target of the media element it was opened on.
    pub(super) media: Option<url::Url>,
}

/// COMMAND's name for the page: its scheme and host.
pub(super) fn endpoint_title(address: &str) -> String {
    match url::Url::parse(address) {
        Ok(url) => match url.host_str() {
            Some(host) => format!(
                "{}  /  {}{}",
                url.scheme().to_ascii_uppercase(),
                host,
                url.port()
                    .map_or_else(String::new, |port| format!(":{port}"))
            ),
            None => url.scheme().to_ascii_uppercase(),
        },
        Err(_) => String::from("TRust"),
    }
}

impl DesktopApp {
    /// Pages have a context menu; a Telnet session keeps the secondary
    /// button for the remote host, and a locked pointer is the page's.
    pub(super) fn context_menu_available(&self) -> bool {
        self.tab.terminal.is_none()
            && self.pointer_lock.is_none()
            && (self.tab.page_layout.is_some() || self.tab.protocol_page.is_some())
    }

    /// The page's bookmark address: its destination as bookmarks store it.
    pub(super) fn page_bookmark_address(&self) -> Option<String> {
        trust::bookmarks::canonical(self.tab.browser.current_page()?.target()).ok()
    }

    pub(super) fn bookmark_worker(&mut self) -> &trust::bookmarks::Worker {
        if self.bookmarks.is_none() {
            let proxy = self.event_proxy.clone();
            self.bookmarks = Some(trust::bookmarks::Worker::start(
                &self.runtime,
                move |completion| {
                    let _ = proxy.send_event(DesktopEvent::Bookmarks(completion));
                },
            ));
        }
        self.bookmarks.as_ref().unwrap()
    }

    /// Ask the bookmark store whether the page shown is bookmarked. The
    /// store is shared with other TRust instances, so the answer is asked
    /// for whenever the menu could show it.
    pub(super) fn refresh_page_bookmark(&mut self) {
        if let Some(address) = self.page_bookmark_address() {
            let _ = self
                .bookmark_worker()
                .submit(trust::bookmarks::Operation::Lookup(address));
        }
    }

    /// The bookmark store's answer about an address.
    pub(super) fn page_bookmark_answer(&mut self, address: String, ids: Vec<u64>) {
        self.tab.page_bookmark = Some((address, ids));
        if self.context_menu.is_some() {
            self.update_context_menu_items();
        }
    }

    fn page_bookmarked(&self) -> bool {
        let Some(address) = self.page_bookmark_address() else {
            return false;
        };
        self.tab
            .page_bookmark
            .as_ref()
            .is_some_and(|(saved, ids)| *saved == address && !ids.is_empty())
    }

    fn has_copyable_selection(&self) -> bool {
        self.tab
            .form_editor
            .as_ref()
            .and_then(TextEditor::selected_text)
            .is_some_and(|text| !text.is_empty())
            || self.tab.text_selection.selects()
    }

    /// The hyperlink at `at` (the keyboard's target when the menu key
    /// opened the menu), resolved as following it would.
    pub(super) fn hyperlink_at(&self, at: CssPoint, from_keyboard: bool) -> Option<(Link, String)> {
        let hit = if from_keyboard {
            self.tab.keyboard_target.clone()
        } else {
            self.scene.as_ref().and_then(|scene| scene.page_hit_at(at))
        }?;
        let link = match hit.link? {
            Link::JsClick { href, .. } if !href.is_empty() => self.dead_page_href(&href)?,
            link @ (Link::Http(_)
            | Link::Gemini(_)
            | Link::Gopher(_)
            | Link::Telnet { .. }
            | Link::OneShot(_)
            | Link::Dict(_)) => link,
            Link::External(target) if parse_telnet_target(&target).is_some() => {
                Link::External(target)
            }
            _ => return None,
        };
        let address = link.to_string();
        Some((link, address))
    }

    /// The media element at `at` (the keyboard's target when the menu key
    /// opened the menu): its external playback target. The pointer finds
    /// it under whatever its player layers over it.
    pub(super) fn media_at(&self, at: CssPoint, from_keyboard: bool) -> Option<url::Url> {
        let page = &self.tab.page_layout.as_ref()?.layout.paint;
        if from_keyboard {
            let node = self.tab.keyboard_target.as_ref()?.node;
            return page.media_targets.get(&node).cloned();
        }
        self.scene.as_ref()?.media_target_at(page, at)
    }

    fn context_menu_entries(&self, link: bool, media: bool) -> Vec<(MenuAction, ContextMenuItem)> {
        let item = |label: &str, shortcut, icon, enabled, separated, caution| ContextMenuItem {
            label: label.to_string(),
            shortcut,
            icon,
            enabled,
            separated,
            caution,
        };
        let mut entries = Vec::new();
        if media {
            entries.push((
                MenuAction::OpenInMpv,
                item(
                    "Open in mpv",
                    None,
                    ContextMenuIcon::ExternalPlayer,
                    true,
                    false,
                    false,
                ),
            ));
        }
        if link {
            entries.push((
                MenuAction::OpenLinkInNewTab,
                item(
                    "Open in New Tab",
                    None,
                    ContextMenuIcon::NewTab,
                    true,
                    media,
                    false,
                ),
            ));
            entries.push((
                MenuAction::CopyLink,
                item(
                    "Copy Link",
                    None,
                    ContextMenuIcon::Link,
                    self.clipboard.is_some(),
                    false,
                    false,
                ),
            ));
        }
        let copy = self.has_copyable_selection();
        if copy {
            entries.push((
                MenuAction::Copy,
                item(
                    "Copy",
                    Some("Ctrl+C"),
                    ContextMenuIcon::Copy,
                    true,
                    link || media,
                    false,
                ),
            ));
        }
        entries.push((
            MenuAction::Reload,
            item(
                "Reload",
                Some("Ctrl+R"),
                ContextMenuIcon::Reload,
                true,
                copy || link || media,
                false,
            ),
        ));
        entries.push((
            MenuAction::OpenCommand,
            item(
                "Open COMMAND",
                Some("Tab"),
                ContextMenuIcon::Command,
                true,
                false,
                false,
            ),
        ));
        entries.push((
            MenuAction::KillPage,
            item(
                "Kill Page",
                None,
                ContextMenuIcon::Kill,
                self.tab.browser.page_is_live(),
                false,
                true,
            ),
        ));
        let bookmarkable = self.page_bookmark_address().is_some();
        entries.push(if self.page_bookmarked() {
            (
                MenuAction::RemoveBookmark,
                item(
                    "Remove Bookmark",
                    None,
                    ContextMenuIcon::BookmarkRemove,
                    true,
                    true,
                    false,
                ),
            )
        } else {
            (
                MenuAction::AddBookmark,
                item(
                    "Add Bookmark",
                    Some("Ctrl+B"),
                    ContextMenuIcon::BookmarkAdd,
                    bookmarkable,
                    true,
                    false,
                ),
            )
        });
        entries
    }

    /// The secondary button's release or the menu key over a page: a live
    /// page decides by firing `contextmenu` (the menu shows when its answer
    /// comes back uncanceled); with Shift, or without a live page, the
    /// menu shows at once.
    pub(super) fn request_page_context_menu(&mut self, bypass: bool, from_keyboard: bool) {
        if !self.context_menu_available() {
            return;
        }
        let at = if from_keyboard {
            self.keyboard_menu_position()
        } else {
            self.pointer
        };
        if self.tab.browser.page_is_live() && self.tab.page_layout.is_some() && !bypass {
            self.context_menu_request = Some((at, from_keyboard));
        } else {
            self.open_context_menu(at, from_keyboard);
        }
    }

    /// Where a keyboard-opened menu shows: at the keyboard's target, else
    /// at the pointer when it is over the page, else near the page's start.
    fn keyboard_menu_position(&self) -> CssPoint {
        let Some(scene) = &self.scene else {
            return self.pointer;
        };
        let content = scene.content_viewport;
        if let Some(target) = &self.tab.keyboard_target {
            return CssPoint::new(target.rect.x + 8.0, target.rect.y + target.rect.height);
        }
        if content.contains(self.pointer) {
            return self.pointer;
        }
        CssPoint::new(content.x + 24.0, content.y + 24.0)
    }

    /// The live page's answers to context menu requests.
    pub(super) fn sync_context_menu_requests(&mut self) {
        while let Some(show) = self.tab.browser.take_context_menu_result() {
            if let Some((at, from_keyboard)) = self.context_menu_request.take()
                && show
            {
                self.open_context_menu(at, from_keyboard);
            }
        }
    }

    /// Show the menu at `at` (window CSS pixels). From the keyboard its
    /// first item is highlighted, as a keyboard-opened menu is.
    pub(super) fn open_context_menu(&mut self, at: CssPoint, from_keyboard: bool) {
        if !self.context_menu_available() {
            return;
        }
        let Some(bounds) = self.scene.as_ref().map(|scene| scene.content_viewport) else {
            return;
        };
        self.close_select_popup();
        let link = self.hyperlink_at(at, from_keyboard);
        let media = self.media_at(at, from_keyboard);
        let (actions, items): (Vec<_>, Vec<_>) = self
            .context_menu_entries(link.is_some(), media.is_some())
            .into_iter()
            .unzip();
        let geometry = ContextMenuGeometry::new(at, bounds, &items);
        let highlighted = if from_keyboard {
            items.iter().position(|item| item.enabled)
        } else {
            geometry
                .item_at(self.pointer)
                .filter(|&index| items[index].enabled)
        };
        self.context_menu = Some(PageContextMenu {
            actions,
            items,
            geometry,
            highlighted,
            pressed: None,
            title: endpoint_title(&self.tab.browser.snapshot().address),
            link,
            media,
        });
        self.refresh_page_bookmark();
        self.apply_cursor_icon(CursorIcon::Default);
        self.request_overlay_redraw();
        self.update_accessibility(false);
    }

    /// Rebuild the open menu's items in place (the bookmark store answered).
    fn update_context_menu_items(&mut self) {
        let (link, media) = self.context_menu.as_ref().map_or((false, false), |menu| {
            (menu.link.is_some(), menu.media.is_some())
        });
        let (actions, items): (Vec<_>, Vec<_>) =
            self.context_menu_entries(link, media).into_iter().unzip();
        let Some(menu) = &mut self.context_menu else {
            return;
        };
        if menu.items == items {
            return;
        }
        let anchor = CssPoint::new(menu.geometry.panel.x, menu.geometry.panel.y);
        let bounds = self
            .scene
            .as_ref()
            .map_or(menu.geometry.panel, |scene| scene.content_viewport);
        menu.geometry = ContextMenuGeometry::new(anchor, bounds, &items);
        menu.actions = actions;
        menu.items = items;
        menu.highlighted = menu
            .highlighted
            .filter(|&index| menu.items.get(index).is_some_and(|item| item.enabled));
        self.request_overlay_redraw();
        self.update_accessibility(false);
    }

    pub(super) fn close_context_menu(&mut self) {
        if self.context_menu.take().is_some() {
            self.request_overlay_redraw();
            self.update_accessibility(false);
        }
    }

    /// Pointer motion while the menu shows highlights the enabled item
    /// under the pointer; the page sees no motion until it closes.
    pub(super) fn context_menu_pointer_moved(&mut self) -> bool {
        let Some(menu) = &mut self.context_menu else {
            return false;
        };
        let highlighted = menu
            .geometry
            .item_at(self.pointer)
            .filter(|&index| menu.items[index].enabled)
            .or(menu.highlighted.filter(|_| {
                // Leaving the menu keeps a keyboard highlight.
                !menu.geometry.panel.contains(self.pointer)
            }));
        if highlighted != menu.highlighted {
            menu.highlighted = highlighted;
            self.request_overlay_redraw();
            self.update_accessibility(false);
        }
        self.apply_cursor_icon(CursorIcon::Default);
        true
    }

    /// A button while the menu shows: an item's press and release over it
    /// activate it; a press outside closes the menu and is consumed with
    /// its release, as a platform menu's dismissal is.
    pub(super) fn context_menu_pointer_button(
        &mut self,
        state: ElementState,
        button: MouseButton,
    ) -> bool {
        if state == ElementState::Released && self.context_menu_swallow.take() == Some(button) {
            return true;
        }
        let Some(menu) = &mut self.context_menu else {
            return false;
        };
        let item = menu
            .geometry
            .item_at(self.pointer)
            .filter(|&index| menu.items[index].enabled);
        match state {
            ElementState::Pressed => {
                if menu.geometry.panel.contains(self.pointer) {
                    menu.pressed = item;
                } else {
                    self.context_menu_swallow = Some(button);
                    self.close_context_menu();
                }
            }
            ElementState::Released => {
                let pressed = menu.pressed.take();
                if let Some(index) = item.filter(|&index| pressed.is_none_or(|p| p == index)) {
                    self.activate_context_menu_item(index);
                }
            }
        }
        true
    }

    /// Keys while the menu shows: arrows, Home/End and Tab move the
    /// highlight among enabled items, Enter or Space activate it, Escape
    /// closes the menu. Every key belongs to the menu while it shows.
    pub(super) fn context_menu_key(&mut self, input: &KeyInput) -> bool {
        let Some(menu) = &mut self.context_menu else {
            return false;
        };
        if input.state != KeyState::Pressed {
            return true;
        }
        let enabled: Vec<usize> = (0..menu.items.len())
            .filter(|&index| menu.items[index].enabled)
            .collect();
        let at = menu
            .highlighted
            .and_then(|index| enabled.iter().position(|&e| e == index));
        let step = |forwards: bool| -> Option<usize> {
            if enabled.is_empty() {
                return None;
            }
            Some(
                enabled[match (at, forwards) {
                    (None, true) => 0,
                    (None, false) => enabled.len() - 1,
                    (Some(at), true) => (at + 1) % enabled.len(),
                    (Some(at), false) => (at + enabled.len() - 1) % enabled.len(),
                }],
            )
        };
        let next = match &input.key {
            Key::ArrowDown => step(true),
            Key::ArrowUp => step(false),
            Key::Tab => step(!input.modifiers.shift),
            Key::Home => enabled.first().copied(),
            Key::End => enabled.last().copied(),
            Key::Enter => {
                if let Some(index) = menu.highlighted {
                    self.activate_context_menu_item(index);
                }
                return true;
            }
            Key::Character(text) if text == " " => {
                if let Some(index) = menu.highlighted {
                    self.activate_context_menu_item(index);
                }
                return true;
            }
            Key::Escape => {
                self.close_context_menu();
                return true;
            }
            Key::Other(name) if name == "ContextMenu" => {
                self.close_context_menu();
                return true;
            }
            _ => return true,
        };
        if next != menu.highlighted {
            menu.highlighted = next;
            self.request_overlay_redraw();
            self.update_accessibility(false);
        }
        true
    }

    pub(super) fn activate_context_menu_item(&mut self, index: usize) {
        let Some((action, link, media)) = self
            .context_menu
            .as_ref()
            .filter(|menu| menu.items.get(index).is_some_and(|item| item.enabled))
            .and_then(|menu| {
                menu.actions
                    .get(index)
                    .map(|&action| (action, menu.link.clone(), menu.media.clone()))
            })
        else {
            return;
        };
        self.close_context_menu();
        match action {
            MenuAction::OpenInMpv => {
                if let Some(url) = media {
                    self.tab.browser.open_in_external_player(url);
                    self.launch_external_media_requests();
                }
            }
            MenuAction::OpenLinkInNewTab => {
                if let Some((link, _)) = link {
                    self.open_link_in_new_tab(link, false);
                }
            }
            MenuAction::CopyLink => {
                if let Some((_, address)) = link {
                    let copied = self
                        .clipboard
                        .as_mut()
                        .is_some_and(|clipboard| clipboard.set_text(address).is_ok());
                    self.tab.browser.set_status(if copied {
                        "Link copied"
                    } else {
                        "The clipboard is unavailable"
                    });
                }
            }
            MenuAction::Copy => self.copy(false),
            MenuAction::Reload => self.dispatch(UserAction::Reload),
            MenuAction::OpenCommand => self.open_command(true),
            MenuAction::KillPage => self.kill_current_page(),
            MenuAction::AddBookmark => self.bookmark_command("bookmark"),
            MenuAction::RemoveBookmark => {
                if let Some(address) = self.page_bookmark_address() {
                    let result = self
                        .bookmark_worker()
                        .submit(trust::bookmarks::Operation::RemoveUrl(address));
                    self.tab.browser.set_status(match result {
                        Ok(()) => "Updating bookmarks…".into(),
                        Err(error) => error,
                    });
                }
            }
        }
        self.request_redraw();
    }

    /// Paint the menu at the end of `scene`, above all other chrome, and
    /// note where it starts so an overlay-only repaint replaces just it.
    pub(super) fn paint_page_context_menu(&mut self, scene: &mut Scene) {
        self.overlay_start = scene.primitives.len();
        let menu = self.context_menu.as_ref().map(|menu| {
            paint_context_menu(
                &mut scene.primitives,
                &menu.geometry,
                &menu.items,
                menu.highlighted,
                &menu.title,
            );
            menu.geometry.damage()
        });
        // A page's question whether to leave it shows above everything.
        let prompt = self.paint_unload_prompt(&mut scene.primitives);
        self.overlay_damage = match (menu, prompt) {
            (Some(a), Some(b)) => Some(trust::render::union_css_rects(a, b)),
            (a, b) => a.or(b),
        };
    }

    /// Repaint only the overlay: the menu's commands at the end of the
    /// retained scene are replaced, and the raster updates the menu's
    /// rectangle alone. The page is neither recomposed nor relaid out.
    pub(super) fn request_overlay_redraw(&mut self) {
        self.overlay_redraw_pending = true;
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Replace the strip's commands in the retained `scene` with a fresh
    /// paint, keeping everything painted after it; returns the strip's
    /// rectangle.
    pub(super) fn repaint_strip_in(&mut self, scene: &mut Scene) -> CssRect {
        let old = self.strip_range.clone();
        let tail = scene.primitives.split_off(old.end);
        scene.primitives.truncate(old.start);
        self.paint_tab_strip(scene);
        let grown = self.strip_range.end as isize - old.end as isize;
        scene.primitives.extend(tail);
        self.overlay_start = (self.overlay_start as isize + grown) as usize;
        // Antialiased edges reach a pixel past the strip, as scene damage
        // bounds them.
        CssRect::new(0.0, 0.0, self.metrics.css.width, self.strip_height() + 2.0)
    }

    /// Repaint only the tab strip and/or the overlays over the retained
    /// scene: their commands are replaced in place, and the raster updates
    /// their rectangles alone. The page is neither recomposed nor relaid out.
    pub(super) fn draw_partial(&mut self, strip: bool, overlay: bool) -> Result<(), String> {
        let Some(mut scene) = self.scene.take() else {
            return self.draw();
        };
        if self.overlay_start > scene.primitives.len()
            || self.strip_range.end > self.overlay_start
            || self.strip_range.start > self.strip_range.end
        {
            self.scene = Some(scene);
            return self.draw();
        }
        let mut damage = None;
        if strip {
            damage = Some(self.repaint_strip_in(&mut scene));
        }
        if overlay {
            let previous = self.overlay_damage.take();
            scene.primitives.truncate(self.overlay_start);
            self.paint_page_context_menu(&mut scene);
            for rect in [previous, self.overlay_damage].into_iter().flatten() {
                damage = Some(match damage {
                    Some(damage) => trust::render::union_css_rects(damage, rect),
                    None => rect,
                });
            }
        }
        let presented = match damage {
            Some(rect) => self.present_scene(&scene, Some(rect)),
            None => Ok(true),
        };
        self.scene = Some(scene);
        match presented {
            Ok(presented) => {
                self.force_full_raster |= !presented;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }
}
