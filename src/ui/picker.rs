#[allow(unused_imports)]
use crate::sync_util::LockExt;
use std::path::{Component, PathBuf};
use std::sync::{Arc, Mutex};

/// Owned snapshot of a picker's display state, handed to the renderer so its
/// candidate list is painted through the ratatui scene — visible on `/dev/tty`,
/// diff-safe, and inheriting the theme background. Previously the pickers wrote
/// raw cursor-positioned escapes to `std::io::stdout()`, which `TerminalGuard`
/// redirects to the log, so the overlay never reached the screen [dirge-92em].
#[derive(Clone, Default)]
pub struct PickerOverlay {
    /// Header line (the `ListPicker` prompt); `None` for the file picker.
    pub title: Option<String>,
    /// Candidate rows as pre-formatted display strings.
    pub rows: Vec<String>,
    /// Index of the highlighted row into `rows`.
    pub selected: usize,
    /// Shown when `rows` is empty (e.g. `"no matches"`).
    pub empty_hint: Option<String>,
}

pub struct ListPicker {
    pub active: bool,
    pub items: Vec<String>,
    pub selected: usize,
    prompt: String,
}

#[derive(Clone)]
pub struct SearchItem<T> {
    pub value: T,
    pub label: String,
    pub search_text: String,
}

pub enum SearchPickerAction<T> {
    None,
    Cancelled,
    Selected(T),
}

/// Typed fuzzy-search picker used by route catalogs and future searchable
/// command surfaces. The renderer only receives owned display rows; callers
/// recover the typed selected value on Enter.
pub struct SearchPicker<T> {
    pub active: bool,
    pub query: String,
    items: Vec<SearchItem<T>>,
    matches: Vec<usize>,
    pub selected: usize,
    prompt: String,
}

impl<T: Clone> SearchPicker<T> {
    pub fn new(prompt: impl Into<String>, items: Vec<SearchItem<T>>, query: String) -> Self {
        let mut picker = Self {
            active: true,
            query,
            items,
            matches: Vec::new(),
            selected: 0,
            prompt: prompt.into(),
        };
        picker.filter();
        picker
    }

    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> SearchPickerAction<T> {
        use crossterm::event::KeyCode;
        match key.code {
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
            }
            KeyCode::Down => {
                if self.selected + 1 < self.matches.len() {
                    self.selected += 1;
                }
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.filter();
            }
            KeyCode::Char(c)
                if !c.is_control()
                    && (key.modifiers.is_empty()
                        || key.modifiers == crossterm::event::KeyModifiers::SHIFT) =>
            {
                self.query.push(c);
                self.filter();
            }
            KeyCode::Enter => {
                if let Some(index) = self.matches.get(self.selected) {
                    self.active = false;
                    return SearchPickerAction::Selected(self.items[*index].value.clone());
                }
            }
            KeyCode::Esc => {
                self.active = false;
                return SearchPickerAction::Cancelled;
            }
            _ => {}
        }
        SearchPickerAction::None
    }

    pub fn append_query(&mut self, text: &str) {
        self.query.extend(text.chars().filter(|c| !c.is_control()));
        self.filter();
    }

    pub fn overlay(&self) -> PickerOverlay {
        PickerOverlay {
            title: Some(if self.query.is_empty() {
                self.prompt.clone()
            } else {
                format!("{} {}", self.prompt, self.query)
            }),
            rows: self
                .matches
                .iter()
                .take(100)
                .map(|index| self.items[*index].label.clone())
                .collect(),
            selected: self.selected,
            empty_hint: Some("no matching models".to_string()),
        }
    }

    fn filter(&mut self) {
        use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization};
        use nucleo_matcher::{Config, Matcher, Utf32Str};

        self.selected = 0;
        if self.query.trim().is_empty() {
            self.matches = (0..self.items.len().min(100)).collect();
            return;
        }
        let atom = Atom::new(
            &self.query,
            CaseMatching::Smart,
            Normalization::Smart,
            AtomKind::Fuzzy,
            false,
        );
        let mut matcher = Matcher::new(Config::DEFAULT);
        let mut buf = Vec::new();
        let mut indices = Vec::new();
        let mut scored = Vec::new();
        for (index, item) in self.items.iter().enumerate() {
            buf.clear();
            indices.clear();
            let haystack = Utf32Str::new(&item.search_text, &mut buf);
            if let Some(score) = atom.indices(haystack, &mut matcher, &mut indices) {
                scored.push((index, score));
            }
        }
        scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        scored.truncate(100);
        self.matches = scored.into_iter().map(|(index, _)| index).collect();
    }
}

impl ListPicker {
    pub fn new() -> Self {
        ListPicker {
            active: false,
            items: Vec::new(),
            selected: 0,
            prompt: String::new(),
        }
    }

    pub fn activate(&mut self, prompt: &str, items: Vec<String>) {
        self.active = true;
        self.prompt = prompt.to_string();
        self.items = items;
        self.selected = 0;
    }

    pub fn deactivate(&mut self) {
        self.active = false;
    }

    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> Option<usize> {
        use crossterm::event::KeyCode;
        match key.code {
            KeyCode::Up => {
                if self.selected > 0 {
                    self.selected -= 1;
                }
                None
            }
            KeyCode::Down => {
                if self.selected + 1 < self.items.len() {
                    self.selected += 1;
                }
                None
            }
            KeyCode::Enter => {
                if self.items.is_empty() {
                    self.active = false;
                    None
                } else {
                    let result = Some(self.selected);
                    self.active = false;
                    result
                }
            }
            KeyCode::Esc => {
                self.active = false;
                None
            }
            _ => None,
        }
    }

    /// Snapshot for the renderer to paint through the ratatui scene.
    pub fn overlay(&self) -> PickerOverlay {
        PickerOverlay {
            title: (!self.prompt.is_empty()).then(|| self.prompt.clone()),
            rows: self.items.clone(),
            selected: self.selected,
            empty_hint: None,
        }
    }
}

pub struct FilePicker {
    pub active: bool,
    pub query: String,
    pub cursor: usize,
    pub matches: Vec<PathBuf>,
    pub selected: usize,
    file_cache: Arc<Mutex<Vec<PathBuf>>>,
}

impl FilePicker {
    pub fn new() -> Self {
        FilePicker {
            active: false,
            query: String::new(),
            cursor: 0,
            matches: Vec::new(),
            selected: 0,
            file_cache: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn activate(&mut self) {
        self.active = true;
        self.query.clear();
        self.cursor = 0;
        self.matches.clear();
        self.selected = 0;
        self.load_files();
        self.filter();
    }

    pub fn deactivate(&mut self) {
        self.active = false;
    }

    fn load_files(&mut self) {
        let files = walk_files(".");
        *self.file_cache.lock_ignore_poison() = files;
    }

    pub fn char_input(&mut self, c: char) {
        // Convert char-index cursor to byte index for String::insert
        let byte_pos = self
            .query
            .char_indices()
            .nth(self.cursor)
            .map(|(i, _)| i)
            .unwrap_or(self.query.len());
        self.query.insert(byte_pos, c);
        self.cursor += 1;
        self.filter();
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 && !self.query.is_empty() {
            self.cursor -= 1;
            // Convert char-index cursor to byte index for String::remove
            let byte_pos = self
                .query
                .char_indices()
                .nth(self.cursor)
                .map(|(i, _)| i)
                .unwrap_or(self.query.len());
            self.query.remove(byte_pos);
            self.filter();
        }
    }

    fn filter(&mut self) {
        let cache = self.file_cache.lock_ignore_poison();
        if cache.is_empty() {
            self.matches.clear();
            return;
        }
        let query_lower = self.query.to_lowercase();
        self.matches = cache
            .iter()
            .filter(|p| {
                let lower = p.to_string_lossy().to_lowercase();
                lower.contains(&query_lower)
            })
            .take(50)
            .cloned()
            .collect();
        self.selected = 0;
    }

    pub fn select_next(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + 1) % self.matches.len();
        }
    }

    pub fn select_prev(&mut self) {
        if !self.matches.is_empty() {
            self.selected = if self.selected == 0 {
                self.matches.len() - 1
            } else {
                self.selected - 1
            };
        }
    }

    pub fn selected_path(&self) -> Option<&PathBuf> {
        self.matches.get(self.selected)
    }

    #[cfg(test)]
    pub fn test_set_cache(&mut self, files: Vec<PathBuf>) {
        *self.file_cache.lock_ignore_poison() = files;
    }

    /// Snapshot for the renderer to paint above the input box through the
    /// ratatui scene. Rows are the matched paths; an empty match set surfaces
    /// the `"no matches"` hint.
    pub fn overlay(&self) -> PickerOverlay {
        PickerOverlay {
            title: None,
            rows: self
                .matches
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
            selected: self.selected,
            empty_hint: Some("no matches".to_string()),
        }
    }
}

fn walk_files(root: &str) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .max_depth(Some(8))
        .sort_by_file_name(|a, b| a.cmp(b))
        .build();

    for entry in walker.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path
            .components()
            .any(|c| matches!(c, Component::Normal(n) if n.to_string_lossy().starts_with('.')))
        {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        let rel = rel.trim_start_matches('/').to_string();
        files.push(PathBuf::from(rel));
        if files.len() >= 200 {
            break;
        }
    }
    files
}

#[cfg(test)]
mod search_picker_tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn picker(query: &str) -> SearchPicker<&'static str> {
        SearchPicker::new(
            "Models:",
            vec![
                SearchItem {
                    value: "claude",
                    label: "anthropic claude-sonnet".into(),
                    search_text: "anthropic claude-sonnet Claude Sonnet".into(),
                },
                SearchItem {
                    value: "gpt",
                    label: "openai gpt-5".into(),
                    search_text: "openai gpt-5".into(),
                },
            ],
            query.into(),
        )
    }

    #[test]
    fn fuzzy_query_filters_and_enter_returns_typed_value() {
        let mut picker = picker("cld snnt");
        assert_eq!(picker.overlay().rows, ["anthropic claude-sonnet"]);
        assert!(matches!(
            picker.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            SearchPickerAction::Selected("claude")
        ));
    }

    #[test]
    fn editing_navigation_and_cancel_are_safe() {
        let mut picker = picker("");
        let _ = picker.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(picker.selected, 1);
        let _ = picker.handle_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT));
        assert_eq!(picker.query, "G");
        let _ = picker.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert!(picker.query.is_empty());
        assert!(matches!(
            picker.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            SearchPickerAction::Cancelled
        ));
    }

    #[test]
    fn empty_results_enter_does_not_panic() {
        let mut picker = picker("zzzz-no-model");
        assert!(picker.overlay().rows.is_empty());
        assert!(matches!(
            picker.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            SearchPickerAction::None
        ));
    }
}
