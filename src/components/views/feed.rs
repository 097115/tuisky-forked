use super::types::{Action, Data, Transition, View};
use super::utils::{profile_name, profile_name_as_str};
use super::ViewComponent;
use crate::backend::types::FeedDescriptor;
use crate::backend::{Watch, Watcher};
use bsky_sdk::api::app::bsky::feed::defs::{
    FeedViewPost, FeedViewPostReasonRefs, PostViewEmbedRefs, ReplyRefParentRefs,
};
use bsky_sdk::api::records::{KnownRecord, Record};
use bsky_sdk::api::types::Union;
use chrono::Local;
use color_eyre::Result;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, List, ListState, Padding, Paragraph};
use ratatui::Frame;
use std::sync::Arc;
use textwrap::Options;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

pub struct FeedViewComponent {
    items: Vec<FeedViewPost>,
    state: ListState,
    action_tx: UnboundedSender<Action>,
    descriptor: FeedDescriptor,
    watcher: Box<dyn Watch<Output = Vec<FeedViewPost>>>,
    quit: Option<oneshot::Sender<()>>,
}

impl FeedViewComponent {
    pub fn new(
        action_tx: UnboundedSender<Action>,
        watcher: Arc<Watcher>,
        descriptor: FeedDescriptor,
    ) -> Self {
        let watcher = Box::new(watcher.feed(descriptor.clone()));
        Self {
            items: Vec::new(),
            state: ListState::default(),
            action_tx,
            descriptor,
            watcher,
            quit: None,
        }
    }
    fn lines(feed_view_post: &FeedViewPost, area: Rect) -> Option<Vec<Line>> {
        let Record::Known(KnownRecord::AppBskyFeedPost(record)) = &feed_view_post.post.record
        else {
            return None;
        };
        let mut lines = Vec::new();
        {
            let mut spans = [
                vec![
                    Span::from(
                        feed_view_post
                            .post
                            .indexed_at
                            .as_ref()
                            .with_timezone(&Local)
                            .format("%Y-%m-%d %H:%M:%S %z")
                            .to_string(),
                    )
                    .green(),
                    Span::from(": "),
                ],
                profile_name(&feed_view_post.post.author),
            ]
            .concat();
            if let Some(labels) = feed_view_post
                .post
                .author
                .labels
                .as_ref()
                .filter(|v| !v.is_empty())
            {
                spans.push(Span::from(" "));
                spans.push(format!("[{} labels]", labels.len()).magenta());
            }
            lines.push(Line::from(spans));
        }
        if let Some(Union::Refs(FeedViewPostReasonRefs::ReasonRepost(repost))) =
            &feed_view_post.reason
        {
            lines.push(
                Line::from(format!("  Reposted by {}", profile_name_as_str(&repost.by))).blue(),
            );
        }
        if let Some(reply) = &feed_view_post.reply {
            if let Union::Refs(ReplyRefParentRefs::PostView(post_view)) = &reply.parent {
                lines.push(Line::from(
                    [
                        vec![Span::from("  Reply to ").blue()],
                        profile_name(&post_view.author),
                    ]
                    .concat(),
                ));
            }
        }
        lines.extend(
            textwrap::wrap(
                &record.text,
                Options::new(usize::from(area.width) - 2)
                    .initial_indent("  ")
                    .subsequent_indent("  "),
            )
            .iter()
            .map(|s| Line::from(s.to_string())),
        );
        if let Some(embed) = &feed_view_post.post.embed {
            let content = match embed {
                Union::Refs(PostViewEmbedRefs::AppBskyEmbedImagesView(images)) => {
                    format!("{} images", images.images.len())
                }
                Union::Refs(PostViewEmbedRefs::AppBskyEmbedExternalView(_)) => {
                    String::from("external")
                }
                Union::Refs(PostViewEmbedRefs::AppBskyEmbedRecordView(_)) => String::from("record"),
                Union::Refs(PostViewEmbedRefs::AppBskyEmbedRecordWithMediaView(_)) => {
                    String::from("recordWithMedia")
                }
                _ => String::from("unknown"),
            };
            lines.push(Line::from(format!("  Embedded {content}")).yellow());
        }
        lines.push(
            Line::from(format!(
                "   💬{:<4} 🔁{:<4} 🩷{:<4}",
                feed_view_post.post.reply_count.unwrap_or_default(),
                feed_view_post.post.repost_count.unwrap_or_default(),
                feed_view_post.post.like_count.unwrap_or_default()
            ))
            .dim(),
        );
        Some(lines)
    }
}

impl ViewComponent for FeedViewComponent {
    fn activate(&mut self) -> Result<()> {
        let (tx, mut rx) = (self.action_tx.clone(), self.watcher.subscribe());
        let (quit_tx, mut quit_rx) = oneshot::channel();
        self.quit = Some(quit_tx);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    changed = rx.changed() => {
                        match changed {
                            Ok(()) => {
                                if let Err(e) = tx.send(Action::Update(Box::new(Data::Feed(
                                    rx.borrow_and_update().clone(),
                                )))) {
                                    log::error!("failed to send update action: {e}");
                                }
                            }
                            Err(e) => {
                                log::warn!("changed channel error: {e}");
                                break;
                            }
                        }
                    }
                    _ = &mut quit_rx => {
                        break;
                    }
                }
            }
            log::debug!("subscription finished");
        });
        Ok(())
    }
    fn deactivate(&mut self) -> Result<()> {
        if let Some(tx) = self.quit.take() {
            if tx.send(()).is_err() {
                log::error!("failed to send quit signal");
            }
        }
        self.watcher.unsubscribe();
        Ok(())
    }
    fn update(&mut self, action: Action) -> Result<Option<Action>> {
        match action {
            Action::NextItem if !self.items.is_empty() => {
                self.state.select(Some(
                    self.state
                        .selected()
                        .map(|s| (s + 1).min(self.items.len() - 1))
                        .unwrap_or_default(),
                ));
                return Ok(Some(Action::Render));
            }
            Action::PrevItem if !self.items.is_empty() => {
                self.state.select(Some(
                    self.state
                        .selected()
                        .map(|s| s.max(1) - 1)
                        .unwrap_or_default(),
                ));
                return Ok(Some(Action::Render));
            }
            Action::Enter => {
                if let Some(feed_view_post) = self.state.selected().and_then(|i| self.items.get(i))
                {
                    return Ok(Some(Action::Transition(Transition::Push(Box::new(
                        View::Post(Box::new((
                            feed_view_post.post.clone(),
                            feed_view_post
                                .reply
                                .as_ref()
                                .and_then(|reply| match &reply.parent {
                                    Union::Refs(ReplyRefParentRefs::PostView(post_view)) => {
                                        Some(post_view.as_ref().clone())
                                    }
                                    _ => None,
                                }),
                        ))),
                    )))));
                }
            }
            Action::Back => return Ok(Some(Action::Transition(Transition::Pop))),
            Action::Refresh => {
                self.watcher.refresh();
            }
            Action::Update(data) => {
                let Data::Feed(feed) = data.as_ref() else {
                    return Ok(None);
                };
                log::debug!("update feed view: {}", feed.len());
                // TODO: update state.selected
                let select = if let Some(cid) = self
                    .state
                    .selected()
                    .and_then(|i| self.items.get(i))
                    .map(|feed_view_post| feed_view_post.post.cid.as_ref())
                {
                    feed.iter()
                        .position(|feed_view_post| feed_view_post.post.cid.as_ref() == cid)
                } else {
                    None
                };
                self.items.clone_from(feed);
                self.state.select(select);
                return Ok(Some(Action::Render));
            }
            _ => {}
        }
        Ok(None)
    }
    fn draw(&mut self, f: &mut Frame<'_>, area: Rect) -> Result<()> {
        let header = Paragraph::new(match &self.descriptor {
            FeedDescriptor::Feed(generator_view) => Line::from(vec![
                Span::from(generator_view.display_name.clone()).bold(),
                Span::from(" "),
                Span::from(format!(
                    "by {}",
                    profile_name_as_str(&generator_view.creator)
                ))
                .dim(),
            ]),
            FeedDescriptor::List => Line::from(""),
            FeedDescriptor::Timeline(value) => Line::from(value.as_str()),
        })
        .bold()
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Color::Gray)
                .padding(Padding::horizontal(1)),
        );
        let mut items = Vec::new();
        for feed_view_post in &self.items {
            if let Some(lines) = Self::lines(feed_view_post, area) {
                items.push(Text::from(lines));
            }
        }

        let layout =
            Layout::vertical([Constraint::Length(2), Constraint::Percentage(100)]).split(area);
        f.render_widget(header, layout[0]);
        f.render_stateful_widget(
            List::new(items)
                .highlight_style(Style::default().reset().reversed())
                .block(Block::default().padding(Padding::horizontal(1))),
            layout[1],
            &mut self.state,
        );
        Ok(())
    }
}
