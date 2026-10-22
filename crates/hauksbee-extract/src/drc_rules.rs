//! KiCad custom-rule scope: the condition language and layer scope of a
//! `.kicad_dru` clearance rule, judged over a pair of copper items.
//!
//! A custom clearance rule applies to a pair of items only when its
//! `(condition ...)` holds for them and its `(layer ...)` covers the layer
//! being checked. Every judgement here is three-valued ([`Tri`]): a condition
//! that names something the extractor does not know about a particular item
//! (for example the layer of a via, or a function this reader does not
//! implement) is `Unknown`, never guessed. The DRC sweep turns an `Unknown`
//! that could change a verdict into a withheld finding, counted and named in
//! the report, so a board is neither accused nor cleared on a rule that was
//! not actually evaluated.
//!
//! What is evaluated was established against `kicad-cli pcb drc` 10.0.x on
//! synthetic boards:
//!
//! - a condition is tried with the item pair in both orders (`A`/`B` swapped),
//! - `Type`, `NetName` and `NetClass` compare case-insensitively, with `*` and
//!   `?` wildcards in the literal; `Layer`, `Pad_Type`, `Pad_Number` and
//!   footprint patterns compare case-sensitively,
//! - an arc track has `Type == 'Track'` (not `'Arc'`),
//! - `memberOfFootprint` matches the footprint reference or its exact library
//!   identifier,
//! - a quoted `(layer "outer")` is not the `outer` keyword, and a rules file
//!   with a layer or condition KiCad cannot compile is ignored by KiCad as a
//!   whole.
//!
//! Anything outside that list is read but judged `Unknown`.

use crate::drc::ItemKind;

/// A three-valued truth value (Kleene logic).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tri {
    Yes,
    No,
    Unknown,
}

impl Tri {
    const fn from_bool(value: bool) -> Self {
        if value {
            Self::Yes
        } else {
            Self::No
        }
    }

    const fn negate(self) -> Self {
        match self {
            Self::Yes => Self::No,
            Self::No => Self::Yes,
            Self::Unknown => Self::Unknown,
        }
    }

    const fn both(self, other: Self) -> Self {
        match (self, other) {
            (Self::No, _) | (_, Self::No) => Self::No,
            (Self::Yes, Self::Yes) => Self::Yes,
            _ => Self::Unknown,
        }
    }

    const fn either(self, other: Self) -> Self {
        match (self, other) {
            (Self::Yes, _) | (_, Self::Yes) => Self::Yes,
            (Self::No, Self::No) => Self::No,
            _ => Self::Unknown,
        }
    }
}

/// A pad's electrical type as KiCad names it in `Pad_Type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PadType {
    Smd,
    ThroughHole,
    NonPlated,
    EdgeConnector,
}

impl PadType {
    /// Read the `(pad "n" <type> ...)` token.
    pub(crate) fn from_token(token: &str) -> Option<Self> {
        match token {
            "smd" => Some(Self::Smd),
            "thru_hole" => Some(Self::ThroughHole),
            "np_thru_hole" => Some(Self::NonPlated),
            "connect" => Some(Self::EdgeConnector),
            _ => None,
        }
    }

    /// The value `A.Pad_Type` compares against.
    const fn kicad_name(self) -> &'static str {
        match self {
            Self::Smd => "SMD",
            Self::ThroughHole => "Through-hole",
            Self::NonPlated => "NPTH, mechanical",
            Self::EdgeConnector => "Edge connector",
        }
    }
}

/// What a clearance rule's condition can ask about one copper item beyond its
/// kind, net and layer. Filled in by the KiCad reader; an item without it
/// (any non-KiCad input) answers `Unknown` to every such question.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ItemAttrs {
    /// The pad's own `(clearance)`, else its footprint's. KiCad lets an item's
    /// local clearance override the netclass and custom rules.
    pub local_clearance_mm: Option<f64>,
    pub pad_type: Option<PadType>,
    pub pad_number: Option<String>,
    /// Owning footprint reference and library identifier.
    pub footprint_reference: Option<String>,
    pub footprint_lib_id: Option<String>,
}

/// An empty attribute set, for items that carry none.
pub const NO_ATTRS: ItemAttrs = ItemAttrs {
    local_clearance_mm: None,
    pad_type: None,
    pad_number: None,
    footprint_reference: None,
    footprint_lib_id: None,
};

/// One copper item as a rule condition sees it.
#[derive(Debug, Clone, Copy)]
pub struct ItemFacts<'a> {
    pub kind: ItemKind,
    pub net_name: &'a str,
    pub net_class: &'a str,
    pub attrs: &'a ItemAttrs,
}

impl ItemFacts<'_> {
    /// True when the item lives on exactly the layer being swept, so that
    /// `Layer` is that layer: a track, an arc, or an SMD pad. Through-hole
    /// pads, vias and pours span layers and KiCad reports one of them.
    fn on_one_layer(&self) -> bool {
        match self.kind {
            ItemKind::Track | ItemKind::Arc => true,
            ItemKind::Pad => self.attrs.pad_type == Some(PadType::Smd),
            ItemKind::Via | ItemKind::Zone | ItemKind::Graphic => false,
        }
    }
}

// ── Wildcards ────────────────────────────────────────────────────────────────

/// Glob match with `*` (any run) and `?` (any one character).
fn wild_match(pattern: &str, text: &str, case_insensitive: bool) -> bool {
    let fold = |c: char| {
        if case_insensitive {
            c.to_ascii_lowercase()
        } else {
            c
        }
    };
    let pattern: Vec<char> = pattern.chars().map(fold).collect();
    let text: Vec<char> = text.chars().map(fold).collect();
    let (mut p, mut t) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pattern.len() && pattern[p] == '*' {
            star = Some((p + 1, t));
            p += 1;
        } else if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if let Some((sp, st)) = star {
            p = sp;
            t = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}

fn has_wildcard(pattern: &str) -> bool {
    pattern.contains(['*', '?'])
}

// ── Expression language ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    A,
    B,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Prop {
    Type,
    NetName,
    NetClass,
    Layer,
    PadType,
    PadNumber,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Func {
    MemberOfFootprint,
    HasNetclass,
    ExistsOnLayer,
}

#[derive(Debug, Clone, PartialEq)]
enum Operand {
    Literal(String),
    Prop(Side, Prop),
    Call(Side, Func, Vec<String>),
    /// A number, a property or a function this reader does not implement.
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Compare {
    Equal,
    NotEqual,
    Other,
}

#[derive(Debug, Clone, PartialEq)]
enum Expr {
    Or(Box<Self>, Box<Self>),
    And(Box<Self>, Box<Self>),
    Not(Box<Self>),
    Compare(Operand, Compare, Operand),
    Truth(Operand),
}

/// A value an operand evaluates to.
enum Value {
    /// Text, and whether KiCad compares it without regard to case.
    Text(String, bool),
    Truth(Tri),
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Ident(String),
    Text(String),
    Number,
    Open,
    Close,
    Comma,
    Equal,
    NotEqual,
    Other,
    And,
    Or,
    Not,
}

fn tokenize(source: &str) -> Option<Vec<Token>> {
    let chars: Vec<char> = source.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match c {
            c if c.is_whitespace() => i += 1,
            '(' => {
                tokens.push(Token::Open);
                i += 1;
            }
            ')' => {
                tokens.push(Token::Close);
                i += 1;
            }
            ',' => {
                tokens.push(Token::Comma);
                i += 1;
            }
            '=' if next == Some('=') => {
                tokens.push(Token::Equal);
                i += 2;
            }
            '!' if next == Some('=') => {
                tokens.push(Token::NotEqual);
                i += 2;
            }
            '!' => {
                tokens.push(Token::Not);
                i += 1;
            }
            '<' | '>' => {
                tokens.push(Token::Other);
                i += if next == Some('=') { 2 } else { 1 };
            }
            '&' if next == Some('&') => {
                tokens.push(Token::And);
                i += 2;
            }
            '|' if next == Some('|') => {
                tokens.push(Token::Or);
                i += 2;
            }
            '\'' | '"' => {
                let quote = c;
                let mut text = String::new();
                i += 1;
                loop {
                    let ch = *chars.get(i)?;
                    i += 1;
                    if ch == quote {
                        break;
                    }
                    text.push(ch);
                }
                tokens.push(Token::Text(text));
            }
            c if c.is_ascii_digit() || c == '.' => {
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '.') {
                    i += 1;
                }
                tokens.push(Token::Number);
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len()
                    && (chars[i].is_ascii_alphanumeric() || chars[i] == '_' || chars[i] == '.')
                {
                    i += 1;
                }
                tokens.push(Token::Ident(chars[start..i].iter().collect()));
            }
            _ => return None,
        }
    }
    Some(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    at: usize,
    /// Cleared when an operand this reader does not implement was read.
    supported: bool,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn take(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.at).cloned();
        self.at += 1;
        token
    }

    fn or(&mut self) -> Option<Expr> {
        let mut left = self.and()?;
        while self.peek() == Some(&Token::Or) {
            self.at += 1;
            left = Expr::Or(Box::new(left), Box::new(self.and()?));
        }
        Some(left)
    }

    fn and(&mut self) -> Option<Expr> {
        let mut left = self.unary()?;
        while self.peek() == Some(&Token::And) {
            self.at += 1;
            left = Expr::And(Box::new(left), Box::new(self.unary()?));
        }
        Some(left)
    }

    fn unary(&mut self) -> Option<Expr> {
        if self.peek() == Some(&Token::Not) {
            self.at += 1;
            return Some(Expr::Not(Box::new(self.unary()?)));
        }
        self.comparison()
    }

    fn comparison(&mut self) -> Option<Expr> {
        if self.peek() == Some(&Token::Open) {
            self.at += 1;
            let inner = self.or()?;
            return (self.take()? == Token::Close).then_some(inner);
        }
        let left = self.operand()?;
        let compare = match self.peek() {
            Some(Token::Equal) => Compare::Equal,
            Some(Token::NotEqual) => Compare::NotEqual,
            Some(Token::Other) => Compare::Other,
            _ => return Some(Expr::Truth(left)),
        };
        self.at += 1;
        if compare == Compare::Other {
            self.supported = false;
        }
        let right = self.operand()?;
        Some(Expr::Compare(left, compare, right))
    }

    fn operand(&mut self) -> Option<Operand> {
        match self.take()? {
            Token::Text(text) => Some(Operand::Literal(text)),
            Token::Number => {
                self.supported = false;
                Some(Operand::Unsupported)
            }
            Token::Ident(name) => {
                let (side, member) = if let Some(member) = name.strip_prefix("A.") {
                    (Side::A, member.to_string())
                } else if let Some(member) = name.strip_prefix("B.") {
                    (Side::B, member.to_string())
                } else {
                    // A bare function or constant (`isPlated()`, `true`).
                    self.supported = false;
                    if self.peek() == Some(&Token::Open) {
                        self.skip_arguments()?;
                    }
                    return Some(Operand::Unsupported);
                };
                if self.peek() == Some(&Token::Open) {
                    let arguments = self.arguments()?;
                    let func = match member.as_str() {
                        "memberOfFootprint" => Some(Func::MemberOfFootprint),
                        "hasNetclass" => Some(Func::HasNetclass),
                        "existsOnLayer" => Some(Func::ExistsOnLayer),
                        _ => None,
                    };
                    return Some(match func {
                        Some(func) if arguments.len() == 1 => Operand::Call(side, func, arguments),
                        _ => {
                            self.supported = false;
                            Operand::Unsupported
                        }
                    });
                }
                let prop = match member.as_str() {
                    "Type" => Some(Prop::Type),
                    "NetName" => Some(Prop::NetName),
                    "NetClass" => Some(Prop::NetClass),
                    "Layer" => Some(Prop::Layer),
                    "Pad_Type" => Some(Prop::PadType),
                    "Pad_Number" => Some(Prop::PadNumber),
                    _ => None,
                };
                Some(prop.map_or_else(
                    || {
                        self.supported = false;
                        Operand::Unsupported
                    },
                    |prop| Operand::Prop(side, prop),
                ))
            }
            _ => None,
        }
    }

    /// `( 'a', 'b' )` as the string arguments; a non-string argument is read
    /// and dropped (the call is then unsupported).
    fn arguments(&mut self) -> Option<Vec<String>> {
        if self.take()? != Token::Open {
            return None;
        }
        let mut arguments = Vec::new();
        if self.peek() == Some(&Token::Close) {
            self.at += 1;
            return Some(arguments);
        }
        loop {
            match self.take()? {
                Token::Text(text) => arguments.push(text),
                Token::Number | Token::Ident(_) => self.supported = false,
                _ => return None,
            }
            match self.take()? {
                Token::Comma => {}
                Token::Close => return Some(arguments),
                _ => return None,
            }
        }
    }

    fn skip_arguments(&mut self) -> Option<()> {
        self.arguments().map(|_| ())
    }
}

/// Parse a condition. `None` is a syntax error; `Some((expr, supported))`
/// says whether every operand in it is one this reader implements.
fn parse_expr(source: &str) -> Option<(Expr, bool)> {
    let mut parser = Parser {
        tokens: tokenize(source)?,
        at: 0,
        supported: true,
    };
    let expr = parser.or()?;
    (parser.at == parser.tokens.len()).then_some((expr, parser.supported))
}

impl Operand {
    fn value(&self, a: &ItemFacts, b: &ItemFacts, layer: &str) -> Value {
        let facts = |side: Side| if side == Side::A { a } else { b };
        match self {
            Self::Literal(text) => Value::Text(text.clone(), false),
            Self::Unsupported => Value::Unknown,
            Self::Prop(side, prop) => {
                let item = facts(*side);
                match prop {
                    Prop::Type => match item.kind {
                        ItemKind::Track | ItemKind::Arc => Value::Text("Track".into(), true),
                        ItemKind::Via => Value::Text("Via".into(), true),
                        ItemKind::Pad => Value::Text("Pad".into(), true),
                        ItemKind::Zone => Value::Text("Zone".into(), true),
                        ItemKind::Graphic => Value::Unknown,
                    },
                    Prop::NetName => Value::Text(item.net_name.to_string(), true),
                    Prop::NetClass => Value::Text(item.net_class.to_string(), true),
                    Prop::Layer if item.on_one_layer() => Value::Text(layer.to_string(), false),
                    Prop::PadType if item.kind == ItemKind::Pad => {
                        item.attrs.pad_type.map_or(Value::Unknown, |t| {
                            Value::Text(t.kicad_name().into(), false)
                        })
                    }
                    Prop::PadNumber if item.kind == ItemKind::Pad => item
                        .attrs
                        .pad_number
                        .as_ref()
                        .map_or(Value::Unknown, |n| Value::Text(n.clone(), false)),
                    _ => Value::Unknown,
                }
            }
            Self::Call(side, func, arguments) => {
                let item = facts(*side);
                let pattern = arguments.first().map_or("", String::as_str);
                Value::Truth(call(*func, item, pattern, layer))
            }
        }
    }
}

fn call(func: Func, item: &ItemFacts, pattern: &str, layer: &str) -> Tri {
    match func {
        Func::HasNetclass => Tri::from_bool(wild_match(pattern, item.net_class, true)),
        Func::ExistsOnLayer => {
            if !item.on_one_layer() {
                Tri::Unknown
            } else if pattern == layer {
                Tri::Yes
            } else if is_copper_layer_name(pattern) {
                Tri::No
            } else {
                Tri::Unknown
            }
        }
        Func::MemberOfFootprint => {
            if item.kind != ItemKind::Pad {
                return Tri::Unknown;
            }
            let (Some(reference), Some(lib_id)) = (
                item.attrs.footprint_reference.as_deref(),
                item.attrs.footprint_lib_id.as_deref(),
            ) else {
                return Tri::Unknown;
            };
            if wild_match(pattern, reference, false) || pattern == lib_id {
                Tri::Yes
            } else if wild_match(pattern, reference, true)
                || (has_wildcard(pattern) && wild_match(pattern, lib_id, false))
            {
                // Case-only differences and glob-versus-library-id matching
                // were not settled against KiCad.
                Tri::Unknown
            } else {
                Tri::No
            }
        }
    }
}

fn is_copper_layer_name(name: &str) -> bool {
    name == "F.Cu"
        || name == "B.Cu"
        || name
            .strip_prefix("In")
            .and_then(|rest| rest.strip_suffix(".Cu"))
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

fn compare(left: &Value, op: Compare, right: &Value) -> Tri {
    if op == Compare::Other {
        return Tri::Unknown;
    }
    let equal = match (left, right) {
        (Value::Text(l, l_ci), Value::Text(r, r_ci)) => {
            // The literal is the pattern; between two properties it is plain
            // equality. A literal is never case-insensitive on its own, so
            // the property side decides.
            let (pattern, text, ci) = match (*l_ci, *r_ci) {
                (false, true) => (l, r, true),
                (true, false) => (r, l, true),
                (ci, _) => (r, l, ci),
            };
            if wild_match(pattern, text, ci) {
                Tri::Yes
            } else if !ci && wild_match(pattern, text, true) {
                Tri::Unknown
            } else {
                Tri::No
            }
        }
        (Value::Truth(l), Value::Truth(r)) => match (l, r) {
            (Tri::Unknown, _) | (_, Tri::Unknown) => Tri::Unknown,
            _ => Tri::from_bool(l == r),
        },
        _ => Tri::Unknown,
    };
    if op == Compare::NotEqual {
        equal.negate()
    } else {
        equal
    }
}

impl Expr {
    fn eval(&self, a: &ItemFacts, b: &ItemFacts, layer: &str) -> Tri {
        match self {
            Self::Or(l, r) => l.eval(a, b, layer).either(r.eval(a, b, layer)),
            Self::And(l, r) => l.eval(a, b, layer).both(r.eval(a, b, layer)),
            Self::Not(inner) => inner.eval(a, b, layer).negate(),
            Self::Compare(l, op, r) => compare(&l.value(a, b, layer), *op, &r.value(a, b, layer)),
            Self::Truth(operand) => match operand.value(a, b, layer) {
                Value::Truth(t) => t,
                Value::Text(..) | Value::Unknown => Tri::Unknown,
            },
        }
    }
}

// ── Rule scope ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum Condition {
    Always,
    Expr(Expr),
    /// A syntax error, or an operand this reader does not implement: the rule
    /// may or may not apply, and nothing here can say which.
    Opaque,
}

#[derive(Debug, Clone, PartialEq)]
enum LayerScope {
    Any,
    Outer,
    Inner,
    Named(String),
    Opaque,
}

/// One `.kicad_dru` clearance rule, reduced to what the sweep needs.
#[derive(Debug, Clone, PartialEq)]
pub struct CustomClearanceRule {
    pub name: String,
    /// The rule's `(constraint clearance (min ...))` in mm.
    pub min_mm: f64,
    /// `(severity ignore)`: when this rule governs a pair, the pair is not
    /// checked at all.
    pub ignore: bool,
    condition: Condition,
    layer: LayerScope,
}

impl CustomClearanceRule {
    /// Compile a rule's scope from its raw `.kicad_dru` fields.
    #[must_use]
    pub fn compile(
        name: &str,
        condition: Option<&str>,
        layer: Option<&str>,
        layer_is_quoted: bool,
        severity: Option<&str>,
        min_mm: f64,
    ) -> Self {
        let condition = match condition.map(str::trim) {
            None | Some("") => Condition::Always,
            Some(source) => match parse_expr(source) {
                Some((expr, true)) => Condition::Expr(expr),
                _ => Condition::Opaque,
            },
        };
        let layer = match layer {
            None => LayerScope::Any,
            Some("outer") if !layer_is_quoted => LayerScope::Outer,
            Some("inner") if !layer_is_quoted => LayerScope::Inner,
            Some(name) if is_copper_layer_name(name) => LayerScope::Named(name.to_string()),
            Some(_) => LayerScope::Opaque,
        };
        Self {
            name: name.to_string(),
            min_mm,
            ignore: severity == Some("ignore"),
            condition,
            layer,
        }
    }

    /// False when the condition or layer could not be turned into something
    /// this reader can judge, so the rule is only ever `Unknown`.
    #[must_use]
    pub fn is_evaluable(&self) -> bool {
        self.condition != Condition::Opaque && self.layer != LayerScope::Opaque
    }

    /// Whether the rule governs the pair on `layer`.
    pub(crate) fn applies(&self, a: &ItemFacts, b: &ItemFacts, layer: &str) -> Tri {
        let layer_scope = match &self.layer {
            LayerScope::Any => Tri::Yes,
            LayerScope::Outer => Tri::from_bool(layer == "F.Cu" || layer == "B.Cu"),
            LayerScope::Inner => {
                Tri::from_bool(is_copper_layer_name(layer) && layer != "F.Cu" && layer != "B.Cu")
            }
            LayerScope::Named(name) => Tri::from_bool(name == layer),
            LayerScope::Opaque => Tri::Unknown,
        };
        if layer_scope == Tri::No {
            return Tri::No;
        }
        let condition = match &self.condition {
            Condition::Always => Tri::Yes,
            Condition::Expr(expr) => expr.eval(a, b, layer).either(expr.eval(b, a, layer)),
            Condition::Opaque => Tri::Unknown,
        };
        layer_scope.both(condition)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts<'a>(kind: ItemKind, net: &'a str, attrs: &'a ItemAttrs) -> ItemFacts<'a> {
        ItemFacts {
            kind,
            net_name: net,
            net_class: "Default",
            attrs,
        }
    }

    fn rule(condition: &str) -> CustomClearanceRule {
        CustomClearanceRule::compile("r", Some(condition), None, false, None, 0.1)
    }

    fn applies(condition: &str, a: ItemKind, b: ItemKind) -> Tri {
        let attrs = ItemAttrs::default();
        rule(condition).applies(&facts(a, "A", &attrs), &facts(b, "B", &attrs), "F.Cu")
    }

    #[test]
    fn types_compare_in_both_orders_and_ignore_case() {
        assert_eq!(
            applies("A.Type == 'pad'", ItemKind::Track, ItemKind::Pad),
            Tri::Yes
        );
        assert_eq!(
            applies("A.Type == 'Via'", ItemKind::Track, ItemKind::Pad),
            Tri::No
        );
        assert_eq!(
            applies("A.Type == 'Pa*'", ItemKind::Pad, ItemKind::Pad),
            Tri::Yes
        );
        assert_eq!(
            applies("A.Type == 'Track'", ItemKind::Arc, ItemKind::Pad),
            Tri::Yes,
            "an arc track is a Track"
        );
        assert_eq!(
            applies(
                "A.Type == 'Pad' && B.Type == 'Pad'",
                ItemKind::Track,
                ItemKind::Pad
            ),
            Tri::No
        );
        assert_eq!(
            applies("!(A.Type == 'Track')", ItemKind::Pad, ItemKind::Pad),
            Tri::Yes
        );
    }

    #[test]
    fn unknowns_stay_unknown_unless_the_other_operand_decides() {
        // A via has no single layer: A.Layer is unknown, so the rule is too.
        assert_eq!(
            applies("A.Layer == 'F.Cu'", ItemKind::Via, ItemKind::Via),
            Tri::Unknown
        );
        // ...but a false conjunct settles the whole condition.
        assert_eq!(
            applies(
                "A.Layer == 'F.Cu' && A.Type == 'Track'",
                ItemKind::Via,
                ItemKind::Via
            ),
            Tri::No
        );
        // A track's layer is the swept layer; layers compare case-sensitively.
        assert_eq!(
            applies("A.Layer == 'F.Cu'", ItemKind::Track, ItemKind::Track),
            Tri::Yes
        );
        assert_eq!(
            applies("A.Layer == 'f.cu'", ItemKind::Track, ItemKind::Track),
            Tri::Unknown
        );
    }

    #[test]
    fn unimplemented_operands_make_the_rule_opaque() {
        for condition in [
            "A.isPlated()",
            "A.NoSuchProp == 'x'",
            "A.Pad_Number == 1",
            "A.Type == ",
            "A.inDiffPair('*')",
            "A.Width > 0.2mm",
        ] {
            assert!(!rule(condition).is_evaluable(), "{condition}");
        }
        assert!(rule("A.NetClass == 'Fast' || B.hasNetclass('Fast')").is_evaluable());
    }

    #[test]
    fn layer_scopes_follow_kicad_keywords() {
        let attrs = ItemAttrs::default();
        let (a, b) = (
            facts(ItemKind::Track, "A", &attrs),
            facts(ItemKind::Track, "B", &attrs),
        );
        let scoped = |layer: &str, quoted: bool| {
            CustomClearanceRule::compile("r", None, Some(layer), quoted, None, 0.1)
        };
        assert_eq!(scoped("outer", false).applies(&a, &b, "B.Cu"), Tri::Yes);
        assert_eq!(scoped("outer", false).applies(&a, &b, "In1.Cu"), Tri::No);
        assert_eq!(scoped("inner", false).applies(&a, &b, "In1.Cu"), Tri::Yes);
        assert_eq!(scoped("F.Cu", true).applies(&a, &b, "B.Cu"), Tri::No);
        // KiCad rejects a quoted "outer" (and discards its whole rules file).
        assert!(!scoped("outer", true).is_evaluable());
        assert!(!scoped("Nope.Cu", true).is_evaluable());
    }

    #[test]
    fn footprint_membership_matches_reference_or_exact_library_id() {
        let attrs = ItemAttrs {
            footprint_reference: Some("U1".into()),
            footprint_lib_id: Some("lib:fp".into()),
            ..ItemAttrs::default()
        };
        let pad = facts(ItemKind::Pad, "A", &attrs);
        let test = |pattern: &str| {
            rule(&format!("A.memberOfFootprint('{pattern}')")).applies(&pad, &pad, "F.Cu")
        };
        assert_eq!(test("U1"), Tri::Yes);
        assert_eq!(test("U*"), Tri::Yes);
        assert_eq!(test("lib:fp"), Tri::Yes);
        assert_eq!(test("U3"), Tri::No);
        assert_eq!(test("fp"), Tri::No);
        assert_eq!(test("*fp"), Tri::Unknown);
    }

    #[test]
    fn wildcards_match_whole_text() {
        assert!(wild_match("A*", "ABC", false));
        assert!(wild_match("*C", "ABC", false));
        assert!(wild_match("A?C", "ABC", false));
        assert!(!wild_match("A", "ABC", false));
        assert!(wild_match("a*", "ABC", true));
        assert!(!wild_match("a*", "ABC", false));
        assert!(wild_match("*", "", false));
    }
}
