//! Static reader for shell command text (nothing is ever executed). Port of atif-scan's
//! `data/shell.py`.
//!
//! `parse(text)` splits a shell tool's command into simple commands: their words (quotes
//! removed), redirections, pipeline membership, background/repeated scopes, and nested
//! commands (`$(…)`, `<(…)`, backticks, subshells, `sh -c '…'`, a heredoc fed to a shell).
//! Heredoc bodies given to any other program are file contents, returned as `bodies`.
//! Input it can't structure sets `complete = false`; callers then fall back to text patterns.
//!
//! Commands live in an arena (`Script::command`) and refer to each other by index, where
//! Python shares objects. Spans are byte offsets.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

pub type Span = (usize, usize);
pub type Id = usize;

pub const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "ash"];
/// Words before the command name that don't name it.
const KEYWORDS: &[&str] = &["!", "{", "}", "then", "do", "else", "elif", "if", "while", "until", "time", "exec"];
/// Wrappers that run the next word, and their options that take a value.
const WRAPPERS: &[(&str, &[&str])] = &[
    ("sudo", &["-u", "-g", "-C", "-D", "-h", "-p", "-r", "-t", "-U"]),
    ("env", &["-u", "-C", "-S"]),
    ("nohup", &[]),
    ("command", &[]),
    ("builtin", &[]),
    ("nice", &["-n"]),
    ("stdbuf", &["-i", "-o", "-e"]),
    ("timeout", &["-s", "-k", "--signal", "--kill-after"]),
];
const WRITE_OPS: &[&str] = &[">", ">>", ">|", "&>", "&>>", "<>", ">&"];
/// Commands whose heredoc body is written out verbatim.
pub const FILE_WRITERS: &[&str] = &["cat", "tee"];
const MAX_DEPTH: usize = 32;

fn is_word_end(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b';' | b'&' | b'|' | b'<' | b'>' | b'(' | b')')
}

/// `[A-Za-z_]\w*=` at the start of `word`.
pub fn is_assignment(word: &str) -> bool {
    let mut chars = word.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') {
        return false;
    }
    for c in chars {
        if c == '=' {
            return true;
        }
        if !(c.is_alphanumeric() || c == '_') {
            return false;
        }
    }
    false
}

/// `-[a-z]*c[a-z]*`: `-c`, `-lc`, `-ec`, …
pub fn is_shell_c(word: &str) -> bool {
    word.strip_prefix('-').is_some_and(|rest| rest.contains('c') && rest.bytes().all(|b| b.is_ascii_lowercase()))
}

fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

#[derive(Debug, Clone, Default)]
pub struct Command {
    pub words: Vec<String>,
    pub spans: Vec<Span>,
    /// (operator, target, span).
    pub redirects: Vec<(String, String, Span)>,
    /// Commands this one runs: an `sh -c` string or a heredoc fed to a shell.
    pub nested: Vec<Id>,
    /// Command and process substitutions in its words.
    pub substituted: Vec<Id>,
    pub pipeline: usize,
    pub span: Span,
    pub background: bool,
    pub repeated: bool,
}

impl Command {
    fn with_pipeline(pipeline: usize) -> Self {
        Self { pipeline, ..Default::default() }
    }

    /// (word, span) from the command name on: assignments, keywords and wrappers skipped.
    pub fn argv(&self) -> Vec<(&str, Span)> {
        let words: Vec<(&str, Span)> = self.words.iter().map(String::as_str).zip(self.spans.iter().copied()).collect();
        let mut i = 0;
        while i < words.len() {
            let word = words[i].0;
            let base = basename(word);
            if KEYWORDS.contains(&word) || is_assignment(word) {
                i += 1;
            } else if let Some((_, takes_value)) = WRAPPERS.iter().find(|(w, _)| *w == base) {
                i += 1;
                while i < words.len() && (words[i].0.starts_with('-') || is_assignment(words[i].0)) {
                    i += if takes_value.contains(&words[i].0) { 2 } else { 1 };
                }
                if base == "timeout" && i < words.len() {
                    i += 1; // the duration
                }
            } else {
                break;
            }
        }
        words.into_iter().skip(i).collect()
    }

    /// The command's basename (`/usr/bin/curl` → `curl`); a substitution run as the
    /// command keeps its text.
    pub fn name(&self) -> Option<&str> {
        let argv = self.argv();
        let word = argv.first()?.0;
        Some(if word.starts_with("$(") || word.starts_with('`') { word } else { basename(word) })
    }
}

#[derive(Debug, Clone)]
pub struct Script {
    arena: Vec<Command>,
    /// Every command, nested included, in the order read.
    pub order: Vec<Id>,
    /// Heredoc contents (not commands), and the commands owning them (parallel).
    pub bodies: Vec<Span>,
    pub body_commands: Vec<Id>,
    /// The bodies `cat`/`tee` write to a file, and those files (parallel).
    pub file_bodies: Vec<Span>,
    pub file_targets: Vec<String>,
    pub complete: bool,
    pipelines: usize,
}

impl Script {
    fn new() -> Self {
        Self { arena: Vec::new(), order: Vec::new(), bodies: Vec::new(), body_commands: Vec::new(), file_bodies: Vec::new(), file_targets: Vec::new(), complete: true, pipelines: 0 }
    }

    pub fn command(&self, id: Id) -> &Command {
        &self.arena[id]
    }

    /// Every command, nested included.
    pub fn commands(&self) -> impl Iterator<Item = &Command> {
        self.order.iter().map(|&id| &self.arena[id])
    }

    pub fn commands_with_ids(&self) -> impl Iterator<Item = (Id, &Command)> {
        self.order.iter().map(|&id| (id, &self.arena[id]))
    }

    fn alloc(&mut self, command: Command) -> Id {
        self.arena.push(command);
        self.arena.len() - 1
    }

    fn next_pipeline(&mut self) -> usize {
        self.pipelines += 1;
        self.pipelines
    }
}

thread_local! {
    static CACHE: RefCell<HashMap<String, Rc<Script>>> = RefCell::new(HashMap::new());
}

/// Parses `text`. Never fails: anything the reader trips over makes the script
/// incomplete. Cached per thread (several checks read the same command).
pub fn parse(text: &str) -> Rc<Script> {
    if let Some(found) = CACHE.with(|c| c.borrow().get(text).cloned()) {
        return found;
    }
    let mut script = Script::new();
    let mut reader = Reader::new(text, 0);
    reader.commands(&mut script, 0, None, None);
    script.complete = reader.complete;
    propagate_scope(&mut script);
    let script = Rc::new(script);
    CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        if cache.len() >= 512 {
            cache.clear();
        }
        cache.insert(text.to_string(), script.clone());
    });
    script
}

fn mark_scope(script: &mut Script, ids: &[Id], background: bool, repeated: bool) {
    for &id in ids {
        script.arena[id].background |= background;
        script.arena[id].repeated |= repeated;
    }
}

/// Children can precede their parent, or be read later (shell heredocs).
fn propagate_scope(script: &mut Script) {
    let mut pending: Vec<Id> = script.order.clone();
    while let Some(parent) = pending.pop() {
        let (background, repeated) = (script.arena[parent].background, script.arena[parent].repeated);
        let children: Vec<Id> = script.arena[parent].nested.iter().chain(&script.arena[parent].substituted).copied().collect();
        for child in children {
            let bg = background && !script.arena[child].background;
            let rep = repeated && !script.arena[child].repeated;
            if bg || rep {
                script.arena[child].background |= bg;
                script.arena[child].repeated |= rep;
                pending.push(child);
            }
        }
    }
}

/// What `word` does after reading one part.
#[derive(PartialEq)]
enum Next {
    Continue,
    Fail,
    Stop,
}

/// The command `commands` is reading, where it started, and its pipeline.
struct Pending {
    pipeline: usize,
    start: usize,
    command: Id,
    /// The current AND/OR list, groups included.
    members: Vec<Id>,
    grouped: bool,
    /// After a pipeline or AND/OR operator.
    needs_command: bool,
}

impl Pending {
    fn new(script: &mut Script, pipeline: usize, start: usize) -> Self {
        let command = script.alloc(Command::with_pipeline(pipeline));
        Self { pipeline, start, command, members: Vec::new(), grouped: false, needs_command: false }
    }

    fn empty(&self, script: &Script) -> bool {
        let c = &script.arena[self.command];
        !(!c.words.is_empty() || !c.redirects.is_empty() || self.grouped)
    }
}

const REDIRECT_OPS: &[&str] = &["&>>", "&>", ">>", ">|", ">&", "<<-", "<<<", "<<", "<&", "<>", ">", "<"];
const OPERATORS: &[&str] = &["&&", "||", ";;", "|&", "|", ";", "&"];

struct Reader<'t> {
    text: &'t str,
    bytes: &'t [u8],
    base: usize,
    pos: usize,
    /// Inside an `sh -c` string: substitutions are its own.
    literal: bool,
    heredocs: Vec<(String, bool, Id)>,
    /// This reader's script is complete (a nested reader's joins its parent's).
    complete: bool,
}

impl<'t> Reader<'t> {
    fn new(text: &'t str, base: usize) -> Self {
        Self { text, bytes: text.as_bytes(), base, pos: 0, literal: false, heredocs: Vec::new(), complete: true }
    }

    fn len(&self) -> usize {
        self.bytes.len()
    }

    fn byte(&self, i: usize) -> Option<u8> {
        self.bytes.get(i).copied()
    }

    fn starts(&self, s: &str, at: usize) -> bool {
        self.bytes.get(at..).is_some_and(|rest| rest.starts_with(s.as_bytes()))
    }

    fn slice(&self, start: usize, end: usize) -> &'t str {
        let end = end.min(self.len());
        self.text.get(start.min(end)..end).unwrap_or("")
    }

    /// The character at `i` and its length in bytes.
    fn char_at(&self, i: usize) -> Option<(char, usize)> {
        self.text.get(i..)?.chars().next().map(|c| (c, c.len_utf8()))
    }

    fn span(&self, start: usize, end: usize) -> Span {
        (self.base + start, self.base + end)
    }

    fn fail(&mut self) {
        self.complete = false;
        self.pos = self.len();
    }

    /// `\d*` then a redirection operator, at `at`: its end and the operator.
    fn redirect_at(&self, at: usize) -> Option<(usize, &'static str)> {
        let mut i = at;
        while self.byte(i).is_some_and(|b| b.is_ascii_digit()) {
            i += 1;
        }
        REDIRECT_OPS.iter().find(|op| self.starts(op, i)).map(|op| (i + op.len(), *op))
    }

    fn operator_at(&self, at: usize) -> Option<&'static str> {
        OPERATORS.iter().find(|op| self.starts(op, at)).copied()
    }

    /// Read a list, returning whether its closing token (or the end) was reached.
    fn commands(&mut self, s: &mut Script, depth: usize, stop: Option<&str>, pipeline: Option<usize>) -> bool {
        if depth > MAX_DEPTH {
            self.fail();
            return false;
        }
        let pipeline = pipeline.unwrap_or_else(|| s.next_pipeline());
        let mut pending = Pending::new(s, pipeline, self.pos);
        while self.pos < self.len() {
            if !self.command_part(s, &mut pending, depth, stop) {
                return self.complete;
            }
        }
        self.finish(s, &mut pending, true);
        if stop.is_some() || pending.needs_command {
            self.fail();
        }
        self.complete
    }

    /// Record the command being read (if it has anything) and start the next one.
    fn finish(&mut self, s: &mut Script, pending: &mut Pending, new_pipeline: bool) {
        let id = pending.command;
        if !s.arena[id].words.is_empty() || !s.arena[id].redirects.is_empty() {
            s.arena[id].span = self.span(pending.start, self.pos);
            s.order.push(id);
            pending.members.push(id);
        }
        if new_pipeline {
            pending.pipeline = s.next_pipeline();
            pending.members.clear();
        }
        pending.grouped = false;
        pending.command = s.alloc(Command::with_pipeline(pending.pipeline));
        pending.start = self.pos;
    }

    /// Read one blank, separator or token; false when `commands` is done.
    fn command_part(&mut self, s: &mut Script, pending: &mut Pending, depth: usize, stop: Option<&str>) -> bool {
        if self.blank(s, pending) || self.separator(s, pending, depth) {
            return true;
        }
        if self.at_stop(s, pending, stop) {
            if pending.needs_command {
                self.fail();
            } else {
                self.finish(s, pending, true);
                self.pos += stop.map_or(0, str::len);
            }
            return false;
        }
        let read = self.compound_part(s, pending, depth);
        pending.needs_command = false;
        read
    }

    /// An unquoted, whole reserved word at the current token boundary.
    fn keyword(&self, word: &str) -> bool {
        let end = self.pos + word.len();
        self.starts(word, self.pos) && (end == self.len() || self.byte(end).is_some_and(is_word_end))
    }

    fn at_stop(&self, s: &Script, pending: &Pending, stop: Option<&str>) -> bool {
        match stop {
            None => false,
            Some(")") => self.byte(self.pos) == Some(b')'),
            Some(stop) => pending.empty(s) && self.keyword(stop),
        }
    }

    /// Keep compound commands together for a following pipeline or `&`.
    fn compound_part(&mut self, s: &mut Script, pending: &mut Pending, depth: usize) -> bool {
        if self.compound_start(s, pending) {
            if self.byte(self.pos) == Some(b'(') || self.keyword("{") {
                return self.group(s, pending, depth);
            }
            for kind in ["while", "until", "for"] {
                if self.keyword(kind) {
                    return self.lp(s, pending, depth, kind);
                }
            }
        }
        if self.misplaced_token(s, pending) {
            self.fail();
            return false;
        }
        self.token(s, pending.command, depth)
    }

    /// Reserved prefixes can introduce compounds, but quoted words cannot.
    fn compound_start(&self, s: &Script, pending: &Pending) -> bool {
        let command = &s.arena[pending.command];
        !(pending.grouped || !command.redirects.is_empty())
            && command.words.iter().zip(&command.spans).all(|(word, (start, end))| {
                ["if", "then", "else", "elif", "!", "time"].contains(&word.as_str()) && self.slice(start - self.base, end - self.base) == word
            })
    }

    fn misplaced_token(&self, s: &Script, pending: &Pending) -> bool {
        if pending.empty(s) && ["do", "done", "}"].iter().any(|w| self.keyword(w)) {
            return true;
        }
        pending.grouped && self.redirect_at(self.pos).is_none()
    }

    fn group(&mut self, s: &mut Script, pending: &mut Pending, depth: usize) -> bool {
        let stop = if self.byte(self.pos) == Some(b'(') { ")" } else { "}" };
        let before = s.order.len();
        self.pos += 1;
        // The group's first pipeline shares its surrounding pipe, not substitutions.
        let closed = self.commands(s, depth + 1, Some(stop), Some(pending.pipeline));
        pending.command = s.alloc(Command::with_pipeline(pending.pipeline));
        pending.members.extend_from_slice(&s.order[before..]);
        pending.grouped = true;
        closed
    }

    /// Structure loop headers/bodies; only exact constant headers are unbounded.
    fn lp(&mut self, s: &mut Script, pending: &mut Pending, depth: usize, kind: &str) -> bool {
        let before = s.order.len();
        self.pos += kind.len();
        let Some(repeated) = self.loop_header(s, depth, kind) else {
            self.fail();
            return false;
        };
        let body = s.order.len();
        let closed = self.commands(s, depth + 1, Some("done"), None);
        let members: Vec<Id> = s.order[before..].to_vec();
        // A following redirect belongs to the loop, even though it has no command name.
        pending.command = s.alloc(Command { pipeline: pending.pipeline, repeated: closed && repeated, ..Default::default() });
        if closed {
            let ids: Vec<Id> = s.order[body..].to_vec();
            mark_scope(s, &ids, false, repeated);
        }
        pending.members.extend(members);
        pending.grouped = true;
        closed
    }

    /// Whether the header is unbounded; `None` means unsupported or incomplete.
    fn loop_header(&mut self, s: &mut Script, depth: usize, kind: &str) -> Option<bool> {
        if kind == "for" {
            let start = self.pos - kind.len();
            self.header_space(s, true);
            if self.starts("((", self.pos) {
                return self.arithmetic_loop_header(s).then_some(true);
            }
            return self.word_loop_header(s, depth, start).then_some(false);
        }
        let before = s.order.len();
        if !self.commands(s, depth + 1, Some("do"), None) || s.order.len() == before {
            return None;
        }
        let ids: Vec<Id> = s.order[before..].to_vec();
        Some(self.constant_condition(s, &ids, kind))
    }

    /// Keep the existing `for name [in words]` header and its nested expansions.
    fn word_loop_header(&mut self, s: &mut Script, depth: usize, start: usize) -> bool {
        let pipeline = s.next_pipeline();
        let mut pending = Pending::new(s, pipeline, start);
        let id = pending.command;
        s.arena[id].words.push("for".into());
        let span = self.span(start, start + 3);
        s.arena[id].spans.push(span);
        if !self.word_loop_words(s, &mut pending, depth) {
            return false;
        }
        if self.word_loop_continuation(s, id) && !self.word_loop_words(s, &mut pending, depth) {
            return false;
        }
        if !self.valid_word_loop(s, id) {
            return false;
        }
        self.finish(s, &mut pending, true);
        self.pos += 1; // the required semicolon/newline before `do`
        self.loop_do(s)
    }

    /// Only a newline after the variable may precede the optional `in` clause.
    fn word_loop_continuation(&mut self, s: &mut Script, id: Id) -> bool {
        if s.arena[id].words.len() - 1 != 1 || !self.starts("\n", self.pos) {
            return false;
        }
        let start = self.pos;
        self.header_space(s, true);
        if self.keyword("in") {
            return true;
        }
        self.pos = start;
        false
    }

    fn word_loop_words(&mut self, s: &mut Script, pending: &mut Pending, depth: usize) -> bool {
        while self.pos < self.len() && !matches!(self.byte(self.pos), Some(b';' | b'\n')) {
            if self.blank(s, pending) {
                continue;
            }
            if self.byte(self.pos).is_some_and(is_word_end) {
                return false;
            }
            let Some((word, span)) = self.word(s, depth, pending.command, false) else { return false };
            s.arena[pending.command].words.push(word);
            s.arena[pending.command].spans.push(span);
        }
        true
    }

    fn valid_word_loop(&self, s: &Script, id: Id) -> bool {
        let command = &s.arena[id];
        let words = &command.words[1..];
        let Some(first) = words.first() else { return false };
        let mut chars = first.chars();
        if !chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return false;
        }
        if words.len() > 1 && words[1] != "in" {
            return false;
        }
        words.iter().take(2).zip(command.spans.iter().skip(1).take(2)).all(|(word, (start, end))| self.slice(start - self.base, end - self.base) == word)
    }

    fn constant_condition(&self, s: &Script, ids: &[Id], kind: &str) -> bool {
        let [id] = ids else { return false };
        let command = &s.arena[*id];
        let words: Vec<&str> = command.words.iter().map(String::as_str).collect();
        let expected: &[&[&str]] = if kind == "until" { &[&["false"]] } else { &[&["true"], &[":"], &["[", "1", "]"]] };
        let literal = command.words.iter().zip(&command.spans).all(|(word, (start, end))| self.slice(start - self.base, end - self.base) == word);
        literal && command.redirects.is_empty() && expected.contains(&words.as_slice())
    }

    /// Layout in the deliberately narrow `for ((;;))` header.
    fn header_space(&mut self, s: &mut Script, comments: bool) {
        let pipeline = s.next_pipeline();
        let mut pending = Pending::new(s, pipeline, self.pos);
        while self.pos < self.len() {
            if self.byte(self.pos) == Some(b'\n') {
                self.pos += 1;
            } else if (!comments && self.byte(self.pos) == Some(b'#')) || !self.blank(s, &mut pending) {
                break;
            }
        }
    }

    fn arithmetic_loop_header(&mut self, s: &mut Script) -> bool {
        for token in ["((", ";", ";", "))"] {
            self.header_space(s, false);
            if !self.starts(token, self.pos) {
                return false;
            }
            self.pos += token.len();
        }
        self.header_space(s, true);
        if self.starts(";", self.pos) {
            self.pos += 1;
        }
        self.loop_do(s)
    }

    fn loop_do(&mut self, s: &mut Script) -> bool {
        self.header_space(s, true);
        if !self.keyword("do") {
            return false;
        }
        self.pos += 2;
        true
    }

    /// Skip a space/tab, line continuation or comment at `pos`.
    fn blank(&mut self, s: &Script, pending: &mut Pending) -> bool {
        let start = self.pos;
        let mut i = start;
        while matches!(self.byte(i), Some(b' ' | b'\t')) {
            i += 1;
        }
        if i > start {
            self.pos = i;
            let command = &s.arena[pending.command];
            if command.words.is_empty() && command.redirects.is_empty() {
                pending.start = self.pos;
            }
            return true;
        }
        if self.starts("\\\n", self.pos) {
            self.pos += 2;
            return true;
        }
        if self.byte(self.pos) == Some(b'#') {
            self.pos = self.text[self.pos..].find('\n').map_or(self.len(), |n| self.pos + n);
            return true;
        }
        false
    }

    /// A newline (then the heredoc bodies it opens) or a control operator at `pos`.
    fn separator(&mut self, s: &mut Script, pending: &mut Pending, depth: usize) -> bool {
        if self.byte(self.pos) == Some(b'\n') {
            self.newline(s, pending, depth);
            return true;
        }
        let Some(op) = self.operator_at(self.pos) else { return false };
        if self.starts("&>", self.pos) {
            return false;
        }
        if pending.needs_command || op == ";;" {
            self.fail();
            return true;
        }
        self.pos += op.len();
        self.finish(s, pending, false);
        self.end_operator(s, pending, op);
        true
    }

    fn newline(&mut self, s: &mut Script, pending: &mut Pending, depth: usize) {
        if !pending.needs_command {
            self.finish(s, pending, true);
        }
        self.pos += 1;
        self.read_heredocs(s, depth);
        pending.start = self.pos;
    }

    fn end_operator(&mut self, s: &mut Script, pending: &mut Pending, op: &str) {
        if op == "&" {
            let members = pending.members.clone();
            mark_scope(s, &members, true, false);
        }
        pending.needs_command = matches!(op, "|" | "|&" | "&&" | "||");
        if !matches!(op, "|" | "|&") {
            // An asynchronous AND/OR list also belongs to the same `&`.
            if !matches!(op, "&&" | "||") {
                pending.members.clear();
            }
            pending.pipeline = s.next_pipeline();
            s.arena[pending.command].pipeline = pending.pipeline;
        }
    }

    /// A process substitution, redirection, subshell or word of `command`.
    fn token(&mut self, s: &mut Script, command: Id, depth: usize) -> bool {
        let c = self.byte(self.pos);
        if matches!(c, Some(b'<' | b'>')) && self.starts("(", self.pos + 1) {
            self.substitution(s, command, depth, self.pos, self.pos + 2);
            return true;
        }
        // At a token start, so a leading fd number (`2>`) belongs to the redirection.
        if let Some((end, op)) = self.redirect_at(self.pos) {
            let at = self.pos;
            self.pos = end;
            self.redirect(s, command, op, at, depth);
            return true;
        }
        if matches!(c, Some(b'(' | b')')) {
            self.fail(); // a function, case pattern, or misplaced parenthesis
            return false;
        }
        match self.word(s, depth, command, true) {
            Some((word, span)) => {
                s.arena[command].words.push(word);
                s.arena[command].spans.push(span);
                true
            }
            None => false,
        }
    }

    fn redirect(&mut self, s: &mut Script, command: Id, op: &str, at: usize, depth: usize) {
        while matches!(self.byte(self.pos), Some(b' ' | b'\t')) {
            self.pos += 1;
        }
        let Some((word, _)) = self.word(s, depth, command, false) else { return };
        let span = self.span(at, self.pos);
        s.arena[command].redirects.push((op.to_string(), word.clone(), span));
        if op == "<<" || op == "<<-" {
            self.heredocs.push((word, op == "<<-", command));
        }
    }

    /// Bodies of the heredocs opened on the line just ended, in order.
    fn read_heredocs(&mut self, s: &mut Script, depth: usize) {
        let heredocs = std::mem::take(&mut self.heredocs);
        for (delimiter, strip_tabs, command) in heredocs {
            let body = self.pos;
            let (mut end, mut close) = (self.len(), self.len());
            let mut line = self.pos;
            while line < self.len() {
                let stop = self.text[line..].find('\n').map_or(self.len(), |n| line + n);
                let current = &self.text[line..stop];
                let current = if strip_tabs { current.trim_start_matches('\t') } else { current };
                if current == delimiter {
                    (end, close) = (line, stop);
                    break;
                }
                line = stop + 1;
            }
            self.pos = close;
            let name = s.arena[command].name().map(str::to_string);
            let shell = name.as_deref().is_some_and(|n| SHELLS.contains(&n));
            if shell && !s.arena[command].words.iter().any(|w| is_shell_c(w)) {
                let text = &self.text[body..end];
                self.nested(s, command, text, body, depth, false);
            } else {
                s.bodies.push(self.span(body, end));
                s.body_commands.push(command);
                if name.as_deref().is_some_and(|n| FILE_WRITERS.contains(&n))
                    && let Some((target, _)) = writes(&s.arena[command]).into_iter().next()
                {
                    s.file_bodies.push(self.span(body, end));
                    s.file_targets.push(target);
                }
            }
        }
    }

    /// Commands in `text` (a backtick body, heredoc or `sh -c` string) run by `command`,
    /// read with their own reader; spans stay absolute.
    fn nested(&mut self, s: &mut Script, command: Id, text: &str, offset: usize, depth: usize, substituted: bool) {
        if depth > MAX_DEPTH {
            self.fail();
            return;
        }
        let before = s.order.len();
        let mut reader = Reader::new(text, self.base + offset);
        reader.commands(s, depth + 1, None, None);
        self.complete &= reader.complete;
        let inner: Vec<Id> = s.order[before..].to_vec();
        let target = &mut s.arena[command];
        if substituted { target.substituted.extend(inner) } else { target.nested.extend(inner) }
    }

    /// `$(…)`, `<(…)` or `>(…)` starting at `start`: its commands belong to `command`.
    fn substitution(&mut self, s: &mut Script, command: Id, depth: usize, start: usize, inner: usize) {
        let before = s.order.len();
        self.pos = inner;
        self.commands(s, depth + 1, Some(")"), None);
        let found: Vec<Id> = s.order[before..].to_vec();
        let word = self.slice(start, self.pos).to_string();
        let span = self.span(start, self.pos);
        let target = &mut s.arena[command];
        target.substituted.extend(found);
        target.words.push(word);
        target.spans.push(span);
    }

    /// One word, quotes removed; substitutions inside it are read as commands.
    fn word(&mut self, s: &mut Script, depth: usize, command: Id, operand: bool) -> Option<(String, Span)> {
        let start = self.pos;
        let mut out = String::new();
        // `sh -c '…'`: the string is a script the shell runs. Read it literally here and
        // parse it once, as a script.
        let script = operand && start < self.len() && runs_script(&s.arena[command]);
        let outer = std::mem::replace(&mut self.literal, script);
        while self.pos < self.len() && !self.byte(self.pos).is_some_and(is_word_end) {
            let plain_end = self.plain_end(self.pos, |b| !(is_word_end(b) || matches!(b, b'\\' | b'\'' | b'"' | b'$' | b'`')));
            if plain_end > self.pos {
                out.push_str(&self.text[self.pos..plain_end]);
                self.pos = plain_end;
                continue;
            }
            match self.word_part(s, &mut out, depth, command) {
                Next::Continue => {}
                Next::Fail => {
                    self.fail();
                    return None;
                }
                Next::Stop => return None,
            }
        }
        self.literal = outer;
        if script {
            let quoted = matches!(self.byte(start), Some(b'\'' | b'"'));
            self.nested(s, command, &out.clone(), start + usize::from(quoted), depth, false);
        }
        Some((out, self.span(start, self.pos)))
    }

    /// The end of the run of bytes from `at` that `keep` accepts (non-ASCII bytes always).
    fn plain_end(&self, at: usize, keep: impl Fn(u8) -> bool) -> usize {
        let mut i = at;
        while let Some(b) = self.byte(i) {
            if b >= 0x80 || keep(b) {
                i += 1;
            } else {
                break;
            }
        }
        i
    }

    fn word_part(&mut self, s: &mut Script, out: &mut String, depth: usize, command: Id) -> Next {
        let c = self.byte(self.pos);
        if self.literal && matches!(c, Some(b'$' | b'`')) {
            return self.keep(out);
        }
        if let Some(next) = self.expansion(s, out, depth, command) {
            return next;
        }
        match c {
            Some(b'\\') => self.escaped(out),
            Some(b'\'') => self.single_quoted(out),
            Some(b'"') => {
                if self.double_quoted(s, out, depth, command) { Next::Continue } else { Next::Fail }
            }
            Some(b'$') => self.parameter(out),
            _ => self.keep(out),
        }
    }

    /// The character at `pos`, as it is.
    fn keep(&mut self, out: &mut String) -> Next {
        if let Some((c, len)) = self.char_at(self.pos) {
            out.push(c);
            self.pos += len;
        } else {
            self.pos += 1;
        }
        Next::Continue
    }

    fn escaped(&mut self, out: &mut String) -> Next {
        let next = self.char_at(self.pos + 1);
        if next.is_some_and(|(c, _)| c != '\n') {
            out.push(next.unwrap().0);
        }
        self.pos = (self.pos + 1 + next.map_or(1, |(_, l)| l)).min(self.len());
        Next::Continue
    }

    fn single_quoted(&mut self, out: &mut String) -> Next {
        let Some(end) = self.text[self.pos + 1..].find('\'').map(|n| self.pos + 1 + n) else { return Next::Fail };
        out.push_str(&self.text[self.pos + 1..end]);
        self.pos = end + 1;
        Next::Continue
    }

    /// `${…}` kept as text; any other `$` is a plain character.
    fn parameter(&mut self, out: &mut String) -> Next {
        if !self.starts("${", self.pos) {
            return self.keep(out);
        }
        let Some(end) = self.text[self.pos..].find('}').map(|n| self.pos + n) else { return Next::Fail };
        out.push_str(&self.text[self.pos..=end]);
        self.pos = end + 1;
        Next::Continue
    }

    /// `$((…))`, `$(…)` or a backtick substitution at `pos`.
    fn expansion(&mut self, s: &mut Script, out: &mut String, depth: usize, command: Id) -> Option<Next> {
        if self.starts("$((", self.pos) {
            return Some(if self.balanced(out, 1) { Next::Continue } else { Next::Fail });
        }
        if self.starts("$(", self.pos) {
            return Some(if self.command_substitution(s, out, depth, command) { Next::Continue } else { Next::Stop });
        }
        if self.starts("`", self.pos) {
            return Some(if self.backticks(s, out, depth, command) { Next::Continue } else { Next::Fail });
        }
        None
    }

    fn command_substitution(&mut self, s: &mut Script, out: &mut String, depth: usize, command: Id) -> bool {
        let start = self.pos;
        let before = s.order.len();
        self.pos += 2;
        self.commands(s, depth + 1, Some(")"), None);
        let found: Vec<Id> = s.order[before..].to_vec();
        s.arena[command].substituted.extend(found);
        out.push_str(self.slice(start, self.pos));
        self.complete
    }

    fn backticks(&mut self, s: &mut Script, out: &mut String, depth: usize, command: Id) -> bool {
        let mut end = self.pos + 1;
        while end < self.len() && self.byte(end) != Some(b'`') {
            end += if self.byte(end) == Some(b'\\') { 2 } else { 1 };
        }
        if end >= self.len() {
            return false;
        }
        let inner = &self.text[self.pos + 1..end];
        self.nested(s, command, inner, self.pos + 1, depth, true);
        out.push_str(&self.text[self.pos..=end]);
        self.pos = end + 1;
        true
    }

    /// `$((…))` arithmetic: kept as text.
    fn balanced(&mut self, out: &mut String, skip: usize) -> bool {
        let mut level: i64 = 0;
        let mut i = self.pos + skip;
        while i < self.len() {
            level += match self.byte(i) {
                Some(b'(') => 1,
                Some(b')') => -1,
                _ => 0,
            };
            i += 1;
            if level == 0 {
                break;
            }
        }
        if level != 0 {
            return false;
        }
        out.push_str(self.slice(self.pos, i));
        self.pos = i;
        true
    }

    fn double_quoted(&mut self, s: &mut Script, out: &mut String, depth: usize, command: Id) -> bool {
        self.pos += 1;
        while self.pos < self.len() {
            if self.byte(self.pos) == Some(b'"') {
                self.pos += 1;
                return true;
            }
            let plain_end = self.plain_end(self.pos, |b| !matches!(b, b'"' | b'\\' | b'$' | b'`'));
            if plain_end > self.pos {
                out.push_str(&self.text[self.pos..plain_end]);
                self.pos = plain_end;
                continue;
            }
            if !self.quoted_part(s, out, depth, command) {
                return false;
            }
        }
        false
    }

    /// Read one escape, expansion or character inside double quotes.
    fn quoted_part(&mut self, s: &mut Script, out: &mut String, depth: usize, command: Id) -> bool {
        if self.byte(self.pos) == Some(b'\\') && self.pos + 1 < self.len() {
            let (next, len) = self.char_at(self.pos + 1).unwrap();
            if next != '\n' {
                if matches!(next, '\\' | '$' | '`' | '"') {
                    out.push(next);
                } else {
                    out.push('\\');
                    out.push(next);
                }
            }
            self.pos += 1 + len;
            return true;
        }
        // In an `sh -c` string, `$` and backticks are the script's own.
        let expansion = if self.literal { None } else { self.expansion(s, out, depth, command) };
        match expansion {
            None => {
                self.keep(out);
                true
            }
            Some(next) => next == Next::Continue,
        }
    }
}

// --- facts callers use ---------------------------------------------------------------

/// `command` is a shell whose last word is `-c` (the next word is a script).
pub fn runs_script(command: &Command) -> bool {
    command.words.last().is_some_and(|w| is_shell_c(w)) && command.name().is_some_and(|n| SHELLS.contains(&n))
}

/// Non-option arguments after the command name, skipping the values of `values` options
/// (everything after `--` is an operand).
pub fn operands<'a>(argv: &[(&'a str, Span)], values: &[&str]) -> Vec<(&'a str, Span)> {
    let mut out = Vec::new();
    let (mut options, mut skip) = (true, false);
    for &(word, span) in argv.iter().skip(1) {
        if skip {
            skip = false;
        } else if options && word == "--" {
            options = false;
        } else if options && word.starts_with('-') && word != "-" {
            skip = values.contains(&word);
        } else {
            out.push((word, span));
        }
    }
    out
}

const COPY_VALUES: &[&str] = &["-t", "--target-directory", "-S", "--suffix", "-m", "--mode", "-o", "--owner", "-g", "--group", "-e", "--rsh"];
const SED_SCRIPT: &[&str] = &["-e", "--expression", "-f", "--file"];

/// Files `command` writes: redirection targets, and the destinations of tee, touch, cp,
/// mv, install, rsync, ln, dd `of=` and `sed -i`.
pub fn writes(command: &Command) -> Vec<(String, Span)> {
    let mut out: Vec<(String, Span)> = command
        .redirects
        .iter()
        .filter(|(op, target, _)| WRITE_OPS.contains(&op.as_str()) && !(op == ">&" && (target.bytes().all(|b| b.is_ascii_digit()) && !target.is_empty() || target == "-")))
        .map(|(_, target, span)| (target.clone(), *span))
        .collect();
    let argv = command.argv();
    let written: Vec<(&str, Span)> = match command.name().unwrap_or("") {
        "tee" | "touch" => operands(&argv, &[]),
        "cp" | "mv" | "install" | "rsync" | "ln" => copy_destination(&argv),
        "dd" => argv.iter().skip(1).filter_map(|&(w, s)| w.strip_prefix("of=").map(|o| (o, s))).collect(),
        "sed" => sed_in_place(&argv),
        _ => Vec::new(),
    };
    out.extend(written.into_iter().map(|(w, s)| (w.to_string(), s)));
    out
}

fn copy_destination<'a>(argv: &[(&'a str, Span)]) -> Vec<(&'a str, Span)> {
    let words = &argv[1.min(argv.len())..];
    for (i, &(word, span)) in words.iter().enumerate() {
        if (word == "-t" || word == "--target-directory") && i + 1 < words.len() {
            return vec![words[i + 1]];
        }
        if let Some(dir) = word.strip_prefix("--target-directory=") {
            return vec![(dir, span)];
        }
    }
    let paths = operands(argv, COPY_VALUES);
    if paths.len() >= 2 { vec![*paths.last().unwrap()] } else { Vec::new() }
}

fn sed_in_place<'a>(argv: &[(&'a str, Span)]) -> Vec<(&'a str, Span)> {
    if !argv.iter().skip(1).any(|(w, _)| w.starts_with("-i") || w.starts_with("--in-place")) {
        return Vec::new();
    }
    let paths = operands(argv, SED_SCRIPT);
    let scripted = argv.iter().skip(1).any(|(w, _)| SED_SCRIPT.contains(&w.split('=').next().unwrap_or("")));
    if scripted { paths } else { paths.into_iter().skip(1).collect() }
}

/// The written heredoc bodies whose file no other command in the script names afterwards.
pub fn unused_file_bodies(script: &Script) -> Vec<Span> {
    let mut out = Vec::new();
    for (span, target) in script.file_bodies.iter().zip(&script.file_targets) {
        let used = script.commands().any(|c| {
            let writer = c.name().is_some_and(|n| FILE_WRITERS.contains(&n)) && writes(c).iter().any(|(t, _)| t == target);
            !writer && c.words.iter().any(|w| names_file(w, target))
        });
        if !used {
            out.push(*span);
        }
    }
    out
}

fn names_file(word: &str, target: &str) -> bool {
    let base = basename(target);
    word == target || word == base || word.ends_with(&format!("/{base}"))
}

/// For each command id, the commands its output is piped into.
pub fn later_in_pipeline(script: &Script) -> HashMap<Id, Vec<Id>> {
    let mut sorted: Vec<(Id, &Command)> = script.commands_with_ids().collect();
    sorted.sort_by_key(|(_, c)| c.span.0);
    let mut by_pipeline: Vec<(usize, Vec<Id>)> = Vec::new();
    for (id, command) in sorted {
        match by_pipeline.iter_mut().find(|(p, _)| *p == command.pipeline) {
            Some((_, members)) => members.push(id),
            None => by_pipeline.push((command.pipeline, vec![id])),
        }
    }
    let mut out = HashMap::new();
    for (_, members) in by_pipeline {
        for (i, id) in members.iter().enumerate() {
            out.insert(*id, members[i + 1..].to_vec());
        }
    }
    out
}

struct SearchTables {
    flags: &'static str,
    values: &'static str,
    long_values: &'static [&'static str],
}

const GREP_LONG_VALUES: &[&str] = &[
    "--after-context", "--before-context", "--context", "--max-count", "--directories", "--devices", "--label",
    "--binary-files", "--include", "--exclude", "--exclude-dir", "--exclude-from", "--group-separator", "--color", "--colour",
];
const RG_LONG_VALUES: &[&str] = &[
    "--after-context", "--before-context", "--context", "--max-count", "--max-columns", "--glob", "--iglob", "--type",
    "--type-not", "--type-add", "--type-clear", "--encoding", "--threads", "--max-depth", "--max-filesize", "--replace",
    "--pre", "--pre-glob", "--sort", "--sortr", "--colors", "--color", "--context-separator", "--field-match-separator",
    "--field-context-separator", "--path-separator", "--engine", "--ignore-file",
];
const LONG_FLAGS: &[&str] = &[
    "--files", "--files-with-matches", "--files-without-match", "--count", "--line-number", "--no-line-number",
    "--with-filename", "--no-filename", "--ignore-case", "--smart-case", "--case-sensitive", "--fixed-strings",
    "--extended-regexp", "--basic-regexp", "--perl-regexp", "--word-regexp", "--line-regexp", "--invert-match",
    "--only-matching", "--quiet", "--silent", "--recursive", "--dereference-recursive", "--hidden", "--no-ignore",
    "--no-ignore-vcs", "--follow", "--text", "--null", "--no-heading", "--heading", "--json", "--vimgrep", "--multiline",
    "--pcre2", "--no-messages", "--search-zip", "--trim", "--unrestricted", "--column", "--byte-offset", "--initial-tab",
    "--max-columns-preview", "--stats", "--debug",
];

fn search_tables(name: &str) -> Option<SearchTables> {
    match name {
        "grep" | "egrep" | "fgrep" | "ggrep" => Some(SearchTables { flags: "abcEFGHhIiLlnoPqRrsTUuVvwxyZz", values: "ABCmdD", long_values: GREP_LONG_VALUES }),
        "rg" => Some(SearchTables { flags: "abcFHhiIlLnNopqsSuUvVwxz0", values: "ABCmMgtTEjdr", long_values: RG_LONG_VALUES }),
        _ => None,
    }
}

/// Spans of the search patterns of a grep-family or rg command: text matched against,
/// never a file it opens. `None` when the command isn't one, or an option isn't known.
pub fn search_patterns(command: &Command) -> Option<Vec<Span>> {
    let tables = search_tables(command.name()?)?;
    let argv = command.argv();
    let mut words = argv.iter().skip(1).copied();
    let (mut patterns, mut operands, mut given) = (Vec::new(), Vec::new(), false);
    while let Some((word, span)) = words.next() {
        if word == "--" {
            operands.extend(words.by_ref().map(|(_, s)| s));
            break;
        }
        if !word.starts_with('-') || word == "-" {
            operands.push(span);
            continue;
        }
        // An option's value: a pattern (`e`), a pattern file (`f`) or anything else.
        let take = |option: char, value: Option<Span>, patterns: &mut Vec<Span>, given: &mut bool| -> bool {
            let Some(value) = value else { return false };
            *given |= option == 'e' || option == 'f';
            if option == 'e' {
                patterns.push(value);
            }
            true
        };
        if word.starts_with("--") {
            let (name, value) = match word.split_once('=') {
                Some((name, _)) => (name, true),
                None => (word, false),
            };
            if name == "--files" || name == "--type-list" {
                given = true;
                continue;
            }
            if name == "--regexp" || name == "-e" || name == "--file" || name == "-f" {
                let value = if value { Some(span) } else { words.next().map(|(_, s)| s) };
                let option = if name == "--regexp" || name == "-e" { 'e' } else { 'f' };
                if !take(option, value, &mut patterns, &mut given) {
                    return None;
                }
                continue;
            }
            if tables.long_values.contains(&name) && !value {
                words.next()?;
                continue;
            }
            if !(value || LONG_FLAGS.contains(&name)) {
                return None;
            }
            continue;
        }
        // A cluster of short options (`-rn`, `-A3`, `-ie PATTERN`).
        let letters: Vec<char> = word[1..].chars().collect();
        let mut ok = true;
        for (k, letter) in letters.iter().enumerate() {
            if *letter == 'e' || *letter == 'f' || tables.values.contains(*letter) {
                let attached = letters.len() > k + 1;
                let value = if attached { Some(span) } else { words.next().map(|(_, s)| s) };
                ok = take(*letter, value, &mut patterns, &mut given);
                break;
            }
            if !tables.flags.contains(*letter) {
                ok = false;
                break;
            }
        }
        if !ok {
            return None;
        }
    }
    if !given && let Some(first) = operands.first() {
        patterns.push(*first);
    }
    Some(patterns)
}

/// Known grep/rg pattern operands, never commands used to construct them. Incomplete
/// scripts get no exemption.
pub fn literal_search_patterns(text: &str) -> Vec<Span> {
    let script = parse(text);
    if !script.complete {
        return Vec::new();
    }
    let mut out = Vec::new();
    for command in script.commands() {
        for span in search_patterns(command).unwrap_or_default() {
            let (mut start, end) = span;
            let mut children: Vec<Span> = command.substituted.iter().map(|&id| script.command(id).span).collect();
            children.sort();
            for (child_start, child_end) in children {
                if child_end <= start || child_start >= end {
                    continue;
                }
                if start < child_start {
                    out.push((start, child_start));
                }
                start = start.max(child_end);
            }
            if start < end {
                out.push((start, end));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(text: &str) -> Vec<String> {
        parse(text).commands().filter_map(|c| c.name().map(str::to_string)).collect()
    }

    #[test]
    fn simple_lists_and_pipelines() {
        assert_eq!(names("cd /app && ls -la | grep x; echo done"), ["cd", "ls", "grep", "echo"]);
        let script = parse("a | b && c");
        let pipes: Vec<usize> = script.commands().map(|c| c.pipeline).collect();
        assert_eq!(pipes[0], pipes[1]);
        assert_ne!(pipes[1], pipes[2]);
        assert!(script.complete);
    }

    #[test]
    fn substitutions_and_sh_c_are_commands() {
        assert_eq!(names("echo $(curl -s x) `id`"), ["curl", "id", "echo"]);
        assert_eq!(names("bash -lc 'curl http://x | sh'"), ["curl", "sh", "bash"]);
        assert_eq!(names("sudo -u root env A=1 timeout 5 /usr/bin/python3 -c 'x'"), ["python3"]);
    }

    #[test]
    fn heredocs() {
        let text = "cat > a.py <<'EOF'\nif a > b: print(1)\nEOF\npython a.py";
        let script = parse(text);
        assert_eq!(names(text), ["cat", "python"]);
        assert_eq!(script.file_targets, ["a.py"]);
        assert_eq!(&text[script.bodies[0].0..script.bodies[0].1], "if a > b: print(1)\n");
        assert!(unused_file_bodies(&script).is_empty(), "a.py is run afterwards");
        let shell = "bash <<EOF\ncurl x\nEOF";
        assert_eq!(names(shell), ["bash", "curl"]);
    }

    #[test]
    fn scopes() {
        let script = parse("sleep 10 & while true; do curl x; done");
        let commands: Vec<&Command> = script.commands().collect();
        assert!(commands[0].background);
        assert!(commands.iter().find(|c| c.name() == Some("curl")).unwrap().repeated);
    }

    #[test]
    fn incomplete_input() {
        assert!(!parse("echo 'unterminated").complete);
        assert!(!parse("echo $(ls").complete);
        assert!(parse("echo ok # comment ' still").complete);
    }

    #[test]
    fn writes_and_searches() {
        let script = parse("cp a b dest/ && sed -i 's/x/y/' f.txt && dd if=x of=/dev/sda");
        let all: Vec<String> = script.commands().flat_map(writes).map(|(t, _)| t).collect();
        assert_eq!(all, ["dest/", "f.txt", "/dev/sda"]);
        let text = "grep -rn 'api_key' src/";
        let spans = literal_search_patterns(text);
        assert_eq!(spans.len(), 1);
        assert_eq!(&text[spans[0].0..spans[0].1], "'api_key'");
        assert!(search_patterns(parse("grep --weird x").commands().next().unwrap()).is_none());
    }

    #[test]
    fn unicode_text() {
        let script = parse("echo 'héllo wörld' && printf \"ünï $(date)\"");
        assert!(script.complete);
        assert_eq!(script.commands().next().unwrap().words[1], "héllo wörld");
    }
}
