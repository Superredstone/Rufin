//! A podcasts source: its name and the RSS feeds it follows, one address per line.

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

struct Form {
    section: gtk::Box,
    name: adw::EntryRow,
    feeds: gtk::TextView,
    save: gtk::Button,
}

fn form(saved_name: &str, saved_feeds: &[String]) -> Form {
    let resource = "preferences/source/podcast_host.ui";
    let builder = crate::ui_resource::builder(resource);
    crate::objects!(builder, resource, {
        section: gtk::Box,
        name: adw::EntryRow,
        feeds: gtk::TextView,
        save: gtk::Button,
    });
    name.set_text(saved_name);
    feeds.buffer().set_text(&saved_feeds.join("\n"));
    Form {
        section,
        name,
        feeds,
        save,
    }
}

/// The addresses typed, one per line; Sources validates and de-duplicates them.
fn typed_feeds(view: &gtk::TextView) -> Vec<String> {
    let buffer = view.buffer();
    feed_lines(&buffer.text(&buffer.start_iter(), &buffer.end_iter(), false))
}

fn feed_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
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
        let ready_feeds = form.feeds.clone();
        *context.actions.borrow_mut() = Some(SetupActions {
            status: status.clone(),
            connect: form.save.clone(),
            ready: Rc::new(move || !typed_feeds(&ready_feeds).is_empty()),
        });
        connect_entry_row_activation(&form.name, &form.save);
        let source = shell.products.source.clone();
        let (name, feeds) = (form.name.clone(), form.feeds.clone());
        form.save.connect_clicked(move |save| {
            let feeds = typed_feeds(&feeds);
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
    connect_entry_row_activation(&form.name, &form.save);
    let source = shell.products.source.clone();
    let source_id = saved.source.id.clone();
    let weak_shell = Rc::downgrade(shell);
    let (name, feeds) = (form.name.clone(), form.feeds.clone());
    form.save.connect_clicked(move |button| {
        let (source, source_id) = (source.clone(), source_id.clone());
        let name = name.text().trim().to_owned();
        let feeds = typed_feeds(&feeds);
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
    #[test]
    fn feed_lines_skip_blank_lines_and_trim() {
        assert_eq!(
            super::feed_lines(" https://a.example/rss \n\n\thttps://b.example/feed\n  "),
            ["https://a.example/rss", "https://b.example/feed"]
        );
        assert!(super::feed_lines("\n \n").is_empty());
    }
}
