//! A status-row command line with literal completion, owned by the GUI.
use super::{TermWindow, box_model::*, modal::Modal};
use crate::{scripting::guiwin::GuiWin, utilsprites::RenderMetrics};
use config::{
    Dimension, DimensionContext,
    keyassignment::{KeyAssignment, PromptInputLine},
};
use mux::pane::PaneId;
use std::cell::{Cell, Ref, RefCell};
use termwiz::lineedit::{LineEditBuffer, Movement};
use wezterm_term::{KeyCode, KeyModifiers, MouseEvent};

pub struct CommandPrompt {
    args: PromptInputLine,
    pane: PaneId,
    input: RefCell<LineEditBuffer>,
    selected: Cell<usize>,
    elements: RefCell<Option<Vec<ComputedElement>>>,
    rows: RefCell<Vec<window::RectF>>,
}
impl CommandPrompt {
    pub fn new(args: PromptInputLine, pane: PaneId) -> Self {
        let text = args.initial_value.as_deref().unwrap_or("");
        let input = RefCell::new(LineEditBuffer::new(text, text.len()));
        Self {
            args,
            pane,
            input,
            selected: Cell::new(0),
            elements: RefCell::new(None),
            rows: RefCell::new(vec![]),
        }
    }
    fn candidates(&self) -> Vec<String> {
        let input = self.input.borrow();
        self.args
            .completion
            .iter()
            .filter(|s| s.starts_with(input.get_line()))
            .cloned()
            .collect()
    }
    fn complete(&self, index: usize) {
        if let Some(s) = self.candidates().get(index) {
            self.input.borrow_mut().set_line_and_cursor(s, s.len());
        }
        self.selected.set(0);
    }
    fn finish(&self, window: &mut TermWindow, accept: bool) {
        if let KeyAssignment::EmitEvent(name) = self.args.action.as_ref() {
            crate::overlay::prompt::trampoline(
                name.clone(),
                GuiWin::new(window),
                mux_lua::MuxPane(self.pane),
                accept.then(|| self.input.borrow().get_line().to_owned()),
            );
        }
        window.cancel_modal();
    }
    fn compute(&self, window: &mut TermWindow) -> anyhow::Result<Vec<ComputedElement>> {
        let font = window.fonts.default_font()?;
        let metrics = RenderMetrics::with_font_metrics(&font.metrics());
        let cw = metrics.cell_size.width as f32;
        let ch = metrics.cell_size.height as f32;
        let palette = window.palette().clone();
        let dims = window.dimensions;
        let border = window.get_os_border();
        let x = border.left.get() as f32;
        let width = dims.pixel_width as f32 - x - border.right.get() as f32;
        let y = dims.pixel_height as f32 - border.bottom.get() as f32 - ch;
        let compute = |e: Element, bounds| {
            window.compute_element(
                &LayoutContext {
                    width: DimensionContext {
                        dpi: dims.dpi as f32,
                        pixel_max: width,
                        pixel_cell: cw,
                    },
                    height: DimensionContext {
                        dpi: dims.dpi as f32,
                        pixel_max: dims.pixel_height as f32,
                        pixel_cell: ch,
                    },
                    bounds,
                    metrics: &metrics,
                    gl_state: window.render_state.as_ref().unwrap(),
                    zindex: 100,
                },
                &e,
            )
        };
        let colors = |bg: wezterm_term::color::SrgbaTuple, fg: wezterm_term::color::SrgbaTuple| {
            ElementColors {
                border: BorderColor::new(palette.foreground.to_linear()),
                bg: bg.to_linear().into(),
                text: fg.to_linear().into(),
            }
        };
        let input = self.input.borrow();
        let text = format!("{}{}", self.args.prompt, input.get_line());
        let prompt_bg = palette.colors.0[7];
        let mut result = vec![compute(
            Element::new(&font, ElementContent::Text(text))
                .min_width(Some(Dimension::Pixels(width)))
                .min_height(Some(Dimension::Pixels(ch)))
                .colors(colors(prompt_bg, palette.foreground)),
            euclid::rect(x, y, width, ch),
        )?];
        let caret = termwiz::cell::unicode_column_width(
            &format!(
                "{}{}",
                self.args.prompt,
                &input.get_line()[..input.get_cursor()]
            ),
            None,
        ) as f32
            * cw;
        result.push(compute(
            Element::new(&font, ElementContent::Text("▏".into()))
                .colors(colors(prompt_bg, palette.foreground)),
            euclid::rect(x + caret.min(width - cw), y, cw, ch),
        )?);
        drop(input);
        let candidates = self.candidates();
        let count = candidates
            .len()
            .min(((y / ch) as usize).saturating_sub(2))
            .min(10);
        let menu_width = ((candidates
            .iter()
            .map(|s| termwiz::cell::unicode_column_width(s, None))
            .max()
            .unwrap_or(0)
            + 7) as f32
            * cw)
            .min(width - cw);
        let height = (count + 1) as f32 * ch;
        let top = y - height - ch / 2.;
        self.rows.borrow_mut().clear();
        if count > 0 {
            result.push(compute(
                Element::new(&font, ElementContent::Text(String::new()))
                    .min_width(Some(Dimension::Pixels(menu_width)))
                    .min_height(Some(Dimension::Pixels(height)))
                    .border(BoxDimension::new(Dimension::Pixels(1.)))
                    .colors(colors(palette.background, palette.foreground)),
                euclid::rect(x + cw / 2., top, menu_width, height),
            )?);
            for (i, s) in candidates.iter().take(count).enumerate() {
                let bounds =
                    euclid::rect(x + cw, top + ch / 2. + i as f32 * ch, menu_width - cw, ch);
                let active = i == self.selected.get().min(count - 1);
                let children = vec![
                    Element::new(&font, ElementContent::Text(format!(" {s}"))),
                    Element::new(&font, ElementContent::Text(format!("({i}) ")))
                        .float(Float::Right),
                ];
                result.push(compute(
                    Element::new(&font, ElementContent::Children(children))
                        .min_width(Some(Dimension::Pixels(menu_width - cw)))
                        .min_height(Some(Dimension::Pixels(ch)))
                        .colors(colors(
                            if active {
                                palette.colors.0[3]
                            } else {
                                palette.background
                            },
                            if active {
                                palette.colors.0[0]
                            } else {
                                palette.foreground
                            },
                        )),
                    bounds,
                )?);
                self.rows.borrow_mut().push(bounds);
            }
        }
        Ok(result)
    }
}
impl Modal for CommandPrompt {
    fn key_down(
        &self,
        key: KeyCode,
        mods: KeyModifiers,
        window: &mut TermWindow,
    ) -> anyhow::Result<bool> {
        match (key, mods) {
            (KeyCode::Escape, _) | (KeyCode::Char('c' | 'g'), KeyModifiers::CTRL) => {
                self.finish(window, false)
            }
            (KeyCode::Enter, _) => self.finish(window, true),
            (KeyCode::Tab, _) => self.complete(self.selected.get()),
            (KeyCode::Char(c @ '0'..='9'), KeyModifiers::ALT) => {
                self.complete(c as usize - '0' as usize)
            }
            (KeyCode::UpArrow, _) | (KeyCode::Char('k' | 'p'), KeyModifiers::CTRL) => {
                let n = self.candidates().len().min(10);
                if n > 0 {
                    self.selected.set((self.selected.get() + n - 1) % n);
                }
            }
            (KeyCode::DownArrow, _) | (KeyCode::Char('j' | 'n'), KeyModifiers::CTRL) => {
                let n = self.candidates().len().min(10);
                if n > 0 {
                    self.selected.set((self.selected.get() + 1) % n);
                }
            }
            (KeyCode::Home, _) | (KeyCode::Char('a'), KeyModifiers::CTRL) => {
                self.input.borrow_mut().exec_movement(Movement::StartOfLine)
            }
            (KeyCode::End, _) | (KeyCode::Char('e'), KeyModifiers::CTRL) => {
                self.input.borrow_mut().exec_movement(Movement::EndOfLine)
            }
            (KeyCode::LeftArrow, _) | (KeyCode::Char('b'), KeyModifiers::CTRL) => self
                .input
                .borrow_mut()
                .exec_movement(Movement::BackwardChar(1)),
            (KeyCode::RightArrow, _) | (KeyCode::Char('f'), KeyModifiers::CTRL) => self
                .input
                .borrow_mut()
                .exec_movement(Movement::ForwardChar(1)),
            (KeyCode::Backspace, _) | (KeyCode::Char('h'), KeyModifiers::CTRL) => self
                .input
                .borrow_mut()
                .kill_text(Movement::BackwardChar(1), Movement::BackwardChar(1)),
            (KeyCode::Char('w'), KeyModifiers::CTRL) => self
                .input
                .borrow_mut()
                .kill_text(Movement::BackwardWord(1), Movement::BackwardWord(1)),
            (KeyCode::Char('u'), KeyModifiers::CTRL) => self
                .input
                .borrow_mut()
                .kill_text(Movement::StartOfLine, Movement::StartOfLine),
            (KeyCode::Delete, _) | (KeyCode::Char('d'), KeyModifiers::CTRL) => self
                .input
                .borrow_mut()
                .kill_text(Movement::ForwardChar(1), Movement::None),
            (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                self.input.borrow_mut().insert_char(c);
                self.selected.set(0);
            }
            _ => {}
        }
        window.invalidate_modal();
        Ok(true)
    }
    fn window_mouse_event(&self, event: &window::MouseEvent, window: &mut TermWindow) -> bool {
        if matches!(
            event.kind,
            window::MouseEventKind::Press(window::MousePress::Left)
        ) {
            let hit = self.rows.borrow().iter().position(|r| {
                r.contains(euclid::point2(event.coords.x as f32, event.coords.y as f32))
            });
            if let Some(i) = hit {
                self.complete(i);
                window.invalidate_modal();
            }
        }
        true
    }
    fn mouse_event(&self, _: MouseEvent, _: &mut TermWindow) -> anyhow::Result<()> {
        Ok(())
    }
    fn computed_element(
        &self,
        window: &mut TermWindow,
    ) -> anyhow::Result<Ref<'_, [ComputedElement]>> {
        if self.elements.borrow().is_none() {
            self.elements.replace(Some(self.compute(window)?));
        }
        Ok(Ref::map(self.elements.borrow(), |v| {
            v.as_ref().unwrap().as_slice()
        }))
    }
    fn reconfigure(&self, _: &mut TermWindow) {
        self.elements.borrow_mut().take();
    }
}
