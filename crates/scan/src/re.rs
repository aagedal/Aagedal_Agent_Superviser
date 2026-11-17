//! Python-`re`-like helpers over `fancy_regex`, so atif-scan's patterns (lookarounds and
//! all) port verbatim. Spans are byte offsets; Python's are characters, but no span ever
//! crosses between the two.

use std::sync::LazyLock;

/// Capture groups of a match in a `str`.
pub type Captures<'t> = fancy_regex::Captures<'t, str>;

pub type Span = (usize, usize);

pub struct Re(fancy_regex::Regex);

impl Re {
    pub fn new(pattern: &str) -> Self {
        let regex = fancy_regex::RegexBuilder::new(pattern)
            .backtrack_limit(10_000_000)
            .delegate_size_limit(256 << 20)
            .delegate_dfa_size_limit(64 << 20)
            .build()
            .unwrap_or_else(|e| panic!("bad pattern {pattern:?}: {e}"));
        Self(regex)
    }

    /// `re.search`.
    pub fn search(&self, text: &str) -> Option<Span> {
        self.search_from(text, 0)
    }

    /// `re.search(text, pos)`: lookbehinds see text before `pos`; `^` doesn't match there.
    pub fn search_from(&self, text: &str, pos: usize) -> Option<Span> {
        match self.0.find_from_pos(text, pos) {
            Ok(Some(m)) => Some((m.start(), m.end())),
            _ => None,
        }
    }

    /// `re.search(text, pos, endpos)`: the text ends at `end`.
    pub fn search_in(&self, text: &str, pos: usize, end: usize) -> Option<Span> {
        self.search_from(&text[..end], pos)
    }

    pub fn is_match(&self, text: &str) -> bool {
        self.search(text).is_some()
    }

    /// `re.match(text, pos)`: a match starting exactly at `pos`.
    pub fn match_at(&self, text: &str, pos: usize) -> Option<Span> {
        self.search_from(text, pos).filter(|(start, _)| *start == pos)
    }

    /// `re.finditer(text, pos, endpos)`.
    pub fn iter_in<'a>(&'a self, text: &'a str, start: usize, end: usize) -> impl Iterator<Item = Span> + 'a {
        let text = &text[..end];
        let mut pos = start;
        std::iter::from_fn(move || {
            if pos > text.len() {
                return None;
            }
            let (s, e) = self.search_from(text, pos)?;
            pos = if e > s { e } else { next_char(text, e) };
            Some((s, e))
        })
    }

    /// Captures of `re.finditer(text, pos, endpos)`.
    pub fn captures_iter_in<'t>(&self, text: &'t str, start: usize, end: usize) -> Vec<Captures<'t>> {
        let text = &text[..end];
        let mut out = Vec::new();
        let mut pos = start;
        while pos <= text.len() {
            let Some(c) = self.captures_from(text, pos) else { break };
            let m = c.get(0).unwrap();
            pos = if m.end() > m.start() { m.end() } else { next_char(text, m.end()) };
            out.push(c);
        }
        out
    }

    /// `re.finditer`.
    pub fn iter<'a>(&'a self, text: &'a str) -> impl Iterator<Item = Span> + 'a {
        self.0.find_iter(text).filter_map(|m| m.ok()).map(|m| (m.start(), m.end()))
    }

    pub fn captures<'t>(&self, text: &'t str) -> Option<Captures<'t>> {
        self.0.captures(text).ok().flatten()
    }

    pub fn captures_from<'t>(&self, text: &'t str, pos: usize) -> Option<Captures<'t>> {
        self.0.captures_from_pos(text, pos).ok().flatten()
    }

    pub fn captures_iter<'a, 't>(&'a self, text: &'t str) -> impl Iterator<Item = Captures<'t>> + 'a
    where
        't: 'a,
    {
        self.0.captures_iter(text).filter_map(|c| c.ok())
    }

    /// `re.findall` for a pattern with one group (or none: the whole match).
    pub fn find_all<'t>(&self, text: &'t str) -> Vec<&'t str> {
        self.captures_iter(text)
            .filter_map(|c| c.get(1).or_else(|| c.get(0)).map(|m| m.as_str()))
            .collect()
    }

    /// `re.split` without groups.
    pub fn split<'t>(&self, text: &'t str) -> Vec<&'t str> {
        let mut parts = Vec::new();
        let mut last = 0;
        for (start, end) in self.iter(text) {
            parts.push(&text[last..start]);
            last = end;
        }
        parts.push(&text[last..]);
        parts
    }
}

fn next_char(text: &str, at: usize) -> usize {
    text[at..].chars().next().map_or(at + 1, |c| at + c.len_utf8())
}

/// The byte offset `n` characters before `at` (or 0): Python's `text[at - n:]` windows.
pub fn chars_back(text: &str, at: usize, n: usize) -> usize {
    text[..at].char_indices().rev().nth(n.saturating_sub(1)).map_or(0, |(i, _)| if n == 0 { at } else { i })
}

/// The byte offset `n` characters after `at` (or the end).
pub fn chars_forward(text: &str, at: usize, n: usize) -> usize {
    text[at..].char_indices().nth(n).map_or(text.len(), |(i, _)| at + i)
}

/// A lazily compiled pattern: `static X: Lazy = lazy(r"…");`.
pub type Lazy = LazyLock<Re>;

/// `re.fullmatch`: the pattern wrapped to cover the whole text.
pub fn full(pattern: &str) -> Re {
    Re::new(&format!(r"\A(?:{pattern})\z"))
}

#[macro_export]
macro_rules! lazy_re {
    ($pattern:expr) => {
        ::std::sync::LazyLock::new(|| $crate::re::Re::new($pattern))
    };
}
