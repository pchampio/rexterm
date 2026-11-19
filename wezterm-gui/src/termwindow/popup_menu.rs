//! A compact native menu. Contents and actions are supplied by Lua.
use super::box_model::*;
use super::modal::Modal;
use super::TermWindow;
use crate::utilsprites::RenderMetrics;
use config::keyassignment::PopupMenu as MenuArgs;
use config::{Dimension, DimensionContext};
use mux::pane::PaneId;
use std::cell::{Cell, Ref, RefCell};
use wezterm_term::{KeyCode, KeyModifiers, MouseEvent};

pub struct PopupMenu {
    args: MenuArgs,
    pane_id: PaneId,
    selected: Cell<usize>,
    top: Cell<usize>,
    elements: RefCell<Option<Vec<ComputedElement>>>,
    rows: RefCell<Vec<(usize, window::RectF)>>,
}

impl PopupMenu {
    pub fn new(args: MenuArgs, pane_id: PaneId) -> Self {
        let selected = args.items.iter().position(|r| !r.label.is_empty() && r.action.is_some()).unwrap_or(0);
        Self { args, pane_id, selected: Cell::new(selected), top: Cell::new(0), elements: RefCell::new(None), rows: RefCell::new(vec![]) }
    }

    fn move_selection(&self, down: bool) {
        let count = self.args.items.len();
        if count == 0 { return; }
        let mut index = self.selected.get();
        for _ in 0..count {
            index = if down { (index + 1) % count } else { (index + count - 1) % count };
            let entry = &self.args.items[index];
            if !entry.label.is_empty() && entry.action.is_some() {
                self.selected.set(index);
                break;
            }
        }
    }

    fn launch(&self, index: usize, window: &mut TermWindow) -> anyhow::Result<()> {
        if let Some(action) = self.args.items.get(index).and_then(|r| r.action.as_ref()) {
            window.cancel_modal();
            if let Some(pane) = mux::Mux::get().get_pane(self.pane_id) {
                window.perform_key_assignment(&pane, action)?;
            }
        }
        Ok(())
    }

    fn compute(&self, window: &mut TermWindow) -> anyhow::Result<Vec<ComputedElement>> {
        let font = window.fonts.default_font()?;
        let metrics = RenderMetrics::with_font_metrics(&font.metrics());
        let cw = metrics.cell_size.width as f32;
        let ch = metrics.cell_size.height as f32;
        let palette = window.palette().clone();
        let bg: InheritableColor = palette.background.to_linear().into();
        let fg: InheritableColor = palette.foreground.to_linear().into();
        let selected_bg: InheritableColor = palette.colors.0[3].to_linear().into();
        let selected_fg: InheritableColor = palette.colors.0[0].to_linear().into();
        let width_cells = self.args.items.iter().map(|r| termwiz::cell::unicode_column_width(&r.label, None) + 8).max().unwrap_or(20)
            .max(termwiz::cell::unicode_column_width(&self.args.title, None) + 4);
        let width = (width_cells as f32 * cw).min((window.dimensions.pixel_width as f32 - 2. * cw).max(cw));
        let available_height = window.dimensions.pixel_height as f32 - if window.show_tab_bar { window.tab_bar_pixel_height()? } else { 0. };
        let visible = ((available_height / ch) as usize).saturating_sub(3).max(1).min(self.args.items.len());
        let mut top = self.top.get().min(self.args.items.len().saturating_sub(visible));
        if self.selected.get() < top { top = self.selected.get(); }
        if self.selected.get() >= top + visible { top = self.selected.get() + 1 - visible; }
        self.top.set(top);
        let height = (visible + 1) as f32 * ch;
        let bottom_right = self.args.anchor.as_deref().is_some_and(|a| a.eq_ignore_ascii_case("BottomRight"));
        let (x, y) = if bottom_right {
            ((window.dimensions.pixel_width as f32 - width - cw).max(0.), (available_height - height - ch / 2.).max(ch / 2.))
        } else {
            ((window.dimensions.pixel_width as f32 - width) / 2., ((available_height - height) / 2.).max(ch / 2.))
        };
        let dims = window.dimensions;
        let compute = |element: Element, bounds| window.compute_element(&LayoutContext {
            width: DimensionContext { dpi: dims.dpi as f32, pixel_max: dims.pixel_width as f32, pixel_cell: cw },
            height: DimensionContext { dpi: dims.dpi as f32, pixel_max: dims.pixel_height as f32, pixel_cell: ch },
            bounds, metrics: &metrics, gl_state: window.render_state.as_ref().unwrap(), zindex: 100,
        }, &element);
        let colors = |background: InheritableColor, foreground: InheritableColor| ElementColors {
            border: BorderColor::new(palette.foreground.to_linear()), bg: background, text: foreground,
        };
        let mut elements = vec![compute(Element::new(&font, ElementContent::Text(String::new()))
            .min_width(Some(Dimension::Pixels(width))).min_height(Some(Dimension::Pixels(height)))
            .border(BoxDimension::new(Dimension::Pixels(1.)))
            .colors(colors(bg.clone(), fg.clone())), euclid::rect(x, y, width, height))?];
        let title = format!(" {} ", self.args.title);
        let title_width = termwiz::cell::unicode_column_width(&title, None) as f32 * cw;
        elements.push(compute(Element::new(&font, ElementContent::Text(title))
            .max_width(Some(Dimension::Pixels(width - cw))).colors(colors(bg.clone(), fg.clone())),
            euclid::rect(x + (width - title_width.min(width - cw)) / 2., y - ch / 2., width - cw, ch))?);
        self.rows.borrow_mut().clear();
        for (offset, (index, item)) in self.args.items.iter().enumerate().skip(top).take(visible).enumerate() {
            let bounds = euclid::rect(x + cw / 2., y + ch / 2. + offset as f32 * ch, width - cw, ch);
            if item.label.is_empty() {
                elements.push(compute(Element::new(&font, ElementContent::Text(String::new()))
                    .min_width(Some(Dimension::Pixels(width - 2.)))
                    .border(BoxDimension { top: Dimension::Pixels(1.), ..Default::default() })
                    .colors(colors(bg.clone(), fg.clone())),
                    euclid::rect(x + 1., bounds.origin.y + ch / 2., width - 2., 1.))?);
                continue;
            }
            let active = index == self.selected.get() && item.action.is_some();
            let (row_bg, row_fg) = if active { (selected_bg.clone(), selected_fg.clone()) }
                // Unavailable here: a colour between the text and the page
                // (text is drawn opaque, so an alpha would not dim it).
                else if item.action.is_none() {
                    let (f, b) = (palette.foreground.to_linear(), palette.background.to_linear());
                    let t = 0.42;
                    (bg.clone(), window::color::LinearRgba::with_components(b.0 + (f.0 - b.0) * t, b.1 + (f.1 - b.1) * t, b.2 + (f.2 - b.2) * t, 1.).into())
                }
                else { (bg.clone(), fg.clone()) };
            let label = termwiz_funcs::truncate_right(&item.label, ((width / cw) as usize).saturating_sub(8));
            let mut children = vec![Element::new(&font, ElementContent::Text(format!(" {label}")))];
            if let Some(key) = &item.key {
                let prefix = if item.mods == KeyModifiers::CTRL { "C-" } else if item.mods == KeyModifiers::ALT { "M-" } else { "" };
                children.push(Element::new(&font, ElementContent::Text(format!("({prefix}{key}) "))).float(Float::Right));
            }
            elements.push(compute(Element::new(&font, ElementContent::Children(children))
                .min_width(Some(Dimension::Pixels(width - cw))).min_height(Some(Dimension::Pixels(ch)))
                .colors(colors(row_bg, row_fg)), bounds)?);
            self.rows.borrow_mut().push((index, bounds));
        }
        // The preview of the selected row: a box of plain text at the left
        // of the menu (or at its right when the left has no room).
        if let Some(preview) = self.args.items.get(self.selected.get()).and_then(|item| item.preview.as_deref()) {
            let lines: Vec<&str> = preview.lines().collect();
            let rows = lines.len().clamp(1, ((available_height / ch) as usize).saturating_sub(2).max(1));
            let lines = &lines[lines.len().saturating_sub(rows)..];
            let cols = lines.iter().map(|l| termwiz::cell::unicode_column_width(l, None)).max().unwrap_or(0).max(12) + 2;
            let room_left = x - cw;
            let room_right = window.dimensions.pixel_width as f32 - (x + width) - cw;
            let box_width = (cols as f32 * cw).min(room_left.max(room_right)).max(cw);
            let box_height = rows as f32 * ch + ch / 2.;
            let box_x = if room_left >= room_right { x - box_width - cw / 2. } else { x + width + cw / 2. };
            let box_y = (y + height - box_height).max(ch / 2.);
            elements.push(compute(Element::new(&font, ElementContent::Text(String::new()))
                .min_width(Some(Dimension::Pixels(box_width))).min_height(Some(Dimension::Pixels(box_height)))
                .border(BoxDimension::new(Dimension::Pixels(1.)))
                .colors(colors(bg.clone(), fg.clone())), euclid::rect(box_x, box_y, box_width, box_height))?);
            let text_cols = ((box_width / cw) as usize).saturating_sub(2);
            for (row, line) in lines.iter().enumerate() {
                let line = termwiz_funcs::truncate_right(line, text_cols);
                elements.push(compute(Element::new(&font, ElementContent::Text(format!(" {line}")))
                    .max_width(Some(Dimension::Pixels(box_width - cw))).colors(colors(bg.clone(), fg.clone())),
                    euclid::rect(box_x + cw / 2., box_y + ch / 4. + row as f32 * ch, box_width - cw, ch))?);
            }
        }
        Ok(elements)
    }
}

impl Modal for PopupMenu {
    fn window_mouse_event(&self, event: &window::MouseEvent, window: &mut TermWindow) -> bool {
        let hit = self.rows.borrow().iter().find(|(_, r)| r.contains(euclid::point2(event.coords.x as f32, event.coords.y as f32))).map(|(i, _)| *i);
        match event.kind {
            window::MouseEventKind::Move => {
                if let Some(i) = hit.filter(|i| self.args.items[*i].action.is_some()) { self.selected.set(i); }
            }
            window::MouseEventKind::Press(window::MousePress::Left) => {
                if let Some(i) = hit { self.launch(i, window).ok(); } else { window.cancel_modal(); }
            }
            window::MouseEventKind::VertWheel(n) => self.move_selection(n < 0),
            _ => {}
        }
        window.invalidate_modal();
        true
    }

    fn mouse_event(&self, _: MouseEvent, _: &mut TermWindow) -> anyhow::Result<()> { Ok(()) }

    fn key_down(&self, key: KeyCode, mods: KeyModifiers, window: &mut TermWindow) -> anyhow::Result<bool> {
        if let KeyCode::Char(c) = &key {
            if let Some(i) = self.args.items.iter().position(|r| r.key.as_deref() == Some(&c.to_string())
                && (r.mods == mods || (r.mods == KeyModifiers::NONE && mods == KeyModifiers::SHIFT))) {
                self.launch(i, window)?;
                return Ok(true);
            }
        }
        match (key, mods) {
            (KeyCode::Escape, _) | (KeyCode::Char('c' | 'g'), KeyModifiers::CTRL) => window.cancel_modal(),
            (KeyCode::UpArrow, _) | (KeyCode::Char('k' | 'p'), KeyModifiers::CTRL) => self.move_selection(false),
            (KeyCode::DownArrow, _) | (KeyCode::Char('j' | 'n'), KeyModifiers::CTRL) => self.move_selection(true),
            (KeyCode::Enter, _) => self.launch(self.selected.get(), window)?,
            _ => {}
        }
        window.invalidate_modal();
        Ok(true)
    }

    fn computed_element(&self, window: &mut TermWindow) -> anyhow::Result<Ref<'_, [ComputedElement]>> {
        if self.elements.borrow().is_none() { self.elements.replace(Some(self.compute(window)?)); }
        Ok(Ref::map(self.elements.borrow(), |e| e.as_ref().unwrap().as_slice()))
    }

    fn reconfigure(&self, _: &mut TermWindow) { self.elements.borrow_mut().take(); }
}
