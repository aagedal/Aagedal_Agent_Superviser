//! The trace a pending call is judged in: steps, their calls' classified fields and the
//! tool results the agent received. Port of the parts of atif-scan's `data/model.py` the
//! ported checks read.

use serde_json::{Map, Value};

use crate::re::Span;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Channel {
    Message,
    Reasoning,
    Command,
    Path,
    Query,
    Url,
    Observation,
    /// Tool-argument strings no rule classified. Still scanned by text checks.
    Arguments,
    /// Argument data that is not an action target: file contents, edits, prompts, patterns.
    Payload,
}

/// Channels whose meaning depends on recognizing the tool that produced them.
pub const TOOL_INPUT_CHANNELS: &[Channel] = &[Channel::Command, Channel::Path, Channel::Query, Channel::Url];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Content {
    pub text: String,
    /// False when the value couldn't be read (unparseable arguments, a computed value).
    pub understood: bool,
}

impl Content {
    pub fn text(text: impl Into<String>) -> Self {
        Self { text: text.into(), understood: true }
    }

    pub fn unknown() -> Self {
        Self { text: String::new(), understood: false }
    }
}

pub type Field = (Channel, Content);

#[derive(Debug, Clone)]
pub struct Call {
    pub index: usize,
    /// Normalized category: shell, read, write, search_files, web_fetch, web_search, inert or other.
    pub tool: &'static str,
    pub fields: Vec<Field>,
    pub id: String,
    pub name: String,
    pub arguments: Option<Map<String, Value>>,
}

#[derive(Debug, Clone)]
pub struct Observation {
    pub source_call_id: Option<String>,
    pub content: Content,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    System,
    User,
    Agent,
}

#[derive(Debug, Clone)]
pub struct Step {
    pub index: usize,
    pub source: Source,
    pub message: Content,
    pub reasoning: Content,
    pub calls: Vec<Call>,
    pub observations: Vec<Observation>,
}

impl Step {
    /// The results recorded for `call` (linked by its id).
    pub fn results_for(&self, call: &Call) -> Vec<(usize, &Observation)> {
        self.observations
            .iter()
            .enumerate()
            .filter(|(_, o)| o.source_call_id.as_deref() == Some(call.id.as_str()))
            .collect()
    }
}

/// Where evidence is: a step, a channel, and the call/field or observation in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Locator {
    pub step: usize,
    pub channel: Channel,
    pub call: Option<usize>,
    pub field: Option<usize>,
    pub observation: Option<usize>,
    pub span: Option<Span>,
}

impl Locator {
    pub fn step(step: usize, channel: Channel) -> Self {
        Self { step, channel, call: None, field: None, observation: None, span: None }
    }

    pub fn with_span(mut self, span: Span) -> Self {
        self.span = Some(span);
        self
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Surface<'a> {
    pub at: Locator,
    pub content: &'a Content,
    /// The call's normalized category.
    pub tool: Option<&'static str>,
    pub tool_name: Option<&'a str>,
}

impl<'a> Surface<'a> {
    pub fn text(&self) -> &'a str {
        &self.content.text
    }
}

#[derive(Debug, Clone, Default)]
pub struct Trace {
    pub steps: Vec<Step>,
}

impl Trace {
    /// Message, reasoning and every call field of each agent step.
    pub fn agent_surfaces(&self) -> Vec<Surface<'_>> {
        let mut out = Vec::new();
        for step in self.steps.iter().filter(|s| s.source == Source::Agent) {
            out.push(Surface { at: Locator::step(step.index, Channel::Message), content: &step.message, tool: None, tool_name: None });
            out.push(Surface { at: Locator::step(step.index, Channel::Reasoning), content: &step.reasoning, tool: None, tool_name: None });
            for call in &step.calls {
                for (field_index, (channel, content)) in call.fields.iter().enumerate() {
                    let at = Locator { call: Some(call.index), field: Some(field_index), ..Locator::step(step.index, *channel) };
                    out.push(Surface { at, content, tool: Some(call.tool), tool_name: Some(&call.name) });
                }
            }
        }
        out
    }

    pub fn agent_calls(&self) -> impl Iterator<Item = (&Step, &Call)> {
        self.steps.iter().filter(|s| s.source == Source::Agent).flat_map(|s| s.calls.iter().map(move |c| (s, c)))
    }

    /// Every recorded tool result.
    pub fn observation_surfaces(&self) -> Vec<Surface<'_>> {
        let mut out = Vec::new();
        for step in &self.steps {
            for (index, observation) in step.observations.iter().enumerate() {
                let at = Locator { observation: Some(index), ..Locator::step(step.index, Channel::Observation) };
                out.push(Surface { at, content: &observation.content, tool: None, tool_name: None });
            }
        }
        out
    }

    pub fn agent_steps(&self) -> usize {
        self.steps.iter().filter(|s| s.source == Source::Agent).count()
    }
}
