//! Clipped page translation and wheel smoothing without momentum.
use iced::advanced::widget::{Operation, Tree, tree};
use iced::advanced::{Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer, widget};
use iced::{Element, Event, Length, Rectangle, Size, Vector};
use std::time::{Duration, Instant};

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
    })
}
struct Surface<'a, Message> {
    content: Element<'a, Message>,
    offset: f32,
    smooth: bool,
    key: &'static str,
}
#[derive(Default)]
struct Motion {
    from: f32,
    target: f32,
    start: Option<Instant>,
    cursor: Option<iced::Point>,
    key: &'static str,
}
impl Motion {
    fn value(&self, now: Instant) -> f32 {
        let Some(start) = self.start else {
            return self.target;
        };
        let t = now.saturating_duration_since(start).as_secs_f32() / 0.1;
        self.from + (self.target - self.from) * (1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3))
    }
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
#[derive(Default)]
struct Position {
    current: f32,
    maximum: f32,
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
        if matches!(
            event,
            Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Pixels { .. }
            }) | Event::Mouse(mouse::Event::ButtonPressed(_))
                | Event::Keyboard(_)
        ) {
            motion.start = None;
        }
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
            if now.saturating_duration_since(start) >= Duration::from_millis(100) {
                motion.start = None;
            } else {
                shell.request_redraw();
            }
        }
        self.content.as_widget_mut().update(
            child, event, bounds, cursor, renderer, clipboard, shell, viewport,
        );
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

pub fn jump<Message: Send + 'static>(
    section: &'static str,
    message: impl Fn(f32) -> Message + Send + 'static,
) -> iced::Task<Message> {
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
        widget.operate(&mut tree, bounds, &renderer, &mut position);
        check_eq(
            position.current,
            77.0,
            "eased wheel and direct trackpad deltas reach content",
        )?;
        check(
            tree.state.downcast_ref::<Motion>().start.is_none(),
            "no momentum after the movement ends",
        )
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
