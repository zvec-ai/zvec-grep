//! Replay of one translation unit's preprocessor timeline.
//!
//! A bare call spelled like a macro is not a call: `TRACE_POINT(1)` expands to
//! whatever the macro's body says, so binding it to a same-named function in
//! another file invents an edge. Which names stand defined at a given line is
//! a fact about the translation unit — the file's directives plus every header
//! it includes, in order — not about the file alone, so the answer is replayed
//! from the persisted directive timeline rather than read off one file.
//!
//! The replay answers one question per call site: is `name` *certainly*
//! defined at `line`? Certainty is three-valued because a condition may test a
//! flag the workspace never spells (`#if FREE_THREADED == 0`): both branches
//! are then live, and a `#define` inside them is only possibly defined. Only a
//! certainly-defined name suppresses a call, so the rule can only remove edges
//! the replay has evidence against.

use std::collections::{HashMap, HashSet};

use crate::domain::{PreprocessorDirective, PreprocessorKind};
use crate::storage::{IndexStore, ResolutionSnapshot};

use super::graph_resolver::import_target_file;

/// Deepest include chain the replay follows. Documents this deep are
/// pathological; stopping keeps the walk bounded.
const MAX_INCLUDE_DEPTH: usize = 64;

/// Directives one replay may execute across every included file. An unguarded
/// include graph is exponential in depth, so the walk carries a budget; a unit
/// that exhausts it answers nothing rather than answering from a prefix.
const MAX_DIRECTIVES: usize = 8192;

/// How certainly a name stands defined at a line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Support {
    Defined,
    Undefined,
    /// A conditional the workspace cannot settle may or may not have run.
    Maybe,
}

/// Three-valued truth of one condition, and of whether a branch is live.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Tri {
    True,
    False,
    Unknown,
}

fn and(left: Tri, right: Tri) -> Tri {
    match (left, right) {
        (Tri::False, _) | (_, Tri::False) => Tri::False,
        (Tri::True, Tri::True) => Tri::True,
        _ => Tri::Unknown,
    }
}

fn not(value: Tri) -> Tri {
    match value {
        Tri::True => Tri::False,
        Tri::False => Tri::True,
        Tri::Unknown => Tri::Unknown,
    }
}

/// One file's macro timeline: for each watched name, the support each event
/// left behind, in source order.
///
/// Events carry the line they take effect on *in the unit that included them*,
/// so a `#define` in a header takes effect at its `#include`, which is where
/// the preprocessor puts it too.
pub(crate) struct MacroTimeline {
    events: HashMap<String, Vec<(usize, Support)>>,
}

impl MacroTimeline {
    /// Replays the translation unit `file_id` belongs to.
    ///
    /// Only names a bare call in this file still spells are recorded: the
    /// timeline exists to answer those call sites, and a file with none pays
    /// nothing for the walk.
    pub(crate) fn build(
        store: &IndexStore,
        snapshot: &ResolutionSnapshot,
        language: &str,
        file_id: u32,
    ) -> crate::EngineResult<Self> {
        let watched: HashSet<String> = store.pending_call_names(file_id)?.into_iter().collect();
        if watched.is_empty() {
            return Ok(Self {
                events: HashMap::new(),
            });
        }
        let mut replay = Replay {
            store,
            snapshot,
            language,
            watched,
            events: HashMap::new(),
            macros: HashMap::new(),
            directives: HashMap::new(),
            include_stack: Vec::new(),
            once: HashSet::new(),
            budget: MAX_DIRECTIVES,
            truncated: false,
        };
        replay.execute(file_id, None, Tri::True)?;
        // A truncated replay knows only a prefix of the unit, so no name it
        // recorded is certainly defined at the lines it never reached.
        Ok(Self {
            events: if replay.truncated {
                HashMap::new()
            } else {
                replay.events
            },
        })
    }

    /// Whether the unit certainly has `name` defined at `line`.
    pub(crate) fn defined_at(&self, name: &str, line: usize) -> bool {
        let Some(events) = self.events.get(name) else {
            return false;
        };
        let position = events.partition_point(|(at, _)| *at <= line);
        position > 0 && events[position - 1].1 == Support::Defined
    }
}

/// A macro's state at the point the replay has reached.
struct Macro {
    support: Support,
    value: Option<String>,
}

/// One file being replayed, with the state every file of the unit shares.
struct Replay<'a> {
    store: &'a IndexStore,
    snapshot: &'a ResolutionSnapshot,
    language: &'a str,
    /// Bare call names this file still spells, the only ones recorded.
    watched: HashSet<String>,
    events: HashMap<String, Vec<(usize, Support)>>,
    macros: HashMap<String, Macro>,
    /// Directives read per file: a header included twice is read once.
    directives: HashMap<u32, Vec<PreprocessorDirective>>,
    /// Files currently open: an include back into this set would never finish.
    include_stack: Vec<u32>,
    /// Files a `#pragma once` has already been honoured for.
    once: HashSet<u32>,
    budget: usize,
    truncated: bool,
}

impl Replay<'_> {
    /// Replays one file's directives.
    ///
    /// `origin` is the line the file's whole content takes effect on in the
    /// unit — the `#include` that pulled it in — or `None` for the unit's own
    /// file, whose directives speak at their own lines. `inherited` is the
    /// enclosing region: a header included under an unsettled condition only
    /// possibly defines what its body defines.
    fn execute(
        &mut self,
        file_id: u32,
        origin: Option<usize>,
        inherited: Tri,
    ) -> crate::EngineResult<()> {
        if self.truncated || self.include_stack.len() >= MAX_INCLUDE_DEPTH {
            return Ok(());
        }
        let directives = self.directives(file_id)?;
        let path = self.snapshot.file_path(file_id).map(str::to_owned);
        self.include_stack.push(file_id);
        let mut frames: Vec<Frame> = Vec::new();
        for directive in &directives {
            if self.budget == 0 {
                // The unit is larger than the walk may follow. Stopping here
                // is honest only if the caller discards the partial answer.
                self.truncated = true;
                break;
            }
            self.budget -= 1;
            let line = origin.unwrap_or(directive.line);
            let region = frames
                .last()
                .map_or(inherited, |frame| and(inherited, frame.active));
            match directive.kind {
                PreprocessorKind::Include => {
                    self.include(directive, region, line, path.as_deref())?;
                }
                PreprocessorKind::Define => self.define(directive, region, line),
                PreprocessorKind::Undef => self.undef(directive, region, line),
                PreprocessorKind::PragmaOnce => {
                    if region != Tri::False {
                        self.once.insert(file_id);
                    }
                }
                PreprocessorKind::If => {
                    let value = self.expression_condition(directive);
                    frames.push(Frame::open(inherited, value));
                }
                PreprocessorKind::Ifdef => {
                    let value = self.guard_condition(directive, false);
                    frames.push(Frame::open(inherited, value));
                }
                PreprocessorKind::Ifndef => {
                    let value = self.guard_condition(directive, true);
                    frames.push(Frame::open(inherited, value));
                }
                PreprocessorKind::Elif => {
                    let value = self.expression_condition(directive);
                    if let Some(frame) = frames.last_mut() {
                        frame.branch(value);
                    }
                }
                PreprocessorKind::Elifdef => {
                    let negate = directive
                        .value
                        .as_deref()
                        .is_some_and(|opener| opener.contains("ndef"));
                    let value = self.guard_condition(directive, negate);
                    if let Some(frame) = frames.last_mut() {
                        frame.branch(value);
                    }
                }
                PreprocessorKind::Else => {
                    if let Some(frame) = frames.last_mut() {
                        frame.otherwise();
                    }
                }
                PreprocessorKind::Endif => {
                    frames.pop();
                }
            }
        }
        self.include_stack.pop();
        Ok(())
    }

    /// The directives of one file, read once per replay.
    fn directives(&mut self, file_id: u32) -> crate::EngineResult<Vec<PreprocessorDirective>> {
        if let Some(cached) = self.directives.get(&file_id) {
            return Ok(cached.clone());
        }
        let loaded = self.store.preprocessor_directives(file_id)?;
        self.directives.insert(file_id, loaded.clone());
        Ok(loaded)
    }

    /// Pulls one header into the unit.
    ///
    /// A file already open in this chain is skipped rather than entered again:
    /// the second entry would replay a cycle forever. A file whose
    /// `#pragma once` has been honoured is skipped too, which is the only way a
    /// second include can *not* re-run the file's defines.
    fn include(
        &mut self,
        directive: &PreprocessorDirective,
        region: Tri,
        line: usize,
        path: Option<&str>,
    ) -> crate::EngineResult<()> {
        if region == Tri::False {
            return Ok(());
        }
        let (Some(specifier), Some(path)) = (directive.value.as_deref(), path) else {
            // An angle-bracket include addresses roots the workspace cannot
            // prove, so there is nothing to pull in.
            return Ok(());
        };
        let Some(target) = import_target_file(self.snapshot, self.language, path, specifier) else {
            return Ok(());
        };
        if self.once.contains(&target) || self.include_stack.contains(&target) {
            return Ok(());
        }
        self.execute(target, Some(line), region)
    }

    fn define(&mut self, directive: &PreprocessorDirective, region: Tri, line: usize) {
        if region == Tri::False {
            return;
        }
        let Some(name) = directive.name.as_deref() else {
            return;
        };
        let value = directive.value.as_deref();
        let support = match region {
            Tri::True => Support::Defined,
            // Both branches ran, so only a redefinition that agrees with the
            // settled state keeps it settled.
            _ => match self.macros.get(name) {
                Some(current)
                    if current.support == Support::Defined && current.value.as_deref() == value =>
                {
                    Support::Defined
                }
                _ => Support::Maybe,
            },
        };
        self.record(
            name,
            Macro {
                support,
                value: value.map(str::to_owned),
            },
            line,
        );
    }

    fn undef(&mut self, directive: &PreprocessorDirective, region: Tri, line: usize) {
        if region == Tri::False {
            return;
        }
        let Some(name) = directive.name.as_deref() else {
            return;
        };
        let support = match region {
            Tri::True => Support::Undefined,
            _ => match self.macros.get(name).map(|current| current.support) {
                Some(Support::Undefined) => Support::Undefined,
                _ => Support::Maybe,
            },
        };
        self.record(
            name,
            Macro {
                support,
                value: None,
            },
            line,
        );
    }

    fn record(&mut self, name: &str, state: Macro, line: usize) {
        let support = state.support;
        if self.watched.contains(name) {
            self.events
                .entry(name.to_owned())
                .or_default()
                .push((line, support));
        }
        self.macros.insert(name.to_owned(), state);
    }

    /// `#ifdef`/`#ifndef`/`#elifdef`/`#elifndef`: the guard asks this unit
    /// alone, so a name no directive ever mentions is simply not defined.
    fn guard_condition(&self, directive: &PreprocessorDirective, negate: bool) -> Tri {
        let Some(name) = directive.name.as_deref() else {
            return Tri::Unknown;
        };
        let defined = match self.macros.get(name).map(|current| current.support) {
            Some(Support::Defined) => Tri::True,
            Some(Support::Maybe) => Tri::Unknown,
            Some(Support::Undefined) | None => Tri::False,
        };
        if negate { not(defined) } else { defined }
    }

    fn expression_condition(&self, directive: &PreprocessorDirective) -> Tri {
        match directive
            .value
            .as_deref()
            .map(|text| Expression::evaluate(text, &self.macros))
        {
            Some(Some(value)) => {
                if value == 0 {
                    Tri::False
                } else {
                    Tri::True
                }
            }
            // An expression the replay cannot settle leaves both branches live.
            _ => Tri::Unknown,
        }
    }
}

/// One open conditional: whether a later branch can still be live, and whether
/// the branch the replay is inside is.
struct Frame {
    parent: Tri,
    remaining: Tri,
    active: Tri,
}

impl Frame {
    fn open(parent: Tri, value: Tri) -> Self {
        Self {
            parent,
            remaining: not(value),
            active: and(parent, value),
        }
    }

    fn branch(&mut self, value: Tri) {
        self.active = and(self.parent, and(self.remaining, value));
        self.remaining = and(self.remaining, not(value));
    }

    fn otherwise(&mut self) {
        self.active = and(self.parent, self.remaining);
        self.remaining = Tri::False;
    }
}

/// One `#if` expression, evaluated to the integer the preprocessor compares.
///
/// `None` is "not knowable": an operand names a flag the workspace never
/// spells, or an operator the walk does not model. Every rule then answers
/// `None` rather than guessing a branch, which is what keeps an unsettled
/// conditional from suppressing a call.
struct Expression<'a> {
    tokens: Vec<Token>,
    position: usize,
    macros: &'a HashMap<String, Macro>,
}

#[derive(Clone)]
enum Token {
    Number(i64),
    Ident(String),
    Op(&'static str),
}

impl<'a> Expression<'a> {
    fn evaluate(text: &str, macros: &'a HashMap<String, Macro>) -> Option<i64> {
        let mut expression = Self {
            tokens: tokenize(text)?,
            position: 0,
            macros,
        };
        let value = expression.parse(0)?;
        // Trailing tokens mean the text was not understood as one expression.
        (expression.position == expression.tokens.len()).then_some(value)
    }

    /// Precedence climbing: `min` is the binding power the caller's operator
    /// claims, so an operator binds tighter only when its power is at least it.
    fn parse(&mut self, min: u8) -> Option<i64> {
        let mut left = Some(self.prefix()?);
        while let Some((operator, power)) = self.infix() {
            if power < min {
                break;
            }
            self.position += 1;
            if operator == "?" {
                let then_value = self.parse(0)?;
                if !self.eat(":") {
                    return None;
                }
                let else_value = self.parse(power)?;
                left = match left {
                    Some(condition) if condition != 0 => Some(then_value),
                    Some(_) => Some(else_value),
                    None if then_value == else_value => Some(then_value),
                    None => None,
                };
                continue;
            }
            let right = self.parse(power + 1);
            left = apply(operator, left, right);
        }
        left
    }

    fn prefix(&mut self) -> Option<i64> {
        match self.tokens.get(self.position)?.clone() {
            Token::Number(value) => {
                self.position += 1;
                Some(value)
            }
            Token::Ident(name) => {
                self.position += 1;
                if name == "defined" {
                    self.defined_operand()
                } else {
                    macro_value(&name, self.macros, 0)
                }
            }
            Token::Op("(") => {
                self.position += 1;
                let value = self.parse(0)?;
                if !self.eat(")") {
                    return None;
                }
                Some(value)
            }
            Token::Op("!") => {
                self.position += 1;
                self.parse(21).map(|value| i64::from(value == 0))
            }
            Token::Op("~") => {
                self.position += 1;
                self.parse(21).map(|value| !value)
            }
            Token::Op("-") => {
                self.position += 1;
                self.parse(21).map(i64::wrapping_neg)
            }
            Token::Op("+") => {
                self.position += 1;
                self.parse(21)
            }
            Token::Op(_) => None,
        }
    }

    /// `defined X` or `defined(X)`: the guard question, asked of this unit.
    fn defined_operand(&mut self) -> Option<i64> {
        let parenthesized = self.eat("(");
        let Token::Ident(name) = self.tokens.get(self.position)?.clone() else {
            return None;
        };
        self.position += 1;
        if parenthesized && !self.eat(")") {
            return None;
        }
        Some(
            match self.macros.get(&name).map(|current| current.support) {
                Some(Support::Defined) => 1,
                Some(Support::Maybe) => return None,
                Some(Support::Undefined) | None => 0,
            },
        )
    }

    fn infix(&self) -> Option<(&'static str, u8)> {
        let Token::Op(operator) = self.tokens.get(self.position)? else {
            return None;
        };
        Some((*operator, binding_power(operator)?))
    }

    fn eat(&mut self, operator: &str) -> bool {
        match self.tokens.get(self.position) {
            Some(Token::Op(spelled)) if *spelled == operator => {
                self.position += 1;
                true
            }
            _ => false,
        }
    }
}

/// The integer an operand stands for in a `#if` expression.
///
/// A name the unit never mentions is unknowable here: unlike a guard, which
/// asks only what this unit defines, an expression asks what value the flag
/// has, and the build may supply one (`-DFREE_THREADED=1`) that no source
/// spells.
fn macro_value(name: &str, macros: &HashMap<String, Macro>, depth: usize) -> Option<i64> {
    if depth > 8 {
        return None;
    }
    let current = macros.get(name)?;
    match current.support {
        Support::Undefined => Some(0),
        Support::Maybe => None,
        Support::Defined => {
            let value = current.value.as_deref()?.trim();
            if let Some(number) = integer(value) {
                return Some(number);
            }
            // `#define ON OTHER_FLAG` chains to the flag it names.
            let chained = value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
            if chained {
                macro_value(value, macros, depth + 1)
            } else {
                None
            }
        }
    }
}

fn integer(text: &str) -> Option<i64> {
    let trimmed = text.trim_end_matches(['u', 'U', 'l', 'L']);
    let (radix, digits) = match trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        Some(digits) => (16, digits),
        None => match trimmed
            .strip_prefix("0b")
            .or_else(|| trimmed.strip_prefix("0B"))
        {
            Some(digits) => (2, digits),
            None => (10, trimmed),
        },
    };
    i64::from_str_radix(digits, radix).ok()
}

fn tokenize(text: &str) -> Option<Vec<Token>> {
    // Longest first, so `&&` is not read as two `&`.
    const OPERATORS: [&str; 24] = [
        "&&", "||", "==", "!=", "<=", ">=", "<<", ">>", "<", ">", "!", "~", "-", "+", "*", "/",
        "%", "&", "|", "^", "(", ")", "?", ":",
    ];
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte.is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if byte.is_ascii_digit() {
            let start = index;
            while index < bytes.len() && bytes[index].is_ascii_alphanumeric() {
                index += 1;
            }
            tokens.push(Token::Number(integer(&text[start..index])?));
            continue;
        }
        if byte.is_ascii_alphabetic() || byte == b'_' {
            let start = index;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
            {
                index += 1;
            }
            tokens.push(Token::Ident(text[start..index].to_owned()));
            continue;
        }
        // A comment or a token the walk does not model makes the whole
        // expression unknowable instead of misread.
        let rest = &text[index..];
        let operator = OPERATORS
            .iter()
            .find(|operator| rest.starts_with(**operator))?;
        tokens.push(Token::Op(operator));
        index += operator.len();
    }
    Some(tokens)
}

fn binding_power(operator: &str) -> Option<u8> {
    Some(match operator {
        "?" => 1,
        "||" => 2,
        "&&" => 4,
        "|" => 6,
        "^" => 8,
        "&" => 10,
        "==" | "!=" => 12,
        "<" | "<=" | ">" | ">=" => 14,
        "<<" | ">>" => 16,
        "+" | "-" => 18,
        "*" | "/" | "%" => 20,
        _ => return None,
    })
}

/// Applies one binary operator, propagating "not knowable".
///
/// `&&` and `||` keep answerable operands: `0 && unknown` is certainly 0, so a
/// definite operand decides even when the other side does not.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a shift count that does not fit this cast is undefined in C anyway"
)]
fn apply(operator: &str, left: Option<i64>, right: Option<i64>) -> Option<i64> {
    match operator {
        "&&" => {
            return match (left, right) {
                (Some(0), _) | (_, Some(0)) => Some(0),
                (Some(left), Some(right)) => Some(i64::from(left != 0 && right != 0)),
                _ => None,
            };
        }
        "||" => {
            return match (left, right) {
                (Some(left), _) if left != 0 => Some(1),
                (_, Some(right)) if right != 0 => Some(1),
                (Some(0), Some(0)) => Some(0),
                _ => None,
            };
        }
        _ => {}
    }
    let (Some(left), Some(right)) = (left, right) else {
        return None;
    };
    Some(match operator {
        "+" => left.wrapping_add(right),
        "-" => left.wrapping_sub(right),
        "*" => left.wrapping_mul(right),
        "/" => left.checked_div(right)?,
        "%" => left.checked_rem(right)?,
        "<<" => left.wrapping_shl(right as u32),
        ">>" => left.wrapping_shr(right as u32),
        "<" => i64::from(left < right),
        "<=" => i64::from(left <= right),
        ">" => i64::from(left > right),
        ">=" => i64::from(left >= right),
        "==" => i64::from(left == right),
        "!=" => i64::from(left != right),
        "&" => left & right,
        "|" => left | right,
        "^" => left ^ right,
        _ => return None,
    })
}
