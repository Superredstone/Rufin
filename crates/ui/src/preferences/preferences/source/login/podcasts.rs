//! A podcasts source: its name and the RSS feeds it follows. A feed address is typed in its
//! row and added with the row's apply button or Enter; each added feed has a remove button.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use localization::tr;
use rufin_core::runtime::source::EditableSource;
use sources::SourceSetup;

use super::{
    SetupActions, SetupViewContext, SourcePresentation, SourceSetupFlow, begin_connect_attempt,
    complete_source_save, connect_entry_row_activation, setup_scaffold,
};
use crate::shell::shell::Shell;
use crate::widgets::controls::REMOVE_ICON;

struct Form {
    section: gtk::Box,
    name: adw::EntryRow,
    address: adw::EntryRow,
    save: gtk::Button,
    list: Rc<FeedList>,
}

/// The feeds the form holds, drawn as the rows of its group.
struct FeedList {
    group: adw::PreferencesGroup,
    feeds: RefCell<Vec<String>>,
    rows: RefCell<Vec<adw::ActionRow>>,
    /// Runs after the list changes, so the form's button follows it.
    changed: RefCell<Option<Rc<dyn Fn()>>>,
}

impl FeedList {
    fn set(self: &Rc<Self>, feeds: Vec<String>) {
        for row in self.rows.take() {
            self.group.remove(&row);
        }
        self.group.set_visible(!feeds.is_empty());
        for feed in &feeds {
            let row = adw::ActionRow::builder()
                .title(feed)
                .use_markup(false)
                .title_selectable(true)
                .build();
            let remove = gtk::Button::from_icon_name(REMOVE_ICON);
            remove.add_css_class("flat");
            remove.set_valign(gtk::Align::Center);
            let label = tr("Remove feed");
            remove.set_tooltip_text(Some(&label));
            remove.update_property(&[gtk::accessible::Property::Label(&label)]);
            let list = Rc::downgrade(self);
            let removed = feed.clone();
            remove.connect_clicked(move |_| {
                if let Some(list) = list.upgrade() {
                    let mut feeds = list.feeds.borrow().clone();
                    feeds.retain(|feed| *feed != removed);
                    list.set(feeds);
                }
            });
            row.add_suffix(&remove);
            self.group.add(&row);
            self.rows.borrow_mut().push(row);
        }
        *self.feeds.borrow_mut() = feeds;
        let changed = self.changed.borrow().clone();
        if let Some(changed) = changed {
            changed();
        }
    }
}

fn form(saved_name: &str, saved_feeds: &[String]) -> Form {
    let resource = "preferences/source/podcast_host.ui";
    let builder = crate::ui_resource::builder(resource);
    crate::objects!(builder, resource, {
        section: gtk::Box,
        name: adw::EntryRow,
        address: adw::EntryRow,
        feeds: adw::PreferencesGroup,
        save: gtk::Button,
    });
    name.set_text(saved_name);
    crate::widgets::forms::attach_address_suggestions(&address);
    let list = Rc::new(FeedList {
        group: feeds,
        feeds: RefCell::new(Vec::new()),
        rows: RefCell::new(Vec::new()),
        changed: RefCell::new(None),
    });
    list.set(saved_feeds.to_vec());
    let added = Rc::downgrade(&list);
    address.connect_apply(move |address| {
        if let Some(list) = added.upgrade() {
            add_typed(address, &list);
        }
    });
    let typing = Rc::downgrade(&list);
    address.connect_text_notify(move |address| {
        address.remove_css_class("error");
        let changed = typing
            .upgrade()
            .and_then(|list| list.changed.borrow().clone());
        if let Some(changed) = changed {
            changed();
        }
    });
    Form {
        section,
        name,
        address,
        save,
        list,
    }
}

/// Whether `text` can be a feed address; Sources checks it fully when the source is saved.
fn web_address(text: &str) -> bool {
    let text = text.trim().to_ascii_lowercase();
    ["http://", "https://"].iter().any(|scheme| {
        text.strip_prefix(scheme)
            .is_some_and(|rest| !rest.is_empty())
    })
}

/// Moves the typed address into the list. False, marking the row, when it is no web address;
/// an empty row adds nothing and passes.
fn add_typed(address: &adw::EntryRow, list: &Rc<FeedList>) -> bool {
    let typed = address.text().trim().to_owned();
    if typed.is_empty() {
        return true;
    }
    if !web_address(&typed) {
        address.add_css_class("error");
        return false;
    }
    let mut feeds = list.feeds.borrow().clone();
    if !feeds.contains(&typed) {
        feeds.push(typed);
    }
    address.set_text("");
    list.set(feeds);
    true
}

/// The feeds to save, with an address still in its row: a pasted address and Add is enough.
fn feeds_to_save(
    address: &adw::EntryRow,
    list: &Rc<FeedList>,
    status: &gtk::Label,
) -> Option<Vec<String>> {
    if !add_typed(address, list) {
        status.set_text(&tr(
            "Enter a feed address starting with http:// or https://",
        ));
        status.add_css_class("error-text");
        status.set_visible(true);
        return None;
    }
    Some(list.feeds.borrow().clone())
}

pub(super) fn setup_flow(
    _shell: &Rc<Shell>,
    presentation: &'static SourcePresentation,
) -> Rc<dyn SourceSetupFlow> {
    Rc::new(Flow { presentation })
}

struct Flow {
    presentation: &'static SourcePresentation,
}

impl SourceSetupFlow for Flow {
    fn view(&self, shell: &Rc<Shell>, context: &SetupViewContext) -> gtk::Widget {
        let (scroller, content, actions, status) =
            setup_scaffold(shell, context, self.presentation);
        let form = form(&tr("Podcasts"), &[]);
        form.section.remove(&form.save);
        form.save.set_label(&tr("Add"));
        form.save.add_css_class("pill");
        content.append(&form.section);
        actions.append(&form.save);
        content.append(&actions);
        content.append(&status);

        let ready: Rc<dyn Fn() -> bool> = {
            let (list, address) = (Rc::downgrade(&form.list), form.address.downgrade());
            Rc::new(move || {
                list.upgrade()
                    .is_some_and(|list| !list.feeds.borrow().is_empty())
                    || address
                        .upgrade()
                        .is_some_and(|row| web_address(&row.text()))
            })
        };
        let (save, follow) = (form.save.downgrade(), Rc::clone(&ready));
        *form.list.changed.borrow_mut() = Some(Rc::new(move || {
            if let Some(save) = save.upgrade() {
                save.set_sensitive(follow());
            }
        }));
        form.save.set_sensitive(ready());
        *context.actions.borrow_mut() = Some(SetupActions {
            status: status.clone(),
            connect: form.save.clone(),
            ready,
        });
        connect_entry_row_activation(&form.name, &form.save);

        let source = shell.products.source.clone();
        let (name, address, list) = (
            form.name.clone(),
            form.address.clone(),
            Rc::clone(&form.list),
        );
        form.save.connect_clicked(move |save| {
            let Some(feeds) = feeds_to_save(&address, &list, &status) else {
                return;
            };
            if feeds.is_empty() {
                status.set_text(&tr("Add at least one podcast feed address"));
                status.set_visible(true);
                return;
            }
            begin_connect_attempt(&status, save, &tr("Reading podcast feeds..."));
            source.configure_source(SourceSetup::Podcasts {
                name: name.text().trim().to_owned(),
                feeds,
            });
        });
        scroller.upcast()
    }
}

pub(super) fn settings_group(
    shell: &Rc<Shell>,
    saved: &EditableSource,
    _presentation: &'static SourcePresentation,
) -> Result<gtk::Widget, String> {
    let form = form(&saved.source.name, &saved.podcast_feeds);
    let status = gtk::Label::new(None);
    status.set_xalign(0.0);
    status.set_wrap(true);
    status.set_visible(false);
    form.section
        .insert_child_after(&status, Some(&form.list.group));
    connect_entry_row_activation(&form.name, &form.save);
    let source = shell.products.source.clone();
    let source_id = saved.source.id.clone();
    let weak_shell = Rc::downgrade(shell);
    let (name, address, list) = (
        form.name.clone(),
        form.address.clone(),
        Rc::clone(&form.list),
    );
    form.save.connect_clicked(move |button| {
        let Some(feeds) = feeds_to_save(&address, &list, &status) else {
            return;
        };
        status.set_visible(false);
        let (source, source_id) = (source.clone(), source_id.clone());
        let name = name.text().trim().to_owned();
        if let Some(shell) = weak_shell.upgrade() {
            complete_source_save(&shell, button, async move {
                source
                    .save_podcasts(Some(source_id), name, feeds)
                    .await
                    .map_err(|error| error.to_string())
            });
        }
    });
    Ok(form.section.upcast())
}

#[cfg(test)]
mod tests {
    use super::web_address;

    #[test]
    fn web_addresses_need_a_scheme_and_more() {
        assert!(web_address("https://example.com/feed.xml"));
        assert!(web_address(" HTTP://example.com/rss "));
        assert!(!web_address("https://"));
        assert!(!web_address("example.com/feed"));
        assert!(!web_address("file:///feed.xml"));
        assert!(!web_address(""));
    }
}
