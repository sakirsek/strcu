//! Windows UI Automation: names and positions of the clickable elements on screen.
//!
//! Elements are fetched with one cached query per window (instead of one COM call per element).

use anyhow::Result;
use serde::Serialize;
use uiautomation::types::{ControlType, Handle, Point, TreeScope, UIProperty};
use uiautomation::{UIAutomation, UIElement};
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, GA_ROOT, GetAncestor, WindowFromPoint};
use windows::core::w;

use super::{Rect, window};

#[derive(Debug, Clone, Serialize)]
pub struct Element {
    pub id: usize,
    pub name: String,
    pub kind: &'static str,
    pub rect: Rect,
    /// Title of the window the element belongs to, or "taskbar".
    pub source: String,
}

fn kind_of(ct: ControlType) -> Option<&'static str> {
    Some(match ct {
        ControlType::Button | ControlType::SplitButton => "button",
        ControlType::MenuItem => "menu item",
        ControlType::TabItem => "tab",
        ControlType::ListItem => "list item",
        ControlType::Hyperlink => "link",
        ControlType::Edit => "text field",
        ControlType::CheckBox => "checkbox",
        ControlType::RadioButton => "radio button",
        ControlType::ComboBox => "dropdown",
        ControlType::TreeItem => "tree item",
        ControlType::DataItem => "data item",
        _ => return None,
    })
}

/// Kind name shown in the panel for the element under the crosshair.
fn kind_name(ct: ControlType) -> &'static str {
    match ct {
        ControlType::Button | ControlType::SplitButton => "button",
        ControlType::MenuItem => "menu item",
        ControlType::Menu | ControlType::MenuBar => "menu",
        ControlType::TabItem => "tab",
        ControlType::ListItem => "list item",
        ControlType::List => "list",
        ControlType::Hyperlink => "link",
        ControlType::Edit => "text field",
        ControlType::Document => "document",
        ControlType::CheckBox => "checkbox",
        ControlType::RadioButton => "radio button",
        ControlType::ComboBox => "dropdown",
        ControlType::TreeItem => "tree item",
        ControlType::DataItem => "data item",
        ControlType::Text => "text",
        ControlType::Image => "image",
        ControlType::ToolBar => "toolbar",
        ControlType::TitleBar => "title bar",
        ControlType::ScrollBar => "scrollbar",
        ControlType::Slider => "slider",
        ControlType::Window => "window",
        ControlType::Pane => "pane",
        ControlType::Group => "group",
        _ => "element",
    }
}

fn clean_name(s: &str) -> String {
    s.replace('\u{200e}', "").lines().next().unwrap_or_default().trim().to_string()
}

pub struct Scanner {
    auto: UIAutomation,
}

impl Scanner {
    pub fn new() -> Result<Self> {
        Ok(Scanner { auto: UIAutomation::new()? })
    }

    /// The foreground window + the taskbar. With `all`, every visible app window.
    pub fn visible_elements(&self, all: bool) -> Result<Vec<Element>> {
        let mut roots: Vec<(isize, String)> = Vec::new();
        if all {
            roots.extend(window::list().into_iter().filter(|w| !w.minimized).map(|w| (w.hwnd, w.title)));
        } else if let Some(fg) = window::foreground() {
            roots.push((fg.hwnd, fg.title));
        }
        let taskbar = unsafe { FindWindowW(w!("Shell_TrayWnd"), None) };
        if let Ok(tb) = taskbar {
            roots.push((tb.0 as isize, "taskbar".into()));
        }

        let screen = super::screen::virtual_screen();
        let mut out = Vec::new();
        for (h, title) in roots {
            for (name, kind, rect) in self.scan_window(h)? {
                let (cx, cy) = rect.center();
                if rect.w < 4 || rect.h < 4 || !screen.contains(cx, cy) {
                    continue;
                }
                // If another window is at the centre point, the element is not visible.
                let at = unsafe { GetAncestor(WindowFromPoint(POINT { x: cx, y: cy }), GA_ROOT) };
                if at.0 as isize != h {
                    continue;
                }
                out.push(Element { id: out.len(), name, kind, rect, source: title.clone() });
            }
        }
        Ok(out)
    }

    /// Element at a point: itself or its nearest named ancestor. Tells the panel what is under the crosshair.
    pub fn element_at(&self, x: i32, y: i32) -> Result<Option<Element>> {
        let walker = self.auto.get_control_view_walker()?;
        let mut cur = Some(self.auto.element_from_point(Point::new(x, y))?);
        for _ in 0..4 {
            let Some(e) = cur else { break };
            let name = clean_name(&e.get_name().unwrap_or_default());
            if !name.is_empty() {
                let r = e.get_bounding_rectangle()?;
                let root = unsafe { GetAncestor(WindowFromPoint(POINT { x, y }), GA_ROOT) };
                let w = window::info(root);
                let source = if w.class == "Shell_TrayWnd" { "taskbar".into() } else { w.title };
                let kind = e.get_control_type().map(kind_name).unwrap_or("element");
                let rect = Rect::from_ltrb(r.get_left(), r.get_top(), r.get_right(), r.get_bottom());
                return Ok(Some(Element { id: 0, name, kind, rect, source }));
            }
            cur = walker.get_parent(&e).ok();
        }
        Ok(None)
    }

    fn scan_window(&self, h: isize) -> Result<Vec<(String, &'static str, Rect)>> {
        let cache = self.auto.create_cache_request()?;
        for p in [UIProperty::Name, UIProperty::ControlType, UIProperty::BoundingRectangle, UIProperty::IsOffscreen] {
            cache.add_property(p)?;
        }
        let root = self.auto.element_from_handle(Handle::from(h))?;
        let cond = self.auto.create_true_condition()?;
        let els: Vec<UIElement> = root.find_all_build_cache(TreeScope::Descendants, &cond, &cache)?;
        let mut v = Vec::new();
        for e in els {
            let Ok(ct) = e.get_cached_control_type() else { continue };
            let Some(kind) = kind_of(ct) else { continue };
            if e.is_cached_offscreen().unwrap_or(true) {
                continue;
            }
            let name = clean_name(&e.get_cached_name().unwrap_or_default());
            if name.is_empty() {
                continue;
            }
            let Ok(r) = e.get_cached_bounding_rectangle() else { continue };
            v.push((name, kind, Rect::from_ltrb(r.get_left(), r.get_top(), r.get_right(), r.get_bottom())));
        }
        Ok(v)
    }
}

/// Finds by name: exact match first, then containing (case-insensitive). Best candidates first.
pub fn find_by_name<'a>(elements: &'a [Element], query: &str) -> Vec<&'a Element> {
    let q = query.to_lowercase();
    let mut exact: Vec<&Element> = elements.iter().filter(|e| e.name.to_lowercase() == q).collect();
    if exact.is_empty() {
        exact = elements.iter().filter(|e| e.name.to_lowercase().contains(&q)).collect();
        exact.sort_by_key(|e| e.name.len());
    }
    exact
}
