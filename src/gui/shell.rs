//! Pieces of the window that the Linux and the native front ends share: the
//! toast, the sidebar, the page frame, artwork tiles, key shortcuts and the
//! cards both windows draw the same way.

use super::{
    artwork::{Cache, Source},
    surface, theme,
};
use humansize::{DECIMAL, format_size};
use iced::keyboard::{Key, Modifiers, key::Named};
use iced::widget::{
    Space, button, column, container, image, row, scrollable, stack, text, tooltip,
};
use iced::{Alignment, Animation, Element, Length, Task, animation::Easing};
use std::time::{Duration, Instant};

/// The easing of every transition.
pub const EASING: Easing = Easing::EaseOutCubic;
/// The window width under which both windows switch to their compact form.
pub const COMPACT_BELOW: f32 = 880.0;
/// The size a window opens at.
pub const WINDOW_SIZE: (f32, f32) = (1100.0, 720.0);
/// The smallest window size, below which the compact layout cannot fit a row.
pub const MIN_WINDOW: (f32, f32) = (640.0, 480.0);
/// How long an informational toast stays.
pub const INFO_SECONDS: u64 = 4;
/// How long a toast showing the worker's refusal stays.
pub const REFUSAL_SECONDS: u64 = 8;

/// The symbols the sidebar draws for each destination.
pub mod icons {
    pub const OVERVIEW: &str = "⌂";
    pub const GAMES: &str = "◈";
    pub const JOBS: &str = "☷";
    pub const SETTINGS: &str = "⚙";
}

/// Runs blocking work without occupying iced's executor or window thread.
///
/// Each call starts its own thread. The error is returned when that thread
/// ends without sending a result.
pub async fn background<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    let (send, receive) = iced::futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let _sent = send.send(f());
    });
    receive
        .await
        .map_err(|_| "The background task stopped unexpectedly.".into())
}

/// The window's theme: `forced_dark` when the user chose one, else the
/// desktop's mode. Anything but a light desktop renders dark.
pub fn window_theme(system: iced::theme::Mode, forced_dark: Option<bool>) -> iced::Theme {
    theme::theme(forced_dark.unwrap_or(system != iced::theme::Mode::Light))
}

/// The text of the toast. An error stays until dismissed; anything else
/// leaves after four seconds.
#[derive(Debug, Clone)]
pub struct Status {
    pub is_error: bool,
    pub text: String,
}

impl Status {
    pub fn info(text: impl Into<String>) -> Self {
        Self {
            is_error: false,
            text: text.into(),
        }
    }
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            is_error: true,
            text: text.into(),
        }
    }
}

/// The toast: its text, when it leaves, and the animation that shows it.
pub struct Toast {
    pub status: Option<Status>,
    /// When the toast starts to hide. `None` for an error, which stays.
    pub deadline: Option<Instant>,
    pub reveal: Animation<bool>,
    /// The deadline a timer was last started for.
    pub scheduled: Option<Instant>,
}

impl Default for Toast {
    fn default() -> Self {
        Self {
            status: None,
            deadline: None,
            reveal: Animation::new(false),
            scheduled: None,
        }
    }
}

impl Toast {
    /// Replaces the toast and restarts its reveal, which takes `fade`.
    pub fn show(&mut self, status: Status, fade: Duration) {
        self.deadline =
            (!status.is_error).then(|| Instant::now() + Duration::from_secs(INFO_SECONDS));
        self.status = Some(status);
        self.reveal = Animation::new(false)
            .duration(fade)
            .easing(EASING)
            .go(true, Instant::now());
    }

    /// Shows the worker's refusal of something the user asked for. It leaves
    /// after a few seconds and is not a lost connection.
    pub fn show_refusal(&mut self, text: String, fade: Duration) {
        self.show(Status::error(text), fade);
        self.deadline = Some(Instant::now() + Duration::from_secs(REFUSAL_SECONDS));
    }

    /// Starts hiding the toast. With `instant` it goes at once. Otherwise it
    /// stays in `status` until `tick` sees the animation end.
    pub fn dismiss(&mut self, instant: bool) {
        self.deadline = None;
        if instant {
            self.status = None;
        } else {
            self.reveal.go_mut(false, Instant::now());
        }
    }

    /// Hides the toast at its deadline, then removes it once the hide
    /// animation has finished.
    pub fn tick(&mut self, instant: bool) {
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.dismiss(instant);
        }
        if self.status.is_some()
            && !self.reveal.value()
            && !self.reveal.is_animating(Instant::now())
        {
            self.status = None;
        }
    }

    /// A timer that sends `tick` when the toast is due to leave, started once
    /// per deadline. The window needs no frames for it, so a toast on screen
    /// does not rebuild the page sixty times a second.
    pub fn timer<M: Clone + Send + 'static>(&mut self, tick: M) -> Task<M> {
        let Some(deadline) = self.deadline else {
            return Task::none();
        };
        if self.scheduled == Some(deadline) {
            return Task::none();
        }
        self.scheduled = Some(deadline);
        let wait = deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(30);
        Task::perform(background(move || std::thread::sleep(wait)), move |_| {
            tick.clone()
        })
    }

    /// How far the toast has revealed, from 0.0 to 1.0.
    fn progress(&self, reduced: bool) -> f32 {
        if reduced {
            1.0
        } else {
            self.reveal.interpolate(0.0, 1.0, Instant::now())
        }
    }

    /// Whether the reveal is still moving.
    pub fn animating(&self) -> bool {
        self.reveal.is_animating(Instant::now())
    }
}

/// `base` with the toast stacked over its bottom right corner, so the toast
/// does not move the page. It fades in and moves 14 pixels into place. The
/// stack is there without a toast too, so widget state keeps its position.
pub fn with_toast<'a, M: Clone + 'a>(
    base: Element<'a, M>,
    toast: &'a Toast,
    reduced: bool,
    dismiss: M,
) -> Element<'a, M> {
    let Some(status) = &toast.status else {
        return stack([base, Space::new().into()])
            .width(Length::Fill)
            .height(Length::Fill)
            .into();
    };
    let reveal = toast.progress(reduced);
    let card = container(
        row![
            text("●").size(12).style(theme::toast_mark(status.is_error)),
            text(&status.text).size(14).width(Length::Fill),
            button(text("×").size(18))
                .style(button::text)
                .padding([3, 6])
                .on_press(dismiss)
        ]
        .spacing(12)
        .align_y(Alignment::Center),
    )
    .padding(14)
    .width(Length::Fill)
    .max_width(380)
    .style(theme::toast(status.is_error, reveal));
    let overlay = container(card)
        .padding(
            iced::Padding::default()
                .right(20)
                .bottom(20.0 + 14.0 * (1.0 - reveal)),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(Alignment::End)
        .align_y(Alignment::End);
    stack([base, overlay.into()])
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// The focus step Tab and Shift+Tab ask for, or `None` for any other key.
pub fn tab_focus<M: Send + 'static>(key: &Key, modifiers: Modifiers) -> Option<Task<M>> {
    (*key == Key::Named(Named::Tab)).then(|| {
        if modifiers.shift() {
            iced::widget::operation::focus_previous()
        } else {
            iced::widget::operation::focus_next()
        }
    })
}

/// A command-key shortcut both windows have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shortcut {
    /// Ctrl+F or Cmd+F: go to the game search.
    Search,
    /// Ctrl+R or Cmd+R: look for games again.
    Rescan,
}

/// The shortcut `key` with `modifiers` stands for, if any.
pub fn shortcut(key: &Key, modifiers: Modifiers) -> Option<Shortcut> {
    let Key::Character(letter) = key else {
        return None;
    };
    if !modifiers.command() {
        return None;
    }
    match letter.as_str() {
        "f" => Some(Shortcut::Search),
        "r" => Some(Shortcut::Rescan),
        _ => None,
    }
}

/// Starts a background decode for each source the cache hands out. A failed
/// decode is reported as `None`, which the cache records.
pub fn artwork_tasks<M: Send + 'static>(
    cache: &mut Cache,
    loaded: fn(Source, Option<image::Handle>) -> M,
) -> Task<M> {
    let mut tasks = vec![];
    while let Some(source) = cache.next() {
        let key = source.clone();
        tasks.push(Task::perform(
            background(move || source.decode().ok()),
            move |result| loaded(key.clone(), result.ok().flatten()),
        ));
    }
    Task::batch(tasks)
}

/// Which artwork tile to draw.
#[derive(Debug, Clone, Copy)]
pub enum Tile {
    /// The square tile at the left of a game row, `size` pixels each way.
    Row(u32),
    /// The tall 128 by 192 cover in the details.
    Cover,
}

/// A game's image, or a stand-in until one is loaded. The sensor asks for the
/// decode when the tile is first shown, and its key changes when the image is
/// evicted from the cache, which makes the sensor ask again.
pub fn artwork_tile<'a, M: Clone + 'a>(
    cache: &'a Cache,
    source: Option<&Source>,
    title: &str,
    tile: Tile,
    visible: fn(Source) -> M,
) -> Element<'a, M> {
    let cached = source.and_then(|source| cache.get(source));
    let has_image = cached.is_some();
    let drawn: Element<'a, M> = match (tile, cached) {
        (Tile::Row(side), Some(handle)) => image(handle.clone())
            .width(side)
            .height(side)
            .content_fit(iced::ContentFit::Cover)
            .into(),
        (Tile::Row(side), None) => {
            container(text(title.chars().next().unwrap_or('F').to_string()).size(23))
                .center(side)
                .style(theme::panel)
                .into()
        }
        (Tile::Cover, Some(handle)) => image(handle.clone())
            .width(128)
            .height(192)
            .content_fit(iced::ContentFit::Contain)
            .into(),
        (Tile::Cover, None) => container(theme::muted("Local artwork"))
            .center_x(128)
            .center_y(192)
            .into(),
    };
    match source {
        Some(source) => {
            let wanted = source.clone();
            iced::widget::sensor(drawn)
                .key((source.clone(), has_image))
                .on_show(move |_| visible(wanted.clone()))
                .into()
        }
        None => drawn,
    }
}

/// One sidebar entry: an icon in a box of one width, so every label starts at
/// the same x, and the label unless `compact`, which shows it as a tooltip.
pub fn nav_entry<'a, M: Clone + 'a>(
    label: &'a str,
    icon: &'a str,
    highlight: f32,
    compact: bool,
    message: M,
) -> Element<'a, M> {
    let icon = text(icon)
        .size(if compact { 20 } else { 18 })
        .style(theme::nav_icon(highlight));
    let content: Element<'a, M> = if compact {
        container(icon)
            .width(Length::Fill)
            .center_x(Length::Fill)
            .into()
    } else {
        row![container(icon).width(22).center_x(22), text(label).size(15)]
            .spacing(8)
            .align_y(Alignment::Center)
            .into()
    };
    let item = button(content)
        .width(Length::Fill)
        .padding(if compact { 11 } else { 12 })
        .style(theme::nav_button(highlight))
        .on_press(message);
    if compact {
        tooltip(item, label, tooltip::Position::Right).into()
    } else {
        item.into()
    }
}

/// The sidebar: the brand, the `top` entries, then `bottom` pinned to the
/// foot. `compact` shrinks it to icons.
pub fn sidebar<'a, M: 'a>(
    compact: bool,
    top: Vec<Element<'a, M>>,
    bottom: Element<'a, M>,
) -> Element<'a, M> {
    let brand: Element<'a, M> = if compact {
        container(text("F").size(24))
            .width(Length::Fill)
            .center_x(Length::Fill)
            .into()
    } else {
        text("Flummox").size(23).into()
    };
    let mut nav = column![brand, Space::new().height(16)]
        .spacing(6)
        .padding(if compact { 10 } else { 16 });
    for entry in top {
        nav = nav.push(entry);
    }
    nav = nav.push(Space::new().height(Length::Fill)).push(bottom);
    container(nav)
        .width(if compact { 72 } else { 208 })
        .height(Length::Fill)
        .style(theme::sidebar)
        .into()
}

/// The page inside its scrolling surface and gutter. `id` names the
/// scrollable that `scroll_to` addresses and under which the offset is
/// recorded.
pub fn page_surface<'a, M: 'a>(
    page: Element<'a, M>,
    id: &'static str,
    compact: bool,
    offset: f32,
    smooth: bool,
    positions: surface::Positions,
) -> Element<'a, M> {
    surface::tracked_surface(
        scrollable(
            container(page)
                .padding(theme::page_gutter(compact))
                .width(Length::Fill),
        )
        .id(iced::widget::Id::new(id))
        .style(theme::scrollable)
        .height(Length::Fill),
        offset,
        smooth,
        id,
        positions,
    )
}

/// The window frame: the sidebar beside the body, on the window background.
pub fn frame<'a, M: 'a>(sidebar: Element<'a, M>, body: Element<'a, M>) -> Element<'a, M> {
    container(row![sidebar, body])
        .width(Length::Fill)
        .height(Length::Fill)
        .style(theme::app_background)
        .into()
}

/// Bytes formatted in decimal units (kB, MB, GB).
pub fn size(bytes: u64) -> String {
    format_size(bytes, DECIMAL)
}

/// How many bytes the plan's volumes are short by, summed.
pub fn shortfall(plan: &crate::storage::SpacePlan) -> u64 {
    plan.requirements
        .iter()
        .map(|requirement| {
            requirement
                .additional
                .saturating_add(requirement.headroom)
                .saturating_sub(requirement.volume.available)
        })
        .fold(0, u64::saturating_add)
}

/// The review of a storage plan: what it is for, one line per volume with the
/// reasons, the retained-original note and the failure when the plan does not
/// fit. A failed plan is a card edged in the failure colour.
pub fn plan_review<'a, M: Clone + 'a>(
    heading: Option<String>,
    plan: &crate::storage::SpacePlan,
    again: M,
    cancel: M,
) -> Element<'a, M> {
    let mut review = column![theme::section_title("Storage plan")].spacing(8);
    if let Some(heading) = heading {
        review = review.push(theme::muted(heading));
    }
    for requirement in &plan.requirements {
        review = review.push(
            text(format!(
                "{}: {} needed including headroom · {} available",
                requirement.volume.path.display(),
                size(requirement.additional.saturating_add(requirement.headroom)),
                size(requirement.volume.available)
            ))
            .size(13),
        );
        review = review.push(theme::muted(requirement.reasons.join(" · ")));
    }
    if plan.retained_original {
        review = review.push(theme::muted(
            "The original is retained until you explicitly reclaim it.",
        ));
    }
    let failed = plan.check().is_err();
    if let Err(error) = plan.check() {
        let short = shortfall(plan);
        review = review.push(theme::danger_text(if short > 0 {
            format!(
                "Not enough free space. Free about {} and check again.",
                size(short)
            )
        } else {
            error.to_string()
        }));
    }
    review = review.push(
        row![
            theme::action("Check again", again),
            theme::secondary("Cancel", cancel)
        ]
        .spacing(8)
        .wrap(),
    );
    if failed {
        theme::attention_card(review, true)
    } else {
        theme::panel_card(review)
    }
}

/// "1 item needs attention" or "N items need attention".
pub fn attention_title(count: usize) -> String {
    if count == 1 {
        "1 item needs attention".into()
    } else {
        format!("{count} items need attention")
    }
}

/// "1 game" or "N games".
pub fn games_count(count: usize) -> String {
    format!("{count} game{}", if count == 1 { "" } else { "s" })
}

/// The card on Overview for discovery warnings and games that need a look.
/// `attention` counts the games, and `review` is the button that lists them.
pub fn attention_notes<'a, M: Clone + 'a>(
    attention: usize,
    warnings: &'a [String],
    review: Option<M>,
) -> Element<'a, M> {
    let mut notes = column![theme::section_text(attention_title(
        attention + warnings.len()
    ))]
    .spacing(4)
    .width(Length::Fill);
    for warning in warnings.iter().take(5) {
        notes = notes.push(theme::warning_text(warning));
    }
    if warnings.len() > 5 {
        notes = notes.push(theme::muted(format!(
            "and {} more",
            warnings.len().saturating_sub(5)
        )));
    }
    if attention > 0 {
        notes = notes.push(theme::muted("Review lists the games"));
    }
    let mut card = row![notes].spacing(12).align_y(Alignment::Center);
    if attention > 0
        && let Some(message) = review
    {
        card = card.push(theme::secondary("Review", message));
    }
    theme::attention_card(card, false)
}

/// The Games page when nothing is listed: a title, a hint, and the button
/// that gets the user out of it.
pub fn empty_panel<'a, M: Clone + 'a>(
    title: &'static str,
    hint: &'static str,
    way_out: Option<(&'static str, M)>,
) -> Element<'a, M> {
    let mut message = column![theme::section_text(title), theme::muted(hint)].spacing(10);
    if let Some((label, press)) = way_out {
        message = message.push(theme::secondary(label, press));
    }
    theme::panel_card(message)
}

/// The Jobs page groups with the line shown when a group is empty.
pub const JOB_GROUPS: [(&str, &str); 4] = [
    ("Running", "No jobs running"),
    ("Waiting", "No games waiting"),
    ("Needs attention", "No jobs need attention"),
    ("History", "Finished jobs will appear here"),
];

/// How many finished jobs the history lists.
pub const HISTORY_LIMIT: usize = 20;

/// One row of Settings > Appearance: a label with its description, and the
/// control at the right.
pub fn setting_row<'a, M: 'a>(
    label: &'a str,
    description: impl text::IntoFragment<'a>,
    control: impl Into<Element<'a, M>>,
) -> Element<'a, M> {
    row![
        column![text(label), theme::muted(description)]
            .spacing(3)
            .width(Length::Fill),
        control.into()
    ]
    .spacing(16)
    .align_y(Alignment::Center)
    .into()
}

/// The About card: the title, the version, what changed and where it is.
pub fn about_card<'a, M: Clone + 'a>(version_line: String, open_changelog: M) -> Element<'a, M> {
    theme::panel_card(
        column![
            theme::section_title("About Flummox"),
            theme::muted(version_line),
            theme::secondary("What changed", open_changelog),
            theme::muted(super::CHANGELOG_URL)
        ]
        .spacing(8),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Ctx, TestResult, check, check_eq, check_ne};

    #[test]
    fn info_leaves_after_four_seconds_and_an_error_stays() -> TestResult {
        let mut toast = Toast::default();
        toast.show(Status::info("done"), Duration::ZERO);
        let info = toast.deadline.ctx("an info toast has a deadline")?;
        let wait = info.saturating_duration_since(Instant::now());
        check(
            wait > Duration::from_secs(INFO_SECONDS - 1)
                && wait <= Duration::from_secs(INFO_SECONDS),
            format!("the deadline is about four seconds away, found {wait:?}"),
        )?;
        toast.show(Status::error("failed"), Duration::ZERO);
        check(toast.deadline.is_none(), "an error has no deadline")
    }

    #[test]
    fn a_refusal_leaves_after_eight_seconds() -> TestResult {
        let mut toast = Toast::default();
        toast.show_refusal("This game is excluded".into(), Duration::ZERO);
        check(
            toast.status.as_ref().is_some_and(|status| status.is_error),
            "a refusal is drawn as an error",
        )?;
        let deadline = toast.deadline.ctx("a refusal has a deadline")?;
        let wait = deadline.saturating_duration_since(Instant::now());
        check(
            wait > Duration::from_secs(REFUSAL_SECONDS - 1)
                && wait <= Duration::from_secs(REFUSAL_SECONDS),
            format!("the deadline is about eight seconds away, found {wait:?}"),
        )
    }

    #[test]
    fn dismissing_hides_at_once_with_reduced_motion_only() -> TestResult {
        let mut toast = Toast::default();
        toast.show(Status::error("failed"), Duration::from_millis(200));
        toast.dismiss(false);
        check(
            toast.status.is_some(),
            "with motion the text stays while it fades",
        )?;
        toast.dismiss(true);
        check(
            toast.status.is_none(),
            "with reduced motion it goes at once",
        )
    }

    #[test]
    fn a_tick_past_the_deadline_removes_the_toast() -> TestResult {
        let mut toast = Toast::default();
        toast.show(Status::info("done"), Duration::ZERO);
        toast.tick(true);
        check(
            toast.status.is_some(),
            "control: before the deadline the toast stays",
        )?;
        toast.deadline = Some(Instant::now());
        toast.tick(true);
        check(toast.status.is_none(), "after the deadline it is gone")
    }

    #[test]
    fn the_timer_starts_once_per_deadline() -> TestResult {
        let mut toast = Toast::default();
        let _none: Task<()> = toast.timer(());
        check(
            toast.scheduled.is_none(),
            "an error with no deadline starts none",
        )?;
        toast.show(Status::info("done"), Duration::ZERO);
        let _first: Task<()> = toast.timer(());
        check_eq(toast.scheduled, toast.deadline, "the first call records it")?;
        toast.show(Status::info("again"), Duration::ZERO);
        check_ne(
            toast.scheduled,
            toast.deadline,
            "a new toast has a deadline no timer was started for",
        )
    }

    #[test]
    fn the_theme_follows_the_desktop_unless_forced() -> TestResult {
        use iced::theme::Mode;
        let dark = |theme: iced::Theme| theme.extended_palette().is_dark;
        check(dark(window_theme(Mode::Dark, None)), "dark desktop")?;
        check(!dark(window_theme(Mode::Light, None)), "light desktop")?;
        check(
            !dark(window_theme(Mode::Dark, Some(false))),
            "a forced light theme wins over a dark desktop",
        )?;
        check(
            dark(window_theme(Mode::Light, Some(true))),
            "a forced dark theme wins over a light desktop",
        )
    }

    #[test]
    fn shortcuts_need_the_command_key() -> TestResult {
        let find = Key::Character("f".into());
        let rescan = Key::Character("r".into());
        check_eq(
            shortcut(&find, Modifiers::COMMAND),
            Some(Shortcut::Search),
            "command F",
        )?;
        check_eq(
            shortcut(&rescan, Modifiers::COMMAND),
            Some(Shortcut::Rescan),
            "command R",
        )?;
        check_eq(shortcut(&find, Modifiers::empty()), None, "a plain F")?;
        check_eq(
            shortcut(&Key::Character("x".into()), Modifiers::COMMAND),
            None,
            "another letter",
        )
    }

    #[test]
    fn only_tab_moves_focus() -> TestResult {
        check(
            tab_focus::<()>(&Key::Named(Named::Tab), Modifiers::empty()).is_some(),
            "Tab",
        )?;
        check(
            tab_focus::<()>(&Key::Named(Named::Tab), Modifiers::SHIFT).is_some(),
            "Shift+Tab",
        )?;
        check(
            tab_focus::<()>(&Key::Named(Named::Escape), Modifiers::empty()).is_none(),
            "control: Escape does not",
        )
    }

    #[test]
    fn counts_agree_with_their_nouns() -> TestResult {
        check_eq(
            attention_title(1),
            "1 item needs attention".to_owned(),
            "one",
        )?;
        check_eq(
            attention_title(3),
            "3 items need attention".to_owned(),
            "many",
        )?;
        check_eq(games_count(1), "1 game".to_owned(), "one game")?;
        check_eq(games_count(0), "0 games".to_owned(), "no games")
    }

    #[test]
    fn a_plan_reports_how_far_short_it_is() -> TestResult {
        let requirement = |available| crate::storage::Requirement {
            volume: crate::storage::Volume {
                identity: "fixture".into(),
                path: "/Games".into(),
                available,
            },
            additional: 4_000,
            headroom: 200,
            reasons: vec![],
        };
        let plan = |available| crate::storage::SpacePlan {
            retained_original: false,
            requirements: vec![requirement(available)],
        };
        check_eq(shortfall(&plan(1_200)), 3_000, "needed minus available")?;
        check_eq(shortfall(&plan(10_000)), 0, "control: room to spare")
    }
}
