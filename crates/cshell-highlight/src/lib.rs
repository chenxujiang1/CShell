//! Pure, bounded matching. Never dispatches actions or mutates terminal cells.
use cshell_domain::{HighlightRule, HighlightStyle, OutputPattern};
use regex::{Regex, RegexBuilder, RegexSet, RegexSetBuilder};
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;
pub const MAX_RULES: usize = 64;
pub const MAX_PATTERN_BYTES: usize = 512;
pub const MAX_LINE_BYTES: usize = 16 * 1024;
pub const MAX_LINE_MATCHES: usize = 128;
pub fn compile_pattern(pattern: &OutputPattern) -> Result<Regex, &'static str> {
    let (text, sensitive, literal) = match pattern {
        OutputPattern::Literal {
            text,
            case_sensitive,
        } => (text, *case_sensitive, true),
        OutputPattern::Regex {
            text,
            case_sensitive,
        } => (text, *case_sensitive, false),
    };
    if text.is_empty() || text.len() > MAX_PATTERN_BYTES || text.chars().any(|c| c == '\0') {
        return Err("invalid output pattern length");
    }
    let source = if literal {
        regex::escape(text)
    } else {
        text.clone()
    };
    let regex = RegexBuilder::new(&source)
        .case_insensitive(!sensitive)
        .size_limit(256 * 1024)
        .dfa_size_limit(256 * 1024)
        .nest_limit(32)
        .build()
        .map_err(|_| "invalid or oversized output regex")?;
    if regex.is_match("") {
        return Err("output pattern must not match empty text");
    }
    Ok(regex)
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HighlightSpan {
    pub range: Range<u32>,
    pub style: HighlightStyle,
}
#[derive(Debug)]
pub struct CompiledHighlights {
    rules: Vec<(Regex, HighlightStyle)>,
    batch: RegexSet,
}
impl CompiledHighlights {
    pub fn new(rules: &[HighlightRule]) -> Result<Self, &'static str> {
        if rules.len() > MAX_RULES {
            return Err("highlight rule limit");
        }
        let mut compiled = Vec::new();
        for rule in rules.iter().filter(|r| r.enabled) {
            compiled.push((compile_pattern(&rule.pattern)?, rule.style));
        }
        let sources = compiled.iter().map(|(r, _)| r.as_str());
        let batch = RegexSetBuilder::new(sources)
            .case_insensitive(true)
            .size_limit(16 * 1024 * 1024)
            .dfa_size_limit(1024 * 1024)
            .build()
            .map_err(|_| "highlight batch limit")?;
        Ok(Self {
            rules: compiled,
            batch,
        })
    }
    pub fn spans(&self, text: &str) -> Vec<HighlightSpan> {
        if text.len() > MAX_LINE_BYTES {
            return Vec::new();
        }
        let boundaries = text
            .grapheme_indices(true)
            .map(|(i, _)| i)
            .chain(std::iter::once(text.len()))
            .collect::<Vec<_>>();
        let mut out = Vec::new();
        // Rule order defines overlap precedence; later spans win at paint time.
        for index in self.batch.matches(text).iter() {
            let (regex, style) = &self.rules[index];
            for found in regex.find_iter(text) {
                if out.len() >= MAX_LINE_MATCHES {
                    return out;
                }
                if found.start() == found.end() {
                    continue;
                }
                let start = boundaries
                    .partition_point(|b| *b <= found.start())
                    .saturating_sub(1);
                let end = boundaries
                    .partition_point(|b| *b < found.end())
                    .min(boundaries.len() - 1);
                out.push(HighlightSpan {
                    range: boundaries[start] as u32..boundaries[end] as u32,
                    style: *style,
                });
            }
        }
        out
    }
}
