//! Decode Rex's wire snapshot once, then import it into WezTerm's terminal.
//! The decoded Ghostty terminal is dropped before live output is processed.
use anyhow::Result;
use std::sync::Arc;
use termwiz::{cell::{Blink, Cell as NativeCell, CellAttributes, Intensity, Underline},
    color::ColorAttribute,surface::{Line,SequenceNo}};
use ghostty_vt::{screen::CellWide, terminal::{Point,PointCoordinate},style::Underline as GUnderline};

use ghostty_vt::{Terminal as Snapshot, snapshot, screen::Screen,
    fmt::{Format, Formatter, FormatterOptions}, terminal::Mode, style::{Style, StyleColor}};
use wezterm_term::{Terminal, TerminalSize};

fn clone_snapshot(source: &Snapshot) -> Result<Snapshot> {
    let bytes=snapshot::encode(source)?;
    let mut decoder=snapshot::Decoder::from_slice(&bytes);
    decoder.set_retain_continuation(true)?;
    Ok(decoder.decode()?)
}
fn sgr(style: Style) -> String {
    let mut values=vec!["0".to_owned()];
    for (enabled,value) in [(style.bold,"1"),(style.faint,"2"),(style.italic,"3"),
        (style.blink,"5"),(style.inverse,"7"),(style.invisible,"8"),
        (style.strikethrough,"9"),(style.overline,"53")] {
        if enabled {values.push(value.into());}
    }
    let underline=match style.underline {
        ghostty_vt::style::Underline::Single=>"4:1",
        ghostty_vt::style::Underline::Double=>"4:2",
        ghostty_vt::style::Underline::Curly=>"4:3",
        ghostty_vt::style::Underline::Dotted=>"4:4",
        ghostty_vt::style::Underline::Dashed=>"4:5",_=>"4:0",
    };
    values.push(underline.into());
    for (c,code) in [(style.fg_color,38),(style.bg_color,48),(style.underline_color,58)] {
        match c {
            StyleColor::Palette(i)=>values.push(format!("{code};5;{}",i.0)),
            StyleColor::Rgb(c)=>values.push(format!("{code};2;{};{};{}",c.r,c.g,c.b)),
            StyleColor::None=>{},
        }
    }
    format!("\x1b[{}m",values.join(";"))
}
fn import_screen(source: &Snapshot, native: &mut Terminal, screen: Screen) -> Result<()> {
    let mut view=clone_snapshot(source)?;
    // Cancel only the adapter's parser continuation; restore the original
    // continuation into WezTerm after all snapshot state has been imported.
    view.vt_write(b"\x18");
    let select=if screen==Screen::Alternate {b"\x1b[?47h"} else {b"\x1b[?47l"};
    view.vt_write(select);native.advance_bytes(select);
    // The formatter must not select/clear a screen after we restored it.
    for mode in [Mode::ALT_SCREEN_LEGACY,Mode::ALT_SCREEN,Mode::ALT_SCREEN_SAVE,Mode::SAVE_CURSOR,Mode::SYNC_OUTPUT] {
        view.set_mode(mode,false)?;
    }
    let mut saved=clone_snapshot(&view)?;
    saved.vt_write(b"\x1b8");
    native.advance_bytes(format!("\x1b[?6l\x1b[{};{}H{}\x1b7",saved.cursor_y()?+1,saved.cursor_x()?+1,sgr(saved.cursor_style()?)).as_bytes());
    let options=FormatterOptions::new().with_format(Format::Vt).with_unwrap(false).with_trim(false)
        .with_palette(true).with_modes(true).with_scrolling_region(true).with_tabstops(true)
        .with_pwd(true).with_keyboard(true).with_cursor(true).with_style(true)
        .with_hyperlink(true).with_protection(true).with_kitty_keyboard(true).with_charsets(true);
    let bytes=Formatter::new(&view,options)?.format_alloc()?;
    native.advance_bytes(b"\x1b[?6l\x1b[r\x1b[H");
    native.advance_bytes(&bytes);
    let seq=native.current_seqno();
    let total=view.scrollbar()?.total;
    let lines=(0..total).map(|y|row(&view,y as u32).map(|r|line(r,seq))).collect::<Result<Vec<_>>>()?;
    native.screen_mut().import_remote_lines(lines,seq);
    import_images(&mut view,native)?;
    Ok(())
}
pub fn restore(source: Snapshot, native: &mut Terminal) -> Result<()> {
    let continuation=source.continuation()?;
    let active=source.active_screen()?;
    let old=native.get_size();
    native.reset_for_remote_snapshot();
    native.resize(TerminalSize {cols:source.cols()? as usize,rows:source.rows()? as usize,
        pixel_width:source.width_px()? as usize,pixel_height:source.height_px()? as usize,dpi:old.dpi});
    if active==Screen::Alternate {
        import_screen(&source,native,Screen::Primary)?;
        import_screen(&source,native,Screen::Alternate)?;
    } else {
        import_screen(&source,native,Screen::Alternate)?;
        import_screen(&source,native,Screen::Primary)?;
    }
    native.advance_bytes(format!("\x1b]2;{}\x1b\\",source.title()?).as_bytes());
    native.advance_bytes(&continuation);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    #[derive(Debug)] struct Config;
    impl wezterm_term::TerminalConfiguration for Config {
        fn color_palette(&self)->wezterm_term::color::ColorPalette {Default::default()}
        fn scrollback_size(&self)->usize {1000}
        fn enable_kitty_graphics(&self)->bool {true}
    }
    fn native()->Terminal {
        let mut t=Terminal::new(TerminalSize{cols:40,rows:5,pixel_width:320,pixel_height:80,dpi:96},Arc::new(Config),"WezTerm","test",Box::new(std::io::sink()));
        t.set_mirrored_output(true);t
    }
    fn snapshot(bytes:&[u8])->Snapshot {
        let mut t=Snapshot::new(ghostty_vt::TerminalOptions{cols:40,rows:5,..Default::default()}).unwrap();
        t.resize(40,5,8,16).unwrap();t.set_continuation_max_bytes(1024*1024).unwrap();t.vt_write(bytes);t
    }
    fn text(t:&Terminal)->String {
        t.screen().lines_in_phys_range(0..t.screen().scrollback_rows()).iter().map(|l|l.as_str().trim_end().to_owned()).collect::<Vec<_>>().join("\n")
    }
    #[test]
    fn restore_primary_history_and_continue_native_color_output() {
        let mut bytes=Vec::new();for i in 0..30 {bytes.extend(format!("ROW_{i}\r\n").as_bytes());}
        bytes.extend(b"\x1b[31mRED");let source=snapshot(&bytes);let mut native=native();
        restore(source,&mut native).unwrap();native.advance_bytes(b"_LIVE\x1b[0m");
        let text=text(&native);assert!(text.contains("ROW_0\nROW_1\n"),"{text:?}");assert!(text.contains("RED_LIVE"),"{text:?}");
        let line=native.screen().lines_in_phys_range(0..native.screen().scrollback_rows()).into_iter().find(|l|l.as_str().contains("RED_LIVE")).unwrap();
        assert_eq!(line.visible_cells().next().unwrap().attrs().foreground(),termwiz::color::ColorAttribute::PaletteIndex(1));
    }
    #[test]
    fn restore_both_screens_saved_cursor_and_partial_osc() {
        let source=snapshot(b"PRIMARY\x1b[3;4H\x1b7\x1b[?1049hALT\x1b]2;par");
        let mut native=native();restore(source,&mut native).unwrap();
        assert!(native.is_alt_screen_active());assert!(text(&native).contains("ALT"));
        native.advance_bytes(b"tial title\x1b\\\x1b[?1049l\x1b8Z");
        assert_eq!(native.get_title(),"partial title");assert!(!native.is_alt_screen_active());
        assert!(text(&native).contains("PRIMARY"),"{:?}",text(&native));
        assert_eq!((native.cursor_pos().x,native.cursor_pos().y),(4,2));
    }
    #[test]
    fn snapshot_kitty_history_uses_native_images_and_deletion() {
        let mut source=snapshot(b"\x1b_Ga=T,f=32,s=1,v=1,c=2,r=2,i=42,q=2;/wAA/w==\x1b\\");
        for _ in 0..10 {source.vt_write(b"\r\nROW");}
        let mut native=native();restore(source,&mut native).unwrap();
        let has_image=|t:&Terminal|t.screen().lines_in_phys_range(0..t.screen().scrollback_rows()).iter()
            .any(|line|line.visible_cells().any(|cell|cell.attrs().images().is_some()));
        assert!(has_image(&native));
        native.advance_bytes(b"\x1b_Ga=d,d=A,q=2\x1b\\");
        assert!(!has_image(&native));
    }
    #[test]
    fn live_synchronized_output_is_atomic() {
        let mut native=native();restore(snapshot(b"BEFORE"),&mut native).unwrap();
        native.advance_bytes(b"\x1b[?2026h\x1b[2J\x1b[HAFTER");
        assert!(text(&native).contains("BEFORE"));assert!(!text(&native).contains("AFTER"));
        native.advance_bytes(b"\x1b[?2026l");
        assert!(text(&native).contains("AFTER"));assert!(!text(&native).contains("BEFORE"));
    }
    #[test]
    fn live_imgcat_uses_native_wezterm_image_decoder() {
        let mut native=native();restore(snapshot(b"READY\r\n"),&mut native).unwrap();
        let png="iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";
        native.advance_bytes(format!("\x1b]1337;File=inline=1;width=2;height=2:{png}\x07").as_bytes());
        assert!(native.screen().lines_in_phys_range(0..native.screen().scrollback_rows()).iter().any(|line|line.visible_cells().any(|cell|cell.attrs().images().is_some())));
    }
}

#[derive(Clone, Debug)]
pub struct Cell {
    pub x: usize,
    pub text: String,
    pub width: usize,
    pub style: Style,
    pub link: Option<String>,
    pub semantic: u8,
}
#[derive(Clone, Debug, Default)]
pub struct Row {
    pub cells: Vec<Cell>,
    pub wrapped: bool,
}

fn row(term: &Snapshot, y: u32) -> Result<Row> {
    let mut cells = Vec::with_capacity(term.cols()? as usize);
    let mut wrapped = false;
    for x in 0..term.cols()? {
        let grid = term.grid_ref(Point::Screen(PointCoordinate { x, y }))?;
        if x == 0 {
            wrapped = grid.row()?.is_wrapped()?;
        }
        let cell = grid.cell()?;
        let wide = cell.wide()?;
        if wide == CellWide::SpacerTail {
            continue;
        }
        let mut chars = vec!['\0'; 16];
        let len = match grid.graphemes(&mut chars) {
            Err(ghostty_vt::Error::OutOfSpace { required }) => {
                chars.resize(required, '\0');
                grid.graphemes(&mut chars)?
            }
            r => r?,
        };
        let text = if len == 0 {
            " ".into()
        } else {
            chars[..len].iter().collect()
        };
        let link = if cell.has_hyperlink()? {
            let mut buf = vec![0; 4096];
            let n = match grid.hyperlink_uri(&mut buf) {
                Err(ghostty_vt::Error::OutOfSpace { required }) => {
                    buf.resize(required, 0);
                    grid.hyperlink_uri(&mut buf)?
                }
                r => r?,
            };
            Some(String::from_utf8_lossy(&buf[..n]).into_owned())
        } else {
            None
        };
        let mut style = grid.style()?;
        match cell.content_tag()? {
            ghostty_vt::screen::CellContentTag::BgColorPalette => {
                style.bg_color = ghostty_vt::style::StyleColor::Palette(cell.bg_color_palette()?)
            }
            ghostty_vt::screen::CellContentTag::BgColorRgb => {
                style.bg_color = ghostty_vt::style::StyleColor::Rgb(cell.bg_color_rgb()?)
            }
            _ => {}
        }
        cells.push(Cell {
            x: x as usize,
            text,
            width: if wide == CellWide::Wide { 2 } else { 1 },
            style,
            link,
            semantic: cell.semantic_content()? as u8,
        });
    }
    Ok(Row { cells, wrapped })
}

fn color(c: StyleColor) -> ColorAttribute {
    match c {
        StyleColor::None => ColorAttribute::Default,
        StyleColor::Palette(i) => ColorAttribute::PaletteIndex(i.0),
        StyleColor::Rgb(c) => ColorAttribute::TrueColorWithDefaultFallback(
            termwiz::color::SrgbaTuple::from((c.r, c.g, c.b)),
        ),
    }
}
fn line(row: Row, seq: SequenceNo) -> Line {
    let mut line = Line::new(seq);
    for c in row.cells {
        let s = c.style;
        let mut a = CellAttributes::default();
        a.set_semantic_type(match c.semantic {
            1 => termwiz::cell::SemanticType::Input,
            2 => termwiz::cell::SemanticType::Prompt,
            _ => termwiz::cell::SemanticType::Output,
        });
        a.set_foreground(color(s.fg_color))
            .set_background(color(s.bg_color))
            .set_underline_color(color(s.underline_color));
        a.set_intensity(if s.bold {
            Intensity::Bold
        } else if s.faint {
            Intensity::Half
        } else {
            Intensity::Normal
        })
        .set_italic(s.italic)
        .set_reverse(s.inverse)
        .set_invisible(s.invisible)
        .set_strikethrough(s.strikethrough)
        .set_overline(s.overline)
        .set_blink(if s.blink { Blink::Slow } else { Blink::None });
        a.set_underline(match s.underline {
            GUnderline::Single => Underline::Single,
            GUnderline::Double => Underline::Double,
            GUnderline::Curly => Underline::Curly,
            GUnderline::Dotted => Underline::Dotted,
            GUnderline::Dashed => Underline::Dashed,
            _ => Underline::None,
        });
        if let Some(link) = c.link {
            a.set_hyperlink(Some(Arc::new(termwiz::hyperlink::Hyperlink::new(link))));
        }
        line.set_cell(c.x, NativeCell::new_grapheme_with_width(&c.text, c.width, a), seq);
    }
    line.set_last_cell_was_wrapped(row.wrapped, seq);
    line
}

fn import_images(view:&mut Snapshot,native:&mut Terminal)->Result<()> {
    use ghostty_vt::terminal::ScrollViewport;
    use termwiz::image::{ImageCell,ImageData,ImageDataType,TextureCoordinate};
    let total=view.scrollbar()?.total as usize;
    let rows=view.rows()? as usize;let cols=view.cols()? as usize;
    let cw=(view.width_px()? / cols.max(1) as u32).max(1) as f32;
    let ch=(view.height_px()? / rows.max(1) as u32).max(1) as f32;
    let mut render=ghostty_vt::render::RenderState::new()?;
    let mut images=crate::graphics::Images::new()?;
    let mut all:Vec<crate::graphics::Image>=vec![];
    let mut y=0;
    loop {
        view.scroll_viewport(ScrollViewport::Row(y));
        let top=view.scrollbar()?.offset as usize;
        let frame=render.update(view)?;
        for mut image in images.frame(view,&frame)? {
            image.placement.cell[1]+=top as i32;
            if !all.iter().any(|old|old.placement==image.placement) {all.push(image);}
        }
        if top+rows>=total {break;}
        y=top+rows;
    }
    view.scroll_viewport(ScrollViewport::Bottom);
    let first=native.screen().phys_to_stable_row_index(0);
    let count=native.screen().scrollback_rows();
    for image in all {
        let p=image.placement;let pixels=image.pixels;
        let data=Arc::new(ImageData::with_data(ImageDataType::new_single_frame(pixels.width,pixels.height,pixels.rgba.to_vec())));
        let left=p.cell[0] as f32*cw+p.offset[0] as f32;
        let top=p.cell[1] as f32*ch+p.offset[1] as f32;
        let right=left+p.size[0] as f32;let bottom=top+p.size[1] as f32;
        let start=(top/ch).floor() as isize;let end=(bottom/ch).ceil() as isize;
        for stable in start.max(first)..end.min(first+count as isize) {
            let row=stable as f32;let y0=(row*ch).max(top);let y1=((row+1.)*ch).min(bottom);
            if y1<=y0 {continue;}
            let line=native.screen_mut().line_mut((stable-first) as usize);
            for x in (left/cw).floor().max(0.) as usize..((right/cw).ceil() as usize).min(cols) {
                let x0=(x as f32*cw).max(left);let x1=((x+1) as f32*cw).min(right);
                if x1<=x0 {continue;}
                let u=|x:f32|(p.source[0] as f32+(x-left)*p.source[2] as f32/p.size[0].max(1) as f32)/pixels.width as f32;
                let v=|y:f32|(p.source[1] as f32+(y-top)*p.source[3] as f32/p.size[1].max(1) as f32)/pixels.height as f32;
                if let Some(cell)=line.cells_mut_for_attr_changes_only().get_mut(x) {
                    cell.attrs_mut().attach_image(Box::new(ImageCell::with_z_index(
                        TextureCoordinate::new_f32(u(x0),v(y0)),TextureCoordinate::new_f32(u(x1),v(y1)),data.clone(),p.z,
                        (x0-x as f32*cw) as u16,(y0-row*ch) as u16,((x+1) as f32*cw-x1) as u16,((row+1.)*ch-y1) as u16,
                        Some(p.image_id),None)));
                }
            }
        }
        native.import_remote_kitty_image(p.image_id,data,start,(end-start).max(0) as usize,cols);
    }
    Ok(())
}
