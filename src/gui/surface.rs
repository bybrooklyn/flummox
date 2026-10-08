//! Clipped page translation and wheel smoothing without momentum.
use iced::advanced::widget::{Operation, Tree, tree};
use iced::advanced::{Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer, widget};
use iced::{Element, Event, Length, Rectangle, Size, Vector};
use std::time::{Duration, Instant};

/// Where each page's scrollable is scrolled to, by scrollable id.
///
/// The surface wrapping a scrollable writes here on every frame. The
/// application reads it when it returns to a page. A message for each scroll
/// step would do the same job, but every message rebuilds the page, which made
/// an eased wheel step rebuild it on every frame.
pub type Positions = std::sync::Arc<std::sync::Mutex<std::collections::HashMap<&'static str, f32>>>;

/// The offset last recorded for the scrollable with this id, or the top.
pub fn recorded(positions: &Positions, key: &str) -> f32 {
    positions
        .lock()
        .ok()
        .and_then(|offsets| offsets.get(key).copied())
        .unwrap_or_default()
}

/// Wraps `content`, drawing it `offset` pixels lower and clipped to its own
/// bounds.
///
/// With `smooth`, line-based wheel steps over the first scrollable inside are
/// eased. `key` must be that scrollable's widget id, and a changed `key`
/// resets the easing state.
#[cfg(target_os = "linux")]
pub fn surface<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    offset: f32,
    smooth: bool,
    key: &'static str,
) -> Element<'a, Message> {
    Element::new(Surface {
        content: content.into(),
        offset,
        smooth,
        key,
        animating: false,
        positions: None,
    })
}
/// [`surface`], also recording the scrollable's offset under `key`.
pub fn tracked_surface<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    offset: f32,
    smooth: bool,
    key: &'static str,
    positions: Positions,
) -> Element<'a, Message> {
    Element::new(Surface {
        content: content.into(),
        offset,
        smooth,
        key,
        animating: false,
        positions: Some(positions),
    })
}
/// Requests future redraws while application animations are active.
pub fn animate<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    animating: bool,
) -> Element<'a, Message> {
    Element::new(Surface {
        content: content.into(),
        offset: 0.0,
        smooth: false,
        key: "animation-driver",
        animating,
        positions: None,
    })
}
/// The widget behind both `surface` and `animate`.
struct Surface<'a, Message> {
    content: Element<'a, Message>,
    /// Vertical translation of the content, in pixels.
    offset: f32,
    smooth: bool,
    key: &'static str,
    /// Ask for another frame after each redraw.
    animating: bool,
    /// Where to record the scrollable's offset, if anywhere.
    positions: Option<Positions>,
}
/// One eased wheel movement, kept in the widget tree between frames.
#[derive(Default)]
struct Motion {
    /// Scroll offset when the movement started or was last retargeted.
    from: f32,
    /// Scroll offset the movement ends at.
    target: f32,
    /// `None` while no movement is in progress.
    start: Option<Instant>,
    /// Where the pointer was at the wheel step. The synthetic scroll events
    /// are delivered there.
    cursor: Option<iced::Point>,
    key: &'static str,
}
impl Motion {
    /// The offset at `now`: ease-out cubic from `from` to `target` over 0.1 s.
    fn value(&self, now: Instant) -> f32 {
        let Some(start) = self.start else {
            return self.target;
        };
        let t = now.saturating_duration_since(start).as_secs_f32() / 0.1;
        self.from + (self.target - self.from) * (1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3))
    }
    /// Starts or extends a movement by `delta` pixels and restarts the clock.
    ///
    /// A step in the direction already travelling adds to the pending target,
    /// so quick steps accumulate. A first step or a reversal starts from
    /// `current`. The target is clamped to `0..=maximum`.
    fn retarget(&mut self, current: f32, delta: f32, maximum: f32, now: Instant) {
        let reversing = self.start.is_some() && delta.signum() != (self.target - current).signum();
        let base = if self.start.is_none() || reversing {
            current
        } else {
            self.target
        };
        self.from = current;
        self.target = (base + delta).clamp(0.0, maximum.max(0.0));
        self.start = Some(now);
    }
}
/// A widget operation that reads the first scrollable it meets: its vertical
/// offset, the largest offset it allows and its visible height.
#[derive(Default)]
struct Position {
    current: f32,
    maximum: f32,
    viewport: f32,
    found: bool,
}
impl Operation for Position {
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
        visit(self);
    }
    fn scrollable(
        &mut self,
        _: Option<&widget::Id>,
        bounds: Rectangle,
        content: Rectangle,
        translation: Vector,
        _: &mut dyn widget::operation::Scrollable,
    ) {
        if self.found {
            return;
        }
        self.found = true;
        self.current = translation.y;
        self.maximum = (content.height - bounds.height).max(0.0);
        self.viewport = bounds.height;
    }
}
impl<Message> Widget<Message, iced::Theme, iced::Renderer> for Surface<'_, Message> {
    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Motion>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(Motion {
            key: self.key,
            ..Default::default()
        })
    }
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(self.content.as_widget())]
    }
    fn diff(&self, tree: &mut Tree) {
        // A different key means different content, so a movement in progress
        // must not carry over to it.
        let motion = tree.state.downcast_mut::<Motion>();
        if motion.key != self.key {
            *motion = Motion {
                key: self.key,
                ..Default::default()
            };
        }
        tree.diff_children(std::slice::from_ref(&self.content));
    }
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let Some(child) = tree.children.first_mut() else {
            return layout::Node::new(Size::ZERO);
        };
        let node = self.content.as_widget_mut().layout(child, renderer, limits);
        layout::Node::with_children(
            node.size(),
            vec![node.translate(Vector::new(0.0, self.offset))],
        )
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        if let (Some(child), Some(bounds)) = (tree.children.first_mut(), layout.children().next()) {
            self.content
                .as_widget_mut()
                .operate(child, bounds, renderer, operation);
        }
    }
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        if self.animating
            && let Event::Window(iced::window::Event::RedrawRequested(now)) = event
        {
            shell.request_redraw_at(*now + Duration::from_millis(16));
        }
        let Some(child) = tree.children.first_mut() else {
            return;
        };
        let Some(bounds) = layout.children().next() else {
            return;
        };
        let motion = tree.state.downcast_mut::<Motion>();
        if !self.smooth {
            motion.start = None;
        }
        // A vertical line-based wheel step is captured here and turned into
        // an eased movement. The child never sees the original event.
        if self.smooth
            && cursor.is_over(layout.bounds())
            && let Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Lines { x, y },
            }) = event
            && *x == 0.0
            && *y != 0.0
        {
            let mut position = Position::default();
            self.content
                .as_widget_mut()
                .operate(child, bounds, renderer, &mut position);
            if position.maximum > 0.0 {
                motion.retarget(
                    position.current,
                    -*y * 60.0,
                    position.maximum,
                    Instant::now(),
                );
                motion.cursor = cursor.position();
                shell.capture_event();
                shell.request_redraw();
                return;
            }
        }
        // Trackpad scrolling, a click or a key press ends the movement so
        // direct input is not fought by the easing.
        if matches!(
            event,
            Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Pixels { .. }
            }) | Event::Mouse(mouse::Event::ButtonPressed(_))
                | Event::Keyboard(iced::keyboard::Event::KeyPressed { .. })
        ) {
            motion.start = None;
        }
        // Each frame of a movement: read where the scrollable is, compute
        // where the easing puts it, and send the child a pixel wheel event
        // for the difference.
        if let Event::Window(iced::window::Event::RedrawRequested(now)) = event
            && let Some(start) = motion.start
        {
            let mut position = Position::default();
            self.content
                .as_widget_mut()
                .operate(child, bounds, renderer, &mut position);
            let delta = position.current - motion.value(*now).clamp(0.0, position.maximum);
            let wheel = Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Pixels { x: 0.0, y: delta },
            });
            let wheel_cursor = motion
                .cursor
                .map(mouse::Cursor::Available)
                .unwrap_or(cursor);
            self.content.as_widget_mut().update(
                child,
                &wheel,
                bounds,
                wheel_cursor,
                renderer,
                clipboard,
                shell,
                viewport,
            );
            // The synthetic wheel changes widget state during a redraw event.
            // Request another frame even when the easing reaches its endpoint.
            shell.request_redraw();
            if now.saturating_duration_since(start) >= Duration::from_millis(100) {
                motion.start = None;
            } else {
                shell.request_redraw();
            }
        }
        self.content.as_widget_mut().update(
            child, event, bounds, cursor, renderer, clipboard, shell, viewport,
        );
        // Record where the scrollable ended up, once a frame.
        if let Some(positions) = &self.positions
            && matches!(
                event,
                Event::Window(iced::window::Event::RedrawRequested(_))
            )
        {
            let mut position = Position::default();
            self.content
                .as_widget_mut()
                .operate(child, bounds, renderer, &mut position);
            if position.found
                && let Ok(mut offsets) = positions.lock()
            {
                offsets.insert(self.key, position.current);
            }
        }
        // Page Up and Page Down move 90 percent of the visible height. Home
        // and End go to the edges.
        // Inputs and open menus get first refusal, so Home/End still edit text.
        if self.key != "animation-driver"
            && !shell.is_event_captured()
            && let Event::Keyboard(iced::keyboard::Event::KeyPressed {
                key: iced::keyboard::Key::Named(key),
                modifiers,
                ..
            }) = event
            && !modifiers.command()
            && !modifiers.alt()
        {
            use iced::keyboard::key::Named;
            let mut position = Position::default();
            self.content
                .as_widget_mut()
                .operate(child, bounds, renderer, &mut position);
            let target = match key {
                Named::PageUp => Some(position.current - position.viewport * 0.9),
                Named::PageDown => Some(position.current + position.viewport * 0.9),
                Named::Home => Some(0.0),
                Named::End => Some(position.maximum),
                _ => None,
            };
            if let Some(target) = target
                && position.maximum > 0.0
            {
                let mut scroll = widget::operation::scrollable::scroll_to::<()>(
                    widget::Id::new(self.key),
                    widget::operation::scrollable::AbsoluteOffset {
                        x: None,
                        y: Some(target.clamp(0.0, position.maximum)),
                    },
                );
                self.content
                    .as_widget_mut()
                    .operate(child, bounds, renderer, &mut scroll);
                motion.start = None;
                shell.capture_event();
                shell.request_redraw();
            }
        }
    }
    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &iced::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        use iced::advanced::Renderer;
        // The layer clips the translated content to this widget's bounds.
        if let (Some(child), Some(bounds)) = (tree.children.first(), layout.children().next())
            && let Some(clip) = layout.bounds().intersection(viewport)
        {
            renderer.with_layer(clip, |renderer| {
                self.content
                    .as_widget()
                    .draw(child, renderer, theme, style, bounds, cursor, &clip)
            });
        }
    }
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        match (tree.children.first(), layout.children().next()) {
            (Some(child), Some(bounds)) => self
                .content
                .as_widget()
                .mouse_interaction(child, bounds, cursor, viewport, renderer),
            _ => mouse::Interaction::default(),
        }
    }
    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, iced::Theme, iced::Renderer>> {
        self.content.as_widget_mut().overlay(
            tree.children.first_mut()?,
            layout.children().next()?,
            renderer,
            viewport,
            translation,
        )
    }
}

/// Finds the container with id `section` and reports how far down it sits.
///
/// The distance is measured from the top of the content of the scrollable
/// whose id is `Settings`. No message is produced when the container is not
/// in the current view.
pub fn jump<Message: Send + 'static>(
    section: &'static str,
    message: impl Fn(f32) -> Message + Send + 'static,
) -> iced::Task<Message> {
    // The scrollable is visited before the containers inside it, so `origin`
    // is set by the time a section is found.
    struct Anchor {
        section: widget::Id,
        origin: f32,
        y: Option<f32>,
    }
    impl Operation<f32> for Anchor {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation<f32>)) {
            visit(self);
        }
        fn scrollable(
            &mut self,
            id: Option<&widget::Id>,
            _: Rectangle,
            content: Rectangle,
            _: Vector,
            _: &mut dyn widget::operation::Scrollable,
        ) {
            if id == Some(&widget::Id::new("Settings")) {
                self.origin = content.y;
            }
        }
        fn container(&mut self, id: Option<&widget::Id>, bounds: Rectangle) {
            if id == Some(&self.section) {
                self.y = Some((bounds.y - self.origin).max(0.0));
            }
        }
        fn finish(&self) -> widget::operation::Outcome<f32> {
            self.y
                .map(widget::operation::Outcome::Some)
                .unwrap_or(widget::operation::Outcome::None)
        }
    }
    widget::operate(Anchor {
        section: widget::Id::new(section),
        origin: 0.0,
        y: None,
    })
    .map(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{TestResult, check, check_eq};
    #[cfg(target_os = "linux")]
    #[test]
    fn wheel_events_scroll_content_and_pixels_remain_direct() -> TestResult {
        let mut element: Element<'_, ()> = surface(
            iced::widget::scrollable(iced::widget::Space::new().height(1000).width(200))
                .id("fixture")
                .height(200)
                .width(200),
            0.0,
            true,
            "fixture",
        );
        let renderer = iced::Renderer::Secondary(iced_tiny_skia::Renderer::new(
            iced::Font::DEFAULT,
            iced::Pixels(16.0),
        ));
        let widget = element.as_widget_mut();
        let mut tree = Tree::new(&*widget);
        let node = widget.layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(200.0, 200.0)),
        );
        let bounds = Layout::new(&node);
        let viewport = Rectangle::with_size(Size::new(200.0, 200.0));
        let cursor = mouse::Cursor::Available(iced::Point::new(50.0, 50.0));
        let mut messages = vec![];
        let mut clipboard = iced::advanced::clipboard::Null;
        let mut dispatch = |tree: &mut Tree, event: Event| {
            widget.update(
                tree,
                &event,
                bounds,
                cursor,
                &renderer,
                &mut clipboard,
                &mut Shell::new(&mut messages),
                &viewport,
            );
        };
        dispatch(
            &mut tree,
            Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Lines { x: 0.0, y: -1.0 },
            }),
        );
        // X11 emits modifier notifications alongside wheel input even when no
        // key was pressed. They must not cancel the pending wheel movement.
        dispatch(
            &mut tree,
            Event::Keyboard(iced::keyboard::Event::ModifiersChanged(
                iced::keyboard::Modifiers::default(),
            )),
        );
        let mut position = Position::default();
        // No immediate jump on a wheel step.
        let motion = tree.state.downcast_ref::<Motion>();
        check_eq(motion.target, 60.0, "one wheel step targets 60 pixels")?;
        let finish = Instant::now() + Duration::from_millis(150);
        dispatch(
            &mut tree,
            Event::Window(iced::window::Event::RedrawRequested(finish)),
        );
        dispatch(
            &mut tree,
            Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Pixels { x: 0.0, y: -17.0 },
            }),
        );
        drop(dispatch);
        widget.operate(&mut tree, bounds, &renderer, &mut position);
        check_eq(
            position.current,
            77.0,
            "eased wheel and direct trackpad deltas reach content",
        )?;
        check(
            tree.state.downcast_ref::<Motion>().start.is_none(),
            "no momentum after the movement ends",
        )?;
        for (key, expected) in [
            (iced::keyboard::key::Named::PageDown, 257.0),
            (iced::keyboard::key::Named::End, 800.0),
            (iced::keyboard::key::Named::Home, 0.0),
        ] {
            let key = iced::keyboard::Key::Named(key);
            widget.update(
                &mut tree,
                &Event::Keyboard(iced::keyboard::Event::KeyPressed {
                    key: key.clone(),
                    modified_key: key,
                    physical_key: iced::keyboard::key::Physical::Code(
                        iced::keyboard::key::Code::PageDown,
                    ),
                    location: iced::keyboard::Location::Standard,
                    modifiers: iced::keyboard::Modifiers::default(),
                    text: None,
                    repeat: false,
                }),
                bounds,
                cursor,
                &renderer,
                &mut clipboard,
                &mut Shell::new(&mut messages),
                &viewport,
            );
            let mut position = Position::default();
            widget.operate(&mut tree, bounds, &renderer, &mut position);
            check_eq(
                position.current,
                expected,
                "keyboard scrolling stays direct",
            )?;
        }
        Ok(())
    }
    #[test]
    fn wheel_retargets_reverses_and_stops_at_its_clamped_target() -> TestResult {
        let now = Instant::now();
        let mut motion = Motion::default();
        motion.retarget(0.0, 60.0, 100.0, now);
        let middle = motion.value(now + Duration::from_millis(50));
        check(
            middle > 0.0 && middle < 60.0,
            "wheel movement eases between bounds",
        )?;
        motion.retarget(middle, -60.0, 100.0, now + Duration::from_millis(50));
        check_eq(motion.target, 0.0, "reversal starts from current position")?;
        check_eq(
            motion.value(now + Duration::from_millis(150)),
            0.0,
            "movement stops without momentum",
        )?;
        motion.retarget(0.0, 200.0, 100.0, now);
        check_eq(
            motion.value(now + Duration::from_secs(2)),
            100.0,
            "target clamps to content edge",
        )
    }
}
