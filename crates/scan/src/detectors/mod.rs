//! Detectors and the building blocks they share. Port of atif-scan's `detectors/text.py`
//! (`SurfaceDetector`, `RegexDetector`), keeping only what decides a match: safeagents
//! reports matches on the pending call, never coverage.

pub mod agents;
pub mod model_calls;
pub mod outbound;
pub mod side_channel;

use crate::model::{Channel, Locator, Surface, Trace};
use crate::re::{Re, Span};
use crate::{CheckSpec, Severity};

pub trait Detector: Send + Sync {
    fn spec(&self) -> &CheckSpec;
    /// Where the check matched in `trace`.
    fn evaluate(&self, trace: &Trace) -> Vec<Locator>;
}

/// What a predicate found in a surface: a span to cite, or the whole surface.
pub enum Hit {
    Span(Span),
    Whole,
}

impl From<Span> for Hit {
    fn from(span: Span) -> Self {
        Hit::Span(span)
    }
}

pub type Predicate = Box<dyn Fn(&Surface) -> Option<Hit> + Send + Sync>;

/// Where a hit on `surface` is.
pub fn matched(surface: &Surface, hit: Option<Hit>) -> Option<Locator> {
    match hit? {
        Hit::Span(span) => Some(surface.at.with_span(span)),
        Hit::Whole => Some(surface.at),
    }
}

/// A predicate over agent-authored surfaces on the given channels.
pub struct SurfaceDetector {
    pub spec: CheckSpec,
    pub channels: &'static [Channel],
    pub predicate: Predicate,
}

impl SurfaceDetector {
    pub fn new(spec: CheckSpec, channels: &'static [Channel], predicate: impl Fn(&Surface) -> Option<Hit> + Send + Sync + 'static) -> Self {
        Self { spec, channels, predicate: Box::new(predicate) }
    }

    /// A case-insensitive regex over the text of the given channels (atif-scan's `RegexDetector`).
    pub fn regex(spec: CheckSpec, pattern: &str, channels: &'static [Channel]) -> Self {
        let compiled = Re::new(&format!("(?i){pattern}"));
        Self::new(spec, channels, move |s| compiled.search(s.text()).map(Hit::Span))
    }
}

impl Detector for SurfaceDetector {
    fn spec(&self) -> &CheckSpec {
        &self.spec
    }

    fn evaluate(&self, trace: &Trace) -> Vec<Locator> {
        trace
            .agent_surfaces()
            .iter()
            .filter(|s| !(s.at.channel == Channel::Arguments && !s.content.understood))
            .filter(|s| self.channels.contains(&s.at.channel))
            .filter_map(|s| matched(s, (self.predicate)(s)))
            .collect()
    }
}

pub fn builtin() -> Vec<Box<dyn Detector>> {
    let mut detectors: Vec<Box<dyn Detector>> = vec![
        // safeagents' own addition: behaviour from the Cohere report atif-scan has no check for.
        Box::new(SurfaceDetector::regex(
            CheckSpec { id: "safeagents.namespace_escape", severity: Severity::Medium, title: "Creates or enters Linux namespaces" },
            r"\b(?:unshare|nsenter)\b",
            &[Channel::Command],
        )),
    ];
    detectors.extend(side_channel::detectors());
    detectors.extend(model_calls::detectors());
    detectors.extend(agents::detectors());
    detectors.extend(outbound::detectors());
    detectors
}
