//! XPath 1.0 over the arena DOM, for the DOM Standard's XPath interfaces.
//!
//! Sources (local standards library): XML Path Language (XPath) Version 1.0,
//! W3C Recommendation 16 November 1999 (`documents/xpath-1.0-*`), whose
//! grammar productions [1]–[39] this parser follows; DOM Level 3 XPath
//! (W3C Note 2004, `documents/DOM-Level-3-XPath-*`) §1.2 "Mapping DOM to
//! XPath"; WHATWG DOM #xpath; and WHATWG HTML "Interactions with XPath and
//! XSLT" (#xpath-1.0-processors), which makes the HTML namespace the default
//! element namespace of a name test without a prefix when the context node
//! belongs to an HTML document.
//!
//! The data model maps the DOM tree as DOM Level 3 XPath §1.2 describes:
//! adjacent Text and CDATASection siblings form one XPath text node that the
//! first non-empty DOM node represents; attributes are nodes whose parent is
//! their element (namespace declarations are not attribute nodes); each
//! element has namespace nodes for its in-scope namespaces; document types
//! are not part of the tree. A tree's root node is the root of the context
//! node's DOM tree: a Document, or the root of a detached subtree.

use std::cell::OnceCell;
use std::fmt::Write as _;

use rustc_hash::FxHashMap;

use super::{Dom, NodeData, NodeId};

const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
const XMLNS_NS: &str = "http://www.w3.org/2000/xmlns/";
const HTML_NS: &str = "http://www.w3.org/1999/xhtml";

/// A failure the DOM interfaces report: an expression that is not legal
/// (`SyntaxError`, DOM L3 INVALID_EXPRESSION_ERR), a prefix the resolver
/// does not know (`NamespaceError`), or an object of the wrong type
/// (`TypeError`, TYPE_ERR).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum XPathError {
    Syntax(String),
    Namespace(String),
    Type(String),
}

/// An XPath node: a DOM node, an attribute of an element (by index in its
/// attribute list) or a namespace node of an element (by prefix).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum XNode {
    Node(NodeId),
    Attribute(NodeId, usize),
    Namespace(NodeId, String),
}

/// A conversion to one of the basic types other than node-set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum XPathConversion {
    Number,
    String,
    Boolean,
}

/// The four basic types of XPath 1.0 §1.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum XValue {
    NodeSet(Vec<XNode>),
    Boolean(bool),
    Number(f64),
    String(String),
}

// ---------------------------------------------------------------------------
// §3.7 Lexical structure.

#[derive(Clone, Debug, PartialEq)]
enum Token {
    LParen,
    RParen,
    LBracket,
    RBracket,
    Dot,
    DotDot,
    At,
    Comma,
    ColonColon,
    /// NameTest: `*`, `prefix:*` or a QName (prefix, local; `None` local = `*`).
    NameTest(Option<String>, Option<String>),
    NodeType(String),
    OperatorName(String),
    Multiply,
    Slash,
    SlashSlash,
    Pipe,
    Plus,
    Minus,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    FunctionName(Option<String>, String),
    AxisName(String),
    Literal(String),
    Number(f64),
    Variable(Option<String>, String),
}

/// XML 1.0 (Fifth Edition) [4] NameStartChar, without ':' (XML Names NCName).
fn is_name_start(c: char) -> bool {
    matches!(c, 'A'..='Z' | '_' | 'a'..='z'
        | '\u{C0}'..='\u{D6}' | '\u{D8}'..='\u{F6}' | '\u{F8}'..='\u{2FF}'
        | '\u{370}'..='\u{37D}' | '\u{37F}'..='\u{1FFF}' | '\u{200C}'..='\u{200D}'
        | '\u{2070}'..='\u{218F}' | '\u{2C00}'..='\u{2FEF}' | '\u{3001}'..='\u{D7FF}'
        | '\u{F900}'..='\u{FDCF}' | '\u{FDF0}'..='\u{FFFD}' | '\u{10000}'..='\u{EFFFF}')
}

/// XML 1.0 [4a] NameChar, without ':'.
fn is_name_char(c: char) -> bool {
    is_name_start(c)
        || matches!(c, '-' | '.' | '0'..='9' | '\u{B7}' | '\u{300}'..='\u{36F}' | '\u{203F}'..='\u{2040}')
}

/// XML 1.0 [3] S.
fn is_xml_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\n')
}

fn tokenize(source: &str) -> Result<Vec<Token>, XPathError> {
    let chars: Vec<char> = source.chars().collect();
    let mut tokens: Vec<Token> = Vec::new();
    let mut i = 0usize;
    let syntax = |message: &str| XPathError::Syntax(message.to_string());
    let skip_space = |mut j: usize| {
        while j < chars.len() && is_xml_space(chars[j]) {
            j += 1;
        }
        j
    };
    let ncname = |start: usize| -> Option<usize> {
        if start < chars.len() && is_name_start(chars[start]) {
            let mut end = start + 1;
            while end < chars.len() && is_name_char(chars[end]) {
                end += 1;
            }
            Some(end)
        } else {
            None
        }
    };
    loop {
        i = skip_space(i);
        if i >= chars.len() {
            break;
        }
        // Rule 1: after a token that is not @, ::, (, [, , or an Operator,
        // `*` is the multiply operator and an NCName is an operator name.
        let operator_context = matches!(
            tokens.last(),
            Some(token) if !matches!(
                token,
                Token::At
                    | Token::ColonColon
                    | Token::LParen
                    | Token::LBracket
                    | Token::Comma
                    | Token::OperatorName(_)
                    | Token::Multiply
                    | Token::Slash
                    | Token::SlashSlash
                    | Token::Pipe
                    | Token::Plus
                    | Token::Minus
                    | Token::Equal
                    | Token::NotEqual
                    | Token::Less
                    | Token::LessEqual
                    | Token::Greater
                    | Token::GreaterEqual
            )
        );
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        let token = match c {
            '(' => {
                i += 1;
                Token::LParen
            }
            ')' => {
                i += 1;
                Token::RParen
            }
            '[' => {
                i += 1;
                Token::LBracket
            }
            ']' => {
                i += 1;
                Token::RBracket
            }
            '@' => {
                i += 1;
                Token::At
            }
            ',' => {
                i += 1;
                Token::Comma
            }
            '|' => {
                i += 1;
                Token::Pipe
            }
            '+' => {
                i += 1;
                Token::Plus
            }
            '-' => {
                i += 1;
                Token::Minus
            }
            '=' => {
                i += 1;
                Token::Equal
            }
            '!' if next == Some('=') => {
                i += 2;
                Token::NotEqual
            }
            '<' if next == Some('=') => {
                i += 2;
                Token::LessEqual
            }
            '<' => {
                i += 1;
                Token::Less
            }
            '>' if next == Some('=') => {
                i += 2;
                Token::GreaterEqual
            }
            '>' => {
                i += 1;
                Token::Greater
            }
            '/' if next == Some('/') => {
                i += 2;
                Token::SlashSlash
            }
            '/' => {
                i += 1;
                Token::Slash
            }
            ':' if next == Some(':') => {
                i += 2;
                Token::ColonColon
            }
            '.' if next == Some('.') => {
                i += 2;
                Token::DotDot
            }
            '.' if next.is_some_and(|d| d.is_ascii_digit()) => {
                // [30] Number ::= '.' Digits
                let start = i;
                i += 1;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
                let text: String = chars[start..i].iter().collect();
                Token::Number(text.parse().map_err(|_| syntax("invalid number"))?)
            }
            '.' => {
                i += 1;
                Token::Dot
            }
            '"' | '\'' => {
                // [29] Literal: no escapes.
                let end = chars[i + 1..]
                    .iter()
                    .position(|&q| q == c)
                    .ok_or_else(|| syntax("unterminated literal"))?;
                let text: String = chars[i + 1..i + 1 + end].iter().collect();
                i += end + 2;
                Token::Literal(text)
            }
            '0'..='9' => {
                // [30] Number ::= Digits ('.' Digits?)?
                let start = i;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
                if i < chars.len() && chars[i] == '.' && chars.get(i + 1) != Some(&'.') {
                    i += 1;
                    while i < chars.len() && chars[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                let text: String = chars[start..i].iter().collect();
                Token::Number(text.parse().map_err(|_| syntax("invalid number"))?)
            }
            '$' => {
                // [36] VariableReference ::= '$' QName
                let end = ncname(i + 1).ok_or_else(|| syntax("invalid variable reference"))?;
                let first: String = chars[i + 1..end].iter().collect();
                if chars.get(end) == Some(&':')
                    && let Some(local_end) = ncname(end + 1)
                {
                    let local: String = chars[end + 1..local_end].iter().collect();
                    i = local_end;
                    Token::Variable(Some(first), local)
                } else {
                    i = end;
                    Token::Variable(None, first)
                }
            }
            '*' if operator_context => {
                i += 1;
                Token::Multiply
            }
            '*' => {
                i += 1;
                Token::NameTest(None, None)
            }
            _ => {
                let Some(end) = ncname(i) else {
                    return Err(syntax("unexpected character"));
                };
                let first: String = chars[i..end].iter().collect();
                if operator_context {
                    if !matches!(first.as_str(), "and" | "or" | "mod" | "div") {
                        return Err(syntax("expected an operator"));
                    }
                    i = end;
                    Token::OperatorName(first)
                } else {
                    // A QName or NCName ':' '*' (no whitespace inside).
                    let (prefix, local, after) =
                        if chars.get(end) == Some(&':') && chars.get(end + 1) != Some(&':') {
                            if chars.get(end + 1) == Some(&'*') {
                                (Some(first.clone()), None, end + 2)
                            } else if let Some(local_end) = ncname(end + 1) {
                                let local: String = chars[end + 1..local_end].iter().collect();
                                (Some(first.clone()), Some(local), local_end)
                            } else {
                                return Err(syntax("invalid qualified name"));
                            }
                        } else {
                            (None, Some(first.clone()), end)
                        };
                    let lookahead = skip_space(after);
                    i = after;
                    match (chars.get(lookahead), chars.get(lookahead + 1), &local) {
                        // Rule 2: followed by '(' → NodeType or FunctionName.
                        (Some('('), _, Some(local)) => {
                            if prefix.is_none()
                                && matches!(
                                    local.as_str(),
                                    "comment" | "text" | "processing-instruction" | "node"
                                )
                            {
                                Token::NodeType(local.clone())
                            } else {
                                Token::FunctionName(prefix, local.clone())
                            }
                        }
                        // Rule 3: followed by '::' → AxisName.
                        (Some(':'), Some(':'), Some(local)) if prefix.is_none() => {
                            Token::AxisName(local.clone())
                        }
                        _ => Token::NameTest(prefix, local),
                    }
                }
            }
        };
        tokens.push(token);
    }
    Ok(tokens)
}

// ---------------------------------------------------------------------------
// Grammar productions [1]–[27].

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Axis {
    Ancestor,
    AncestorOrSelf,
    Attribute,
    Child,
    Descendant,
    DescendantOrSelf,
    Following,
    FollowingSibling,
    Namespace,
    Parent,
    Preceding,
    PrecedingSibling,
    SelfNode,
}

impl Axis {
    fn named(name: &str) -> Option<Self> {
        Some(match name {
            "ancestor" => Self::Ancestor,
            "ancestor-or-self" => Self::AncestorOrSelf,
            "attribute" => Self::Attribute,
            "child" => Self::Child,
            "descendant" => Self::Descendant,
            "descendant-or-self" => Self::DescendantOrSelf,
            "following" => Self::Following,
            "following-sibling" => Self::FollowingSibling,
            "namespace" => Self::Namespace,
            "parent" => Self::Parent,
            "preceding" => Self::Preceding,
            "preceding-sibling" => Self::PrecedingSibling,
            "self" => Self::SelfNode,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
enum NodeTest {
    /// A NameTest: the prefix as written (for HTML's default element
    /// namespace), its namespace once resolved, and the local name (`None`
    /// for `*` or `prefix:*`).
    Name {
        prefix: Option<String>,
        namespace: Option<String>,
        local: Option<String>,
    },
    Node,
    Text,
    Comment,
    ProcessingInstruction(Option<String>),
}

#[derive(Clone, Debug, PartialEq)]
struct Step {
    axis: Axis,
    test: NodeTest,
    predicates: Vec<Expr>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BinaryOp {
    Or,
    And,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    Plus,
    Minus,
    Multiply,
    Div,
    Mod,
    Union,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Function {
    Last,
    Position,
    Count,
    Id,
    LocalName,
    NamespaceUri,
    Name,
    String,
    Concat,
    StartsWith,
    Contains,
    SubstringBefore,
    SubstringAfter,
    Substring,
    StringLength,
    NormalizeSpace,
    Translate,
    Boolean,
    Not,
    True,
    False,
    Lang,
    Number,
    Sum,
    Floor,
    Ceiling,
    Round,
}

impl Function {
    /// §4 Core Function Library: the function and its argument count range.
    fn named(name: &str) -> Option<(Self, usize, usize)> {
        Some(match name {
            "last" => (Self::Last, 0, 0),
            "position" => (Self::Position, 0, 0),
            "count" => (Self::Count, 1, 1),
            "id" => (Self::Id, 1, 1),
            "local-name" => (Self::LocalName, 0, 1),
            "namespace-uri" => (Self::NamespaceUri, 0, 1),
            "name" => (Self::Name, 0, 1),
            "string" => (Self::String, 0, 1),
            "concat" => (Self::Concat, 2, usize::MAX),
            "starts-with" => (Self::StartsWith, 2, 2),
            "contains" => (Self::Contains, 2, 2),
            "substring-before" => (Self::SubstringBefore, 2, 2),
            "substring-after" => (Self::SubstringAfter, 2, 2),
            "substring" => (Self::Substring, 2, 3),
            "string-length" => (Self::StringLength, 0, 1),
            "normalize-space" => (Self::NormalizeSpace, 0, 1),
            "translate" => (Self::Translate, 3, 3),
            "boolean" => (Self::Boolean, 1, 1),
            "not" => (Self::Not, 1, 1),
            "true" => (Self::True, 0, 0),
            "false" => (Self::False, 0, 0),
            "lang" => (Self::Lang, 1, 1),
            "number" => (Self::Number, 0, 1),
            "sum" => (Self::Sum, 1, 1),
            "floor" => (Self::Floor, 1, 1),
            "ceiling" => (Self::Ceiling, 1, 1),
            "round" => (Self::Round, 1, 1),
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Expr {
    Binary(BinaryOp, Box<Expr>, Box<Expr>),
    Negate(Box<Expr>),
    Literal(String),
    Number(f64),
    Call(Function, Vec<Expr>),
    /// [20] FilterExpr: a primary expression filtered by predicates
    /// (with respect to the child axis).
    Filter(Box<Expr>, Vec<Expr>),
    /// [1] LocationPath, or [19] FilterExpr '/' RelativeLocationPath when
    /// `start` is a filter: the steps apply to the start's node-set.
    Path {
        start: PathStart,
        steps: Vec<Step>,
    },
}

#[derive(Clone, Debug, PartialEq)]
enum PathStart {
    /// A relative location path: the context node.
    Context,
    /// An absolute location path: the root node of the context node's tree.
    Root,
    /// A filter expression, which must evaluate to a node-set.
    Filter(Box<Expr>),
}

/// A parsed XPath expression, ready for evaluation once its prefixes are
/// resolved (DOM L3 XPathEvaluator.createExpression "with resolved
/// namespaces").
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct XPathExpression {
    expr: Expr,
}

struct Parser {
    tokens: Vec<Token>,
    position: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.position)
    }

    fn peek_at(&self, offset: usize) -> Option<&Token> {
        self.tokens.get(self.position + offset)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.position).cloned();
        if token.is_some() {
            self.position += 1;
        }
        token
    }

    fn eat(&mut self, token: &Token) -> bool {
        if self.peek() == Some(token) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, token: &Token) -> Result<(), XPathError> {
        if self.eat(token) {
            Ok(())
        } else {
            Err(XPathError::Syntax(format!("expected {token:?}")))
        }
    }

    fn operator_name(&self, name: &str) -> bool {
        matches!(self.peek(), Some(Token::OperatorName(n)) if n == name)
    }

    // [14] Expr ::= OrExpr
    fn expr(&mut self) -> Result<Expr, XPathError> {
        self.or_expr()
    }

    // [21] OrExpr ::= AndExpr | OrExpr 'or' AndExpr
    fn or_expr(&mut self) -> Result<Expr, XPathError> {
        let mut left = self.and_expr()?;
        while self.operator_name("or") {
            self.position += 1;
            let right = self.and_expr()?;
            left = Expr::Binary(BinaryOp::Or, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    // [22] AndExpr ::= EqualityExpr | AndExpr 'and' EqualityExpr
    fn and_expr(&mut self) -> Result<Expr, XPathError> {
        let mut left = self.equality_expr()?;
        while self.operator_name("and") {
            self.position += 1;
            let right = self.equality_expr()?;
            left = Expr::Binary(BinaryOp::And, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    // [23] EqualityExpr
    fn equality_expr(&mut self) -> Result<Expr, XPathError> {
        let mut left = self.relational_expr()?;
        loop {
            let op = match self.peek() {
                Some(Token::Equal) => BinaryOp::Equal,
                Some(Token::NotEqual) => BinaryOp::NotEqual,
                _ => return Ok(left),
            };
            self.position += 1;
            let right = self.relational_expr()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
    }

    // [24] RelationalExpr
    fn relational_expr(&mut self) -> Result<Expr, XPathError> {
        let mut left = self.additive_expr()?;
        loop {
            let op = match self.peek() {
                Some(Token::Less) => BinaryOp::Less,
                Some(Token::LessEqual) => BinaryOp::LessEqual,
                Some(Token::Greater) => BinaryOp::Greater,
                Some(Token::GreaterEqual) => BinaryOp::GreaterEqual,
                _ => return Ok(left),
            };
            self.position += 1;
            let right = self.additive_expr()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
    }

    // [25] AdditiveExpr
    fn additive_expr(&mut self) -> Result<Expr, XPathError> {
        let mut left = self.multiplicative_expr()?;
        loop {
            let op = match self.peek() {
                Some(Token::Plus) => BinaryOp::Plus,
                Some(Token::Minus) => BinaryOp::Minus,
                _ => return Ok(left),
            };
            self.position += 1;
            let right = self.multiplicative_expr()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
    }

    // [26] MultiplicativeExpr
    fn multiplicative_expr(&mut self) -> Result<Expr, XPathError> {
        let mut left = self.unary_expr()?;
        loop {
            let op = match self.peek() {
                Some(Token::Multiply) => BinaryOp::Multiply,
                Some(Token::OperatorName(name)) if name == "div" => BinaryOp::Div,
                Some(Token::OperatorName(name)) if name == "mod" => BinaryOp::Mod,
                _ => return Ok(left),
            };
            self.position += 1;
            let right = self.unary_expr()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
    }

    // [27] UnaryExpr ::= UnionExpr | '-' UnaryExpr
    fn unary_expr(&mut self) -> Result<Expr, XPathError> {
        if self.eat(&Token::Minus) {
            return Ok(Expr::Negate(Box::new(self.unary_expr()?)));
        }
        self.union_expr()
    }

    // [18] UnionExpr ::= PathExpr | UnionExpr '|' PathExpr
    fn union_expr(&mut self) -> Result<Expr, XPathError> {
        let mut left = self.path_expr()?;
        while self.eat(&Token::Pipe) {
            let right = self.path_expr()?;
            left = Expr::Binary(BinaryOp::Union, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    // [19] PathExpr
    fn path_expr(&mut self) -> Result<Expr, XPathError> {
        match self.peek() {
            Some(
                Token::Variable(..)
                | Token::LParen
                | Token::Literal(_)
                | Token::Number(_)
                | Token::FunctionName(..),
            ) => {
                let filter = self.filter_expr()?;
                if self.eat(&Token::Slash) {
                    let steps = self.relative_location_path()?;
                    return Ok(Expr::Path {
                        start: PathStart::Filter(Box::new(filter)),
                        steps,
                    });
                }
                if self.eat(&Token::SlashSlash) {
                    let mut steps = vec![descendant_or_self_step()];
                    steps.extend(self.relative_location_path()?);
                    return Ok(Expr::Path {
                        start: PathStart::Filter(Box::new(filter)),
                        steps,
                    });
                }
                Ok(filter)
            }
            _ => self.location_path(),
        }
    }

    // [20] FilterExpr ::= PrimaryExpr | FilterExpr Predicate
    fn filter_expr(&mut self) -> Result<Expr, XPathError> {
        let primary = self.primary_expr()?;
        let predicates = self.predicates()?;
        Ok(if predicates.is_empty() {
            primary
        } else {
            Expr::Filter(Box::new(primary), predicates)
        })
    }

    // [15] PrimaryExpr
    fn primary_expr(&mut self) -> Result<Expr, XPathError> {
        match self.next() {
            Some(Token::Variable(..)) => {
                // The DOM interfaces supply no variable bindings: an
                // expression with a variable reference is not legal here
                // (DOM L3 XPath INVALID_EXPRESSION_ERR).
                Err(XPathError::Syntax(
                    "variable references are not supported".into(),
                ))
            }
            Some(Token::LParen) => {
                let expr = self.expr()?;
                self.expect(&Token::RParen)?;
                Ok(expr)
            }
            Some(Token::Literal(text)) => Ok(Expr::Literal(text)),
            Some(Token::Number(number)) => Ok(Expr::Number(number)),
            Some(Token::FunctionName(prefix, name)) => {
                // [16] FunctionCall. Only the core library exists; a
                // prefixed (extension) function is not supported.
                let function = prefix
                    .is_none()
                    .then(|| Function::named(&name))
                    .flatten()
                    .ok_or_else(|| XPathError::Syntax(format!("unknown function {name}")))?;
                self.expect(&Token::LParen)?;
                let mut args = Vec::new();
                if !self.eat(&Token::RParen) {
                    loop {
                        args.push(self.expr()?);
                        if self.eat(&Token::RParen) {
                            break;
                        }
                        self.expect(&Token::Comma)?;
                    }
                }
                let (function, min, max) = function;
                if args.len() < min || args.len() > max {
                    return Err(XPathError::Syntax(format!(
                        "wrong argument count for {name}"
                    )));
                }
                Ok(Expr::Call(function, args))
            }
            _ => Err(XPathError::Syntax("expected an expression".into())),
        }
    }

    fn predicates(&mut self) -> Result<Vec<Expr>, XPathError> {
        let mut predicates = Vec::new();
        // [8] Predicate ::= '[' PredicateExpr ']'
        while self.eat(&Token::LBracket) {
            predicates.push(self.expr()?);
            self.expect(&Token::RBracket)?;
        }
        Ok(predicates)
    }

    fn starts_step(&self) -> bool {
        matches!(
            self.peek(),
            Some(
                Token::Dot
                    | Token::DotDot
                    | Token::At
                    | Token::AxisName(_)
                    | Token::NameTest(..)
                    | Token::NodeType(_)
            )
        )
    }

    // [1] LocationPath, [2] AbsoluteLocationPath, [10]
    fn location_path(&mut self) -> Result<Expr, XPathError> {
        if self.eat(&Token::Slash) {
            let steps = if self.starts_step() {
                self.relative_location_path()?
            } else {
                Vec::new()
            };
            return Ok(Expr::Path {
                start: PathStart::Root,
                steps,
            });
        }
        if self.eat(&Token::SlashSlash) {
            let mut steps = vec![descendant_or_self_step()];
            steps.extend(self.relative_location_path()?);
            return Ok(Expr::Path {
                start: PathStart::Root,
                steps,
            });
        }
        Ok(Expr::Path {
            start: PathStart::Context,
            steps: self.relative_location_path()?,
        })
    }

    // [3] RelativeLocationPath, [11]
    fn relative_location_path(&mut self) -> Result<Vec<Step>, XPathError> {
        let mut steps = vec![self.step()?];
        loop {
            if self.eat(&Token::Slash) {
                steps.push(self.step()?);
            } else if self.eat(&Token::SlashSlash) {
                steps.push(descendant_or_self_step());
                steps.push(self.step()?);
            } else {
                return Ok(steps);
            }
        }
    }

    // [4] Step, [5] AxisSpecifier, [7] NodeTest, [12], [13]
    fn step(&mut self) -> Result<Step, XPathError> {
        if self.eat(&Token::Dot) {
            return Ok(Step {
                axis: Axis::SelfNode,
                test: NodeTest::Node,
                predicates: Vec::new(),
            });
        }
        if self.eat(&Token::DotDot) {
            return Ok(Step {
                axis: Axis::Parent,
                test: NodeTest::Node,
                predicates: Vec::new(),
            });
        }
        let axis = match self.peek() {
            Some(Token::At) => {
                self.position += 1;
                Axis::Attribute
            }
            Some(Token::AxisName(name)) => {
                let axis = Axis::named(name)
                    .ok_or_else(|| XPathError::Syntax(format!("unknown axis {name}")))?;
                self.position += 1;
                self.expect(&Token::ColonColon)?;
                axis
            }
            _ => Axis::Child,
        };
        let test = match self.next() {
            Some(Token::NameTest(prefix, local)) => NodeTest::Name {
                prefix,
                namespace: None,
                local,
            },
            Some(Token::NodeType(kind)) => {
                self.expect(&Token::LParen)?;
                let test = match kind.as_str() {
                    "comment" => NodeTest::Comment,
                    "text" => NodeTest::Text,
                    "node" => NodeTest::Node,
                    _ => match self.peek() {
                        Some(Token::Literal(target)) => {
                            let target = target.clone();
                            self.position += 1;
                            NodeTest::ProcessingInstruction(Some(target))
                        }
                        _ => NodeTest::ProcessingInstruction(None),
                    },
                };
                self.expect(&Token::RParen)?;
                test
            }
            _ => return Err(XPathError::Syntax("expected a node test".into())),
        };
        let predicates = self.predicates()?;
        Ok(Step {
            axis,
            test,
            predicates,
        })
    }
}

fn descendant_or_self_step() -> Step {
    // §2.5: `//` is short for /descendant-or-self::node()/.
    Step {
        axis: Axis::DescendantOrSelf,
        test: NodeTest::Node,
        predicates: Vec::new(),
    }
}

impl XPathExpression {
    /// Parse an XPath 1.0 expression ([14] Expr). A syntax error, an unknown
    /// function or axis, or a variable reference is `XPathError::Syntax`.
    pub(crate) fn parse(source: &str) -> Result<Self, XPathError> {
        let tokens = tokenize(source)?;
        let mut parser = Parser {
            tokens,
            position: 0,
        };
        let expr = parser.expr()?;
        if parser.peek().is_some() {
            return Err(XPathError::Syntax("unexpected trailing tokens".into()));
        }
        let _ = parser.peek_at(0);
        Ok(Self { expr })
    }

    /// The namespace prefixes the expression's name tests use, in order of
    /// first appearance, for the namespace resolver.
    pub(crate) fn prefixes(&self) -> Vec<String> {
        let mut prefixes = Vec::new();
        visit_name_tests(&self.expr, &mut |test| {
            if let NodeTest::Name {
                prefix: Some(prefix),
                ..
            } = test
                && !prefixes.contains(prefix)
            {
                prefixes.push(prefix.clone());
            }
        });
        prefixes
    }

    /// Bind each prefix to its namespace (§2.3: "It is an error if the QName
    /// has a prefix for which there is no namespace declaration").
    pub(crate) fn resolve(
        &mut self,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<(), XPathError> {
        let mut failure = None;
        visit_name_tests_mut(&mut self.expr, &mut |test| {
            if let NodeTest::Name {
                prefix: Some(prefix),
                namespace,
                ..
            } = test
            {
                // The xml prefix is always bound (XML Names).
                let resolved = if prefix == "xml" {
                    Some(XML_NS.to_string())
                } else {
                    lookup(prefix).filter(|uri| !uri.is_empty())
                };
                match resolved {
                    Some(uri) => *namespace = Some(uri),
                    None => {
                        failure.get_or_insert_with(|| XPathError::Namespace(prefix.clone()));
                    }
                }
            }
        });
        failure.map_or(Ok(()), Err)
    }
}

fn visit_name_tests(expr: &Expr, visit: &mut dyn FnMut(&NodeTest)) {
    match expr {
        Expr::Binary(_, left, right) => {
            visit_name_tests(left, visit);
            visit_name_tests(right, visit);
        }
        Expr::Negate(inner) => visit_name_tests(inner, visit),
        Expr::Call(_, args) => args.iter().for_each(|arg| visit_name_tests(arg, visit)),
        Expr::Filter(primary, predicates) => {
            visit_name_tests(primary, visit);
            predicates.iter().for_each(|p| visit_name_tests(p, visit));
        }
        Expr::Path { start, steps } => {
            if let PathStart::Filter(filter) = start {
                visit_name_tests(filter, visit);
            }
            for step in steps {
                visit(&step.test);
                step.predicates
                    .iter()
                    .for_each(|p| visit_name_tests(p, visit));
            }
        }
        Expr::Literal(_) | Expr::Number(_) => {}
    }
}

fn visit_name_tests_mut(expr: &mut Expr, visit: &mut dyn FnMut(&mut NodeTest)) {
    match expr {
        Expr::Binary(_, left, right) => {
            visit_name_tests_mut(left, visit);
            visit_name_tests_mut(right, visit);
        }
        Expr::Negate(inner) => visit_name_tests_mut(inner, visit),
        Expr::Call(_, args) => args
            .iter_mut()
            .for_each(|arg| visit_name_tests_mut(arg, visit)),
        Expr::Filter(primary, predicates) => {
            visit_name_tests_mut(primary, visit);
            predicates
                .iter_mut()
                .for_each(|p| visit_name_tests_mut(p, visit));
        }
        Expr::Path { start, steps } => {
            if let PathStart::Filter(filter) = start {
                visit_name_tests_mut(filter, visit);
            }
            for step in steps {
                visit(&mut step.test);
                step.predicates
                    .iter_mut()
                    .for_each(|p| visit_name_tests_mut(p, visit));
            }
        }
        Expr::Literal(_) | Expr::Number(_) => {}
    }
}

// ---------------------------------------------------------------------------
// §5 Data model over the arena DOM.

/// The evaluation of one expression: the tree, its document order (built on
/// first need) and whether HTML's default element namespace applies.
struct Evaluation<'a> {
    dom: &'a Dom,
    root: NodeId,
    html: bool,
    order: OnceCell<FxHashMap<NodeId, usize>>,
}

/// The dynamic context of §1: context node, position and size.
#[derive(Clone)]
struct Context {
    node: XNode,
    position: usize,
    size: usize,
}

fn is_text(data: &NodeData) -> bool {
    matches!(data, NodeData::Text(_) | NodeData::CData(_))
}

fn text_of(data: &NodeData) -> &str {
    match data {
        NodeData::Text(text) | NodeData::CData(text) => text,
        _ => "",
    }
}

impl<'a> Evaluation<'a> {
    fn data(&self, id: NodeId) -> Option<&'a NodeData> {
        self.dom.nodes.get(id).map(|node| &node.data)
    }

    fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.dom.nodes.get(id)?.parent
    }

    fn first_child(&self, id: NodeId) -> Option<NodeId> {
        self.dom.nodes.get(id)?.first_child
    }

    fn last_child(&self, id: NodeId) -> Option<NodeId> {
        self.dom.nodes.get(id)?.last_child
    }

    fn next_sibling(&self, id: NodeId) -> Option<NodeId> {
        self.dom.nodes.get(id)?.next_sibling
    }

    fn prev_sibling(&self, id: NodeId) -> Option<NodeId> {
        self.dom.nodes.get(id)?.prev_sibling
    }

    fn is_text_node(&self, id: NodeId) -> bool {
        self.data(id).is_some_and(is_text)
    }

    /// The first and last DOM nodes of the run of adjacent text siblings
    /// containing `id`.
    fn text_run(&self, id: NodeId) -> (NodeId, NodeId) {
        let mut first = id;
        while let Some(previous) = self.prev_sibling(first).filter(|&p| self.is_text_node(p)) {
            first = previous;
        }
        let mut last = id;
        while let Some(next) = self.next_sibling(last).filter(|&n| self.is_text_node(n)) {
            last = next;
        }
        (first, last)
    }

    /// DOM L3 XPath §1.2.4: the node representing the logical text node of
    /// the run starting at `first`: its first non-empty DOM node, or `None`
    /// when the whole run is empty (no XPath text node).
    fn text_run_node(&self, first: NodeId) -> Option<NodeId> {
        let mut current = Some(first);
        while let Some(id) = current.filter(|&id| self.is_text_node(id)) {
            if !text_of(self.data(id)?).is_empty() {
                return Some(id);
            }
            current = self.next_sibling(id);
        }
        None
    }

    /// Whether a DOM child is an XPath node (document types are not).
    fn is_tree_node(&self, id: NodeId) -> bool {
        !matches!(self.data(id), Some(NodeData::Doctype(_)) | None)
    }

    /// The XPath children of a DOM node, in document order.
    fn children(&self, id: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        if !matches!(
            self.data(id),
            Some(NodeData::Document | NodeData::Fragment | NodeData::Element { .. })
        ) {
            return out;
        }
        let mut current = self.first_child(id);
        while let Some(child) = current {
            if self.is_text_node(child) {
                let (_, last) = self.text_run(child);
                if let Some(node) = self.text_run_node(child) {
                    out.push(node);
                }
                current = self.next_sibling(last);
                continue;
            }
            if self.is_tree_node(child) {
                out.push(child);
            }
            current = self.next_sibling(child);
        }
        out
    }

    fn descendants(&self, id: NodeId, out: &mut Vec<XNode>) {
        for child in self.children(id) {
            out.push(XNode::Node(child));
            self.descendants(child, out);
        }
    }

    /// The descendants in reverse document order.
    fn descendants_reverse(&self, id: NodeId, out: &mut Vec<XNode>) {
        for child in self.children(id).into_iter().rev() {
            self.descendants_reverse(child, out);
            out.push(XNode::Node(child));
        }
    }

    fn element_attrs(&self, element: NodeId) -> &'a [html5ever::Attribute] {
        match self.data(element) {
            Some(NodeData::Element { attrs, .. }) => attrs,
            _ => &[],
        }
    }

    /// DOM L3 XPath §1.2.2: namespace declarations are not attribute nodes.
    fn is_namespace_declaration(attribute: &html5ever::Attribute) -> bool {
        &*attribute.name.ns == XMLNS_NS
            || (attribute.name.ns.is_empty()
                && attribute.name.prefix.is_none()
                && &*attribute.name.local == "xmlns")
    }

    /// DOM L3 XPath §1.2.3: the in-scope namespaces of an element, by
    /// prefix ("" for the default namespace): the xml namespace, the
    /// namespace declarations of the element and its ancestors, and the
    /// element's own prefix (or default namespace).
    fn in_scope_namespaces(&self, element: NodeId) -> Vec<(String, String)> {
        let mut chain = Vec::new();
        let mut current = Some(element);
        while let Some(id) = current {
            if matches!(self.data(id), Some(NodeData::Element { .. })) {
                chain.push(id);
            }
            current = self.parent(id);
        }
        let mut scope: Vec<(String, String)> = vec![("xml".into(), XML_NS.into())];
        let declare = |scope: &mut Vec<(String, String)>, prefix: String, uri: String| {
            scope.retain(|(p, _)| *p != prefix);
            scope.push((prefix, uri));
        };
        for &id in chain.iter().rev() {
            for attribute in self.element_attrs(id) {
                if !Self::is_namespace_declaration(attribute) {
                    continue;
                }
                let prefix = if attribute.name.prefix.is_some() {
                    attribute.name.local.to_string()
                } else {
                    String::new()
                };
                if prefix == "xml" || prefix == "xmlns" {
                    continue;
                }
                declare(&mut scope, prefix, attribute.value.to_string());
            }
            if let Some(NodeData::Element { name, .. }) = self.data(id) {
                let prefix = name.prefix.as_deref().unwrap_or("").to_string();
                if prefix != "xml" {
                    declare(&mut scope, prefix, name.ns.to_string());
                }
            }
        }
        // An empty URI undeclares a prefix (XML Names 1.1 / the default namespace).
        scope.retain(|(_, uri)| !uri.is_empty());
        scope.sort();
        scope
    }

    /// Document order keys (§5): a node's position in a pre-order walk of
    /// the tree; attributes and namespace nodes follow their element and
    /// precede its children, namespace nodes first.
    fn order_key(&self, node: &XNode) -> (usize, u8, usize, String) {
        let order = self.order.get_or_init(|| {
            let mut order = FxHashMap::default();
            let mut stack = vec![self.root];
            let mut index = 0usize;
            while let Some(id) = stack.pop() {
                order.insert(id, index);
                index += 1;
                let mut child = self.last_child(id);
                while let Some(c) = child {
                    stack.push(c);
                    child = self.prev_sibling(c);
                }
            }
            order
        });
        let position = |id: &NodeId| order.get(id).copied().unwrap_or(usize::MAX);
        match node {
            XNode::Node(id) => (position(id), 0, 0, String::new()),
            XNode::Namespace(id, prefix) => (position(id), 1, 0, prefix.clone()),
            XNode::Attribute(id, index) => (position(id), 2, *index, String::new()),
        }
    }

    fn sort(&self, nodes: &mut Vec<XNode>) {
        nodes.sort_by_cached_key(|node| self.order_key(node));
        nodes.dedup();
    }

    // §2.2 Axes.
    fn axis(&self, axis: Axis, node: &XNode) -> Vec<XNode> {
        let mut out = Vec::new();
        match axis {
            Axis::SelfNode => out.push(node.clone()),
            Axis::Child => {
                if let XNode::Node(id) = node {
                    out.extend(self.children(*id).into_iter().map(XNode::Node));
                }
            }
            Axis::Descendant | Axis::DescendantOrSelf => {
                if axis == Axis::DescendantOrSelf {
                    out.push(node.clone());
                }
                if let XNode::Node(id) = node {
                    self.descendants(*id, &mut out);
                }
            }
            Axis::Parent => out.extend(self.parent_of(node).map(XNode::Node)),
            Axis::Ancestor | Axis::AncestorOrSelf => {
                if axis == Axis::AncestorOrSelf {
                    out.push(node.clone());
                }
                let mut current = self.parent_of(node);
                while let Some(id) = current {
                    out.push(XNode::Node(id));
                    current = self.parent(id);
                }
            }
            Axis::FollowingSibling => {
                if let XNode::Node(id) = node {
                    let start = if self.is_text_node(*id) {
                        self.text_run(*id).1
                    } else {
                        *id
                    };
                    if let Some(parent) = self.parent(start) {
                        let start_key = self.order_key(&XNode::Node(start)).0;
                        out.extend(
                            self.children(parent)
                                .into_iter()
                                .filter(|&c| self.order_key(&XNode::Node(c)).0 > start_key)
                                .map(XNode::Node),
                        );
                    }
                }
            }
            Axis::PrecedingSibling => {
                if let XNode::Node(id) = node {
                    let start = if self.is_text_node(*id) {
                        self.text_run(*id).0
                    } else {
                        *id
                    };
                    if let Some(parent) = self.parent(start) {
                        let start_key = self.order_key(&XNode::Node(start)).0;
                        out.extend(
                            self.children(parent)
                                .into_iter()
                                .rev()
                                .filter(|&c| self.order_key(&XNode::Node(c)).0 < start_key)
                                .map(XNode::Node),
                        );
                    }
                }
            }
            Axis::Following => {
                // Everything after the node in document order except its
                // descendants: for an attribute or namespace node, its
                // element's descendants come after it.
                let (mut current, _) = match node {
                    XNode::Node(id) => {
                        let id = if self.is_text_node(*id) {
                            self.text_run(*id).1
                        } else {
                            *id
                        };
                        (Some(id), ())
                    }
                    XNode::Attribute(element, _) | XNode::Namespace(element, _) => {
                        self.descendants(*element, &mut out);
                        (Some(*element), ())
                    }
                };
                while let Some(id) = current {
                    let mut sibling = self.next_sibling(id);
                    while let Some(s) = sibling {
                        if self.is_text_node(s) {
                            let (_, last) = self.text_run(s);
                            if let Some(text) = self.text_run_node(s) {
                                out.push(XNode::Node(text));
                            }
                            sibling = self.next_sibling(last);
                            continue;
                        }
                        if self.is_tree_node(s) {
                            out.push(XNode::Node(s));
                            self.descendants(s, &mut out);
                        }
                        sibling = self.next_sibling(s);
                    }
                    current = self.parent(id);
                }
            }
            Axis::Preceding => {
                // Everything before the node in document order except its
                // ancestors, in reverse document order.
                let mut current = match node {
                    XNode::Node(id) => Some(if self.is_text_node(*id) {
                        self.text_run(*id).0
                    } else {
                        *id
                    }),
                    XNode::Attribute(element, _) | XNode::Namespace(element, _) => Some(*element),
                };
                while let Some(id) = current {
                    let mut sibling = self.prev_sibling(id);
                    while let Some(s) = sibling {
                        if self.is_text_node(s) {
                            let (first, _) = self.text_run(s);
                            if let Some(text) = self.text_run_node(first) {
                                out.push(XNode::Node(text));
                            }
                            sibling = self.prev_sibling(first);
                            continue;
                        }
                        if self.is_tree_node(s) {
                            self.descendants_reverse(s, &mut out);
                            out.push(XNode::Node(s));
                        }
                        sibling = self.prev_sibling(s);
                    }
                    current = self.parent(id);
                }
            }
            Axis::Attribute => {
                if let XNode::Node(id) = node {
                    for (index, attribute) in self.element_attrs(*id).iter().enumerate() {
                        if !Self::is_namespace_declaration(attribute) {
                            out.push(XNode::Attribute(*id, index));
                        }
                    }
                }
            }
            Axis::Namespace => {
                if let XNode::Node(id) = node
                    && matches!(self.data(*id), Some(NodeData::Element { .. }))
                {
                    out.extend(
                        self.in_scope_namespaces(*id)
                            .into_iter()
                            .map(|(prefix, _)| XNode::Namespace(*id, prefix)),
                    );
                }
            }
        }
        out
    }

    fn parent_of(&self, node: &XNode) -> Option<NodeId> {
        match node {
            XNode::Node(id) => self.parent(*id),
            XNode::Attribute(element, _) | XNode::Namespace(element, _) => Some(*element),
        }
    }

    // §2.3 Node tests.
    fn matches(&self, axis: Axis, test: &NodeTest, node: &XNode) -> bool {
        match test {
            NodeTest::Node => true,
            NodeTest::Text => matches!(node, XNode::Node(id) if self.is_text_node(*id)),
            NodeTest::Comment => {
                matches!(node, XNode::Node(id) if matches!(self.data(*id), Some(NodeData::Comment(_))))
            }
            NodeTest::ProcessingInstruction(target) => match node {
                XNode::Node(id) => match self.data(*id) {
                    Some(NodeData::ProcessingInstruction { target: name, .. }) => {
                        target.as_ref().is_none_or(|target| target == name)
                    }
                    _ => false,
                },
                _ => false,
            },
            NodeTest::Name {
                prefix,
                namespace,
                local,
            } => match (axis, node) {
                (Axis::Attribute, XNode::Attribute(element, index)) => {
                    let Some(attribute) = self.element_attrs(*element).get(*index) else {
                        return false;
                    };
                    let ns: &str = &attribute.name.ns;
                    // An unprefixed name test of an attribute has no namespace.
                    let wanted_ns = if prefix.is_some() {
                        namespace.as_deref().unwrap_or("")
                    } else {
                        ""
                    };
                    match local {
                        None if prefix.is_none() => true,
                        None => ns == wanted_ns,
                        Some(local) => ns == wanted_ns && &*attribute.name.local == local,
                    }
                }
                (Axis::Namespace, XNode::Namespace(_, node_prefix)) => match local {
                    // A namespace node's expanded-name has a null URI and
                    // its prefix as the local part.
                    None => prefix.is_none(),
                    Some(local) => prefix.is_none() && node_prefix == local,
                },
                (Axis::Attribute | Axis::Namespace, _) => false,
                (_, XNode::Node(id)) => {
                    let Some(NodeData::Element { name, .. }) = self.data(*id) else {
                        return false;
                    };
                    let ns: &str = &name.ns;
                    match (prefix, local) {
                        (None, None) => true,
                        (Some(_), None) => Some(ns) == namespace.as_deref(),
                        (Some(_), Some(local)) => {
                            Some(ns) == namespace.as_deref() && &*name.local == local
                        }
                        // HTML "Interactions with XPath and XSLT": without a
                        // prefix the default element namespace is the HTML
                        // namespace for a context node from an HTML
                        // document, otherwise null. An HTML element's name
                        // compares ASCII case-insensitively, as an HTML
                        // document's element names are lowercase.
                        (None, Some(local)) => {
                            if self.html {
                                ns == HTML_NS && (*name.local).eq_ignore_ascii_case(local.as_str())
                            } else {
                                ns.is_empty() && &*name.local == local
                            }
                        }
                    }
                }
                _ => false,
            },
        }
    }

    // §5 string-values.
    fn string_value(&self, node: &XNode) -> String {
        match node {
            XNode::Attribute(element, index) => self
                .element_attrs(*element)
                .get(*index)
                .map(|attribute| attribute.value.to_string())
                .unwrap_or_default(),
            XNode::Namespace(element, prefix) => self
                .in_scope_namespaces(*element)
                .into_iter()
                .find(|(p, _)| p == prefix)
                .map(|(_, uri)| uri)
                .unwrap_or_default(),
            XNode::Node(id) => match self.data(*id) {
                Some(NodeData::Text(_) | NodeData::CData(_)) => {
                    // The whole logical text node.
                    let (first, last) = self.text_run(*id);
                    let mut text = String::new();
                    let mut current = Some(first);
                    while let Some(n) = current {
                        text.push_str(text_of(self.data(n).unwrap_or(&NodeData::Fragment)));
                        if n == last {
                            break;
                        }
                        current = self.next_sibling(n);
                    }
                    text
                }
                Some(NodeData::Comment(text)) => text.clone(),
                Some(NodeData::ProcessingInstruction { data, .. }) => data.clone(),
                Some(NodeData::Element { .. } | NodeData::Document | NodeData::Fragment) => {
                    let mut text = String::new();
                    self.collect_text(*id, &mut text);
                    text
                }
                _ => String::new(),
            },
        }
    }

    fn collect_text(&self, id: NodeId, out: &mut String) {
        let mut child = self.first_child(id);
        while let Some(c) = child {
            match self.data(c) {
                Some(NodeData::Text(text) | NodeData::CData(text)) => out.push_str(text),
                Some(NodeData::Element { .. }) => self.collect_text(c, out),
                _ => {}
            }
            child = self.next_sibling(c);
        }
    }

    /// The expanded-name parts of a node: (namespace URI, local part, QName).
    fn names(&self, node: &XNode) -> (String, String, String) {
        match node {
            XNode::Attribute(element, index) => self
                .element_attrs(*element)
                .get(*index)
                .map(|attribute| {
                    let local = attribute.name.local.to_string();
                    let qname = match &attribute.name.prefix {
                        Some(prefix) => format!("{prefix}:{local}"),
                        None => local.clone(),
                    };
                    (attribute.name.ns.to_string(), local, qname)
                })
                .unwrap_or_default(),
            XNode::Namespace(_, prefix) => (String::new(), prefix.clone(), prefix.clone()),
            XNode::Node(id) => match self.data(*id) {
                Some(NodeData::Element { name, .. }) => {
                    let local = name.local.to_string();
                    let qname = match &name.prefix {
                        Some(prefix) => format!("{prefix}:{local}"),
                        None => local.clone(),
                    };
                    (name.ns.to_string(), local, qname)
                }
                Some(NodeData::ProcessingInstruction { target, .. }) => {
                    (String::new(), target.clone(), target.clone())
                }
                _ => Default::default(),
            },
        }
    }

    // §3 Expressions.
    fn eval(&self, expr: &Expr, context: &Context) -> Result<XValue, XPathError> {
        Ok(match expr {
            Expr::Literal(text) => XValue::String(text.clone()),
            Expr::Number(number) => XValue::Number(*number),
            Expr::Negate(inner) => XValue::Number(-self.number(&self.eval(inner, context)?)),
            Expr::Binary(op, left, right) => self.binary(*op, left, right, context)?,
            Expr::Call(function, args) => self.call(*function, args, context)?,
            Expr::Filter(primary, predicates) => {
                let XValue::NodeSet(mut nodes) = self.eval(primary, context)? else {
                    return Err(XPathError::Type("a predicate filters a node-set".into()));
                };
                // §3.3: filter with respect to the child axis (document order).
                self.sort(&mut nodes);
                for predicate in predicates {
                    nodes = self.filter(nodes, predicate)?;
                }
                XValue::NodeSet(nodes)
            }
            Expr::Path { start, steps } => {
                let mut nodes = match start {
                    PathStart::Context => vec![context.node.clone()],
                    PathStart::Root => vec![XNode::Node(self.root)],
                    PathStart::Filter(filter) => match self.eval(filter, context)? {
                        XValue::NodeSet(nodes) => nodes,
                        _ => {
                            return Err(XPathError::Type(
                                "a location path composes with a node-set".into(),
                            ));
                        }
                    },
                };
                for step in steps {
                    let mut next = Vec::new();
                    for node in &nodes {
                        let candidates: Vec<XNode> = self
                            .axis(step.axis, node)
                            .into_iter()
                            .filter(|candidate| self.matches(step.axis, &step.test, candidate))
                            .collect();
                        // §2.4: predicates see proximity positions in the
                        // axis direction (reverse document order for the
                        // ancestor, ancestor-or-self, preceding and
                        // preceding-sibling axes), the order `axis` produces.
                        let mut selected = candidates;
                        for predicate in &step.predicates {
                            selected = self.filter(selected, predicate)?;
                        }
                        next.extend(selected);
                    }
                    self.sort(&mut next);
                    nodes = next;
                }
                if steps.is_empty() {
                    self.sort(&mut nodes);
                }
                XValue::NodeSet(nodes)
            }
        })
    }

    /// §2.4: keep the nodes for which the predicate is true at their
    /// position (a number result compares with the position).
    fn filter(&self, nodes: Vec<XNode>, predicate: &Expr) -> Result<Vec<XNode>, XPathError> {
        let size = nodes.len();
        let mut kept = Vec::new();
        for (index, node) in nodes.into_iter().enumerate() {
            let context = Context {
                node: node.clone(),
                position: index + 1,
                size,
            };
            let keep = match self.eval(predicate, &context)? {
                XValue::Number(number) => number == (index + 1) as f64,
                other => self.boolean(&other),
            };
            if keep {
                kept.push(node);
            }
        }
        Ok(kept)
    }

    fn binary(
        &self,
        op: BinaryOp,
        left: &Expr,
        right: &Expr,
        context: &Context,
    ) -> Result<XValue, XPathError> {
        Ok(match op {
            // §3.4: the right operand is not evaluated when the left decides.
            BinaryOp::Or => XValue::Boolean(
                self.boolean(&self.eval(left, context)?)
                    || self.boolean(&self.eval(right, context)?),
            ),
            BinaryOp::And => XValue::Boolean(
                self.boolean(&self.eval(left, context)?)
                    && self.boolean(&self.eval(right, context)?),
            ),
            BinaryOp::Union => {
                let (XValue::NodeSet(mut a), XValue::NodeSet(b)) =
                    (self.eval(left, context)?, self.eval(right, context)?)
                else {
                    return Err(XPathError::Type("'|' combines node-sets".into()));
                };
                a.extend(b);
                self.sort(&mut a);
                XValue::NodeSet(a)
            }
            BinaryOp::Equal
            | BinaryOp::NotEqual
            | BinaryOp::Less
            | BinaryOp::LessEqual
            | BinaryOp::Greater
            | BinaryOp::GreaterEqual => {
                let a = self.eval(left, context)?;
                let b = self.eval(right, context)?;
                XValue::Boolean(self.compare(op, &a, &b))
            }
            BinaryOp::Plus
            | BinaryOp::Minus
            | BinaryOp::Multiply
            | BinaryOp::Div
            | BinaryOp::Mod => {
                let a = self.number(&self.eval(left, context)?);
                let b = self.number(&self.eval(right, context)?);
                XValue::Number(match op {
                    BinaryOp::Plus => a + b,
                    BinaryOp::Minus => a - b,
                    BinaryOp::Multiply => a * b,
                    BinaryOp::Div => a / b,
                    // §3.5: the remainder of a truncating division, as % in ECMAScript.
                    _ => a % b,
                })
            }
        })
    }

    /// §3.4 comparisons.
    fn compare(&self, op: BinaryOp, a: &XValue, b: &XValue) -> bool {
        match (a, b) {
            (XValue::NodeSet(left), XValue::NodeSet(right)) => {
                let right: Vec<String> = right.iter().map(|n| self.string_value(n)).collect();
                left.iter().any(|l| {
                    let l = self.string_value(l);
                    right.iter().any(|r| {
                        compare_atoms(
                            op,
                            &XValue::String(l.clone()),
                            &XValue::String(r.clone()),
                            self,
                        )
                    })
                })
            }
            (XValue::NodeSet(nodes), other) => self.compare_nodes(op, nodes, other, false),
            (other, XValue::NodeSet(nodes)) => self.compare_nodes(op, nodes, other, true),
            _ => compare_atoms(op, a, b, self),
        }
    }

    fn compare_nodes(&self, op: BinaryOp, nodes: &[XNode], other: &XValue, swapped: bool) -> bool {
        let order = |node: XValue| {
            if swapped {
                compare_atoms(op, other, &node, self)
            } else {
                compare_atoms(op, &node, other, self)
            }
        };
        match other {
            XValue::Boolean(_) => order(XValue::Boolean(!nodes.is_empty())),
            XValue::Number(_) => nodes
                .iter()
                .any(|n| order(XValue::Number(string_to_number(&self.string_value(n))))),
            _ => nodes
                .iter()
                .any(|n| order(XValue::String(self.string_value(n)))),
        }
    }

    // §4.2 string()
    fn string(&self, value: &XValue) -> String {
        match value {
            XValue::String(text) => text.clone(),
            XValue::Boolean(b) => if *b { "true" } else { "false" }.to_string(),
            XValue::Number(number) => number_to_string(*number),
            XValue::NodeSet(nodes) => nodes
                .first()
                .map(|n| self.string_value(n))
                .unwrap_or_default(),
        }
    }

    // §4.4 number()
    fn number(&self, value: &XValue) -> f64 {
        match value {
            XValue::Number(number) => *number,
            XValue::Boolean(b) => f64::from(u8::from(*b)),
            XValue::String(text) => string_to_number(text),
            XValue::NodeSet(_) => string_to_number(&self.string(value)),
        }
    }

    // §4.3 boolean()
    fn boolean(&self, value: &XValue) -> bool {
        match value {
            XValue::Boolean(b) => *b,
            XValue::Number(number) => *number != 0.0 && !number.is_nan(),
            XValue::String(text) => !text.is_empty(),
            XValue::NodeSet(nodes) => !nodes.is_empty(),
        }
    }

    fn node_set_arg(&self, arg: &Expr, context: &Context) -> Result<Vec<XNode>, XPathError> {
        match self.eval(arg, context)? {
            XValue::NodeSet(nodes) => Ok(nodes),
            _ => Err(XPathError::Type("the argument must be a node-set".into())),
        }
    }

    fn string_arg(
        &self,
        args: &[Expr],
        index: usize,
        context: &Context,
    ) -> Result<String, XPathError> {
        Ok(match args.get(index) {
            Some(arg) => self.string(&self.eval(arg, context)?),
            None => self.string_value(&context.node),
        })
    }

    fn number_arg(&self, arg: &Expr, context: &Context) -> Result<f64, XPathError> {
        Ok(self.number(&self.eval(arg, context)?))
    }

    /// The first node in document order of an optional node-set argument
    /// (the context node when omitted).
    fn first_node_arg(
        &self,
        args: &[Expr],
        context: &Context,
    ) -> Result<Option<XNode>, XPathError> {
        Ok(match args.first() {
            Some(arg) => {
                let mut nodes = self.node_set_arg(arg, context)?;
                self.sort(&mut nodes);
                nodes.into_iter().next()
            }
            None => Some(context.node.clone()),
        })
    }

    // §4 Core Function Library.
    fn call(
        &self,
        function: Function,
        args: &[Expr],
        context: &Context,
    ) -> Result<XValue, XPathError> {
        Ok(match function {
            Function::Last => XValue::Number(context.size as f64),
            Function::Position => XValue::Number(context.position as f64),
            Function::Count => XValue::Number(self.node_set_arg(&args[0], context)?.len() as f64),
            Function::Id => {
                let value = self.eval(&args[0], context)?;
                let tokens: Vec<String> = match &value {
                    XValue::NodeSet(nodes) => nodes
                        .iter()
                        .flat_map(|n| split_space(&self.string_value(n)))
                        .collect(),
                    other => split_space(&self.string(other)),
                };
                let mut found = Vec::new();
                for token in tokens {
                    if let Some(id) = self.element_with_id(&token) {
                        found.push(XNode::Node(id));
                    }
                }
                self.sort(&mut found);
                XValue::NodeSet(found)
            }
            Function::LocalName => XValue::String(
                self.first_node_arg(args, context)?
                    .map(|n| self.names(&n).1)
                    .unwrap_or_default(),
            ),
            Function::NamespaceUri => XValue::String(
                self.first_node_arg(args, context)?
                    .map(|n| self.names(&n).0)
                    .unwrap_or_default(),
            ),
            Function::Name => XValue::String(
                self.first_node_arg(args, context)?
                    .map(|n| self.names(&n).2)
                    .unwrap_or_default(),
            ),
            Function::String => XValue::String(match args.first() {
                Some(arg) => {
                    let value = self.eval(arg, context)?;
                    self.string(&self.sorted(value))
                }
                None => self.string_value(&context.node),
            }),
            Function::Concat => {
                let mut text = String::new();
                for arg in args {
                    let value = self.eval(arg, context)?;
                    text.push_str(&self.string(&self.sorted(value)));
                }
                XValue::String(text)
            }
            Function::StartsWith => {
                let (a, b) = (
                    self.string_arg(args, 0, context)?,
                    self.string_arg(args, 1, context)?,
                );
                XValue::Boolean(a.starts_with(&b))
            }
            Function::Contains => {
                let (a, b) = (
                    self.string_arg(args, 0, context)?,
                    self.string_arg(args, 1, context)?,
                );
                XValue::Boolean(a.contains(&b))
            }
            Function::SubstringBefore => {
                let (a, b) = (
                    self.string_arg(args, 0, context)?,
                    self.string_arg(args, 1, context)?,
                );
                XValue::String(a.find(&b).map(|i| a[..i].to_string()).unwrap_or_default())
            }
            Function::SubstringAfter => {
                let (a, b) = (
                    self.string_arg(args, 0, context)?,
                    self.string_arg(args, 1, context)?,
                );
                XValue::String(
                    a.find(&b)
                        .map(|i| a[i + b.len()..].to_string())
                        .unwrap_or_default(),
                )
            }
            Function::Substring => {
                let text = self.string_arg(args, 0, context)?;
                let start = round(self.number_arg(&args[1], context)?);
                let end = match args.get(2) {
                    Some(arg) => start + round(self.number_arg(arg, context)?),
                    None => f64::INFINITY,
                };
                // Characters whose position p (from 1) has start <= p < end.
                XValue::String(
                    text.chars()
                        .enumerate()
                        .filter(|(i, _)| {
                            let p = (*i + 1) as f64;
                            p >= start && p < end
                        })
                        .map(|(_, c)| c)
                        .collect(),
                )
            }
            Function::StringLength => {
                XValue::Number(self.string_arg(args, 0, context)?.chars().count() as f64)
            }
            Function::NormalizeSpace => {
                let text = self.string_arg(args, 0, context)?;
                XValue::String(
                    text.split(is_xml_space)
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join(" "),
                )
            }
            Function::Translate => {
                let text = self.string_arg(args, 0, context)?;
                let from: Vec<char> = self.string_arg(args, 1, context)?.chars().collect();
                let to: Vec<char> = self.string_arg(args, 2, context)?.chars().collect();
                XValue::String(
                    text.chars()
                        .filter_map(|c| match from.iter().position(|&f| f == c) {
                            Some(index) => to.get(index).copied(),
                            None => Some(c),
                        })
                        .collect(),
                )
            }
            Function::Boolean => XValue::Boolean(self.boolean(&self.eval(&args[0], context)?)),
            Function::Not => XValue::Boolean(!self.boolean(&self.eval(&args[0], context)?)),
            Function::True => XValue::Boolean(true),
            Function::False => XValue::Boolean(false),
            Function::Lang => {
                let wanted = self.string_arg(args, 0, context)?;
                XValue::Boolean(self.lang(&context.node).is_some_and(|lang| {
                    lang.eq_ignore_ascii_case(&wanted)
                        || (lang.len() > wanted.len()
                            && lang.as_bytes()[wanted.len()] == b'-'
                            && lang[..wanted.len()].eq_ignore_ascii_case(&wanted))
                }))
            }
            Function::Number => XValue::Number(match args.first() {
                Some(arg) => {
                    let value = self.eval(arg, context)?;
                    self.number(&self.sorted(value))
                }
                None => string_to_number(&self.string_value(&context.node)),
            }),
            Function::Sum => XValue::Number(
                self.node_set_arg(&args[0], context)?
                    .iter()
                    .map(|n| string_to_number(&self.string_value(n)))
                    .sum(),
            ),
            Function::Floor => XValue::Number(self.number_arg(&args[0], context)?.floor()),
            Function::Ceiling => XValue::Number(self.number_arg(&args[0], context)?.ceil()),
            Function::Round => XValue::Number(round(self.number_arg(&args[0], context)?)),
        })
    }

    /// A node-set in document order (string() and number() use its first
    /// node in document order).
    fn sorted(&self, value: XValue) -> XValue {
        match value {
            XValue::NodeSet(mut nodes) => {
                self.sort(&mut nodes);
                XValue::NodeSet(nodes)
            }
            other => other,
        }
    }

    /// §4.3 lang(): the xml:lang of the context node or its nearest
    /// ancestor that has one.
    fn lang(&self, node: &XNode) -> Option<String> {
        let mut current = match node {
            XNode::Node(id) => Some(*id),
            XNode::Attribute(element, _) | XNode::Namespace(element, _) => Some(*element),
        };
        while let Some(id) = current {
            if let Some(attribute) = self
                .element_attrs(id)
                .iter()
                .find(|a| &*a.name.ns == XML_NS && &*a.name.local == "lang")
            {
                return Some(attribute.value.to_string());
            }
            current = self.parent(id);
        }
        None
    }

    /// §4.1 id() via DOM L3 XPath §1.3: the element Document.getElementById
    /// returns, in the context node's tree.
    fn element_with_id(&self, token: &str) -> Option<NodeId> {
        let mut stack = vec![self.root];
        while let Some(id) = stack.pop() {
            if let Some(NodeData::Element { attrs, .. }) = self.data(id)
                && attrs
                    .iter()
                    .any(|a| a.name.ns.is_empty() && &*a.name.local == "id" && &*a.value == token)
            {
                return Some(id);
            }
            let mut child = self.last_child(id);
            while let Some(c) = child {
                stack.push(c);
                child = self.prev_sibling(c);
            }
        }
        None
    }
}

fn compare_atoms(op: BinaryOp, a: &XValue, b: &XValue, eval: &Evaluation<'_>) -> bool {
    match op {
        BinaryOp::Equal | BinaryOp::NotEqual => {
            let equal = if matches!(a, XValue::Boolean(_)) || matches!(b, XValue::Boolean(_)) {
                eval.boolean(a) == eval.boolean(b)
            } else if matches!(a, XValue::Number(_)) || matches!(b, XValue::Number(_)) {
                eval.number(a) == eval.number(b)
            } else {
                eval.string(a) == eval.string(b)
            };
            equal == (op == BinaryOp::Equal)
        }
        _ => {
            let (x, y) = (eval.number(a), eval.number(b));
            match op {
                BinaryOp::Less => x < y,
                BinaryOp::LessEqual => x <= y,
                BinaryOp::Greater => x > y,
                _ => x >= y,
            }
        }
    }
}

fn split_space(text: &str) -> Vec<String> {
    text.split(is_xml_space)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// §4.4 round(): the closest integer, ties toward positive infinity, with
/// -0 for values in [-0.5, -0].
fn round(number: f64) -> f64 {
    if number.is_nan() || number.is_infinite() || number == 0.0 {
        return number;
    }
    if (-0.5..0.0).contains(&number) {
        return -0.0;
    }
    (number + 0.5).floor()
}

/// §4.4 number() of a string: optional whitespace, an optional minus sign,
/// a Number, optional whitespace; anything else is NaN.
pub(crate) fn string_to_number(text: &str) -> f64 {
    let trimmed = text.trim_matches(is_xml_space);
    let (negative, digits) = match trimmed.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, trimmed),
    };
    let valid = !digits.is_empty()
        && digits.chars().all(|c| c.is_ascii_digit() || c == '.')
        && digits.matches('.').count() <= 1
        && digits != ".";
    if !valid {
        return f64::NAN;
    }
    let value: f64 = if let Some(rest) = digits.strip_prefix('.') {
        format!("0.{rest}").parse().unwrap_or(f64::NAN)
    } else {
        digits.trim_end_matches('.').parse().unwrap_or(f64::NAN)
    };
    if negative { -value } else { value }
}

/// §4.2 string() of a number: NaN, Infinity, integers without a decimal
/// point, and other numbers in decimal (never exponential) notation with
/// the digits needed to distinguish them.
pub(crate) fn number_to_string(number: f64) -> String {
    if number.is_nan() {
        return "NaN".into();
    }
    if number.is_infinite() {
        return if number > 0.0 {
            "Infinity"
        } else {
            "-Infinity"
        }
        .into();
    }
    if number == 0.0 {
        return "0".into();
    }
    // Rust's Display for f64 is the shortest round-tripping decimal, without
    // an exponent.
    let mut text = String::new();
    let _ = write!(text, "{number}");
    text
}

impl Dom {
    /// Evaluate a resolved XPath expression with `context` as the context
    /// node (position and size 1). The root node is the root of the context
    /// node's DOM tree; HTML's default element namespace applies when the
    /// context node's node document is an HTML document.
    #[cfg(test)]
    pub(crate) fn xpath_evaluate(
        &self,
        expression: &XPathExpression,
        context: XNode,
    ) -> Result<XValue, XPathError> {
        self.xpath_evaluate_as(expression, context, None)
    }

    /// Evaluate a resolved expression as `xpath_evaluate` does, optionally
    /// converting the result as XPath's `number()`, `string()` or
    /// `boolean()` function would (DOM L3 XPath NUMBER_TYPE, STRING_TYPE and
    /// BOOLEAN_TYPE results).
    pub(crate) fn xpath_evaluate_as(
        &self,
        expression: &XPathExpression,
        context: XNode,
        convert: Option<XPathConversion>,
    ) -> Result<XValue, XPathError> {
        let id = match &context {
            XNode::Node(id) | XNode::Attribute(id, _) | XNode::Namespace(id, _) => *id,
        };
        let mut root = id;
        while let Some(parent) = self.nodes.get(root).and_then(|node| node.parent) {
            root = parent;
        }
        let document = if matches!(
            self.nodes.get(id).map(|n| &n.data),
            Some(NodeData::Document)
        ) {
            id
        } else {
            self.owner_document(id).unwrap_or(self.document())
        };
        let evaluation = Evaluation {
            dom: self,
            root,
            html: !self.is_xml_document(document),
            order: OnceCell::new(),
        };
        // DOM L3 XPath §1.2.4: a Text context node stands for its whole
        // logical text node.
        let context = match context {
            XNode::Node(id) if evaluation.is_text_node(id) => {
                let (first, _) = evaluation.text_run(id);
                XNode::Node(evaluation.text_run_node(first).unwrap_or(id))
            }
            other => other,
        };
        let value = evaluation.eval(
            &expression.expr,
            &Context {
                node: context,
                position: 1,
                size: 1,
            },
        )?;
        let value = evaluation.sorted(value);
        Ok(match convert {
            None => value,
            Some(XPathConversion::Number) => XValue::Number(evaluation.number(&value)),
            Some(XPathConversion::String) => XValue::String(evaluation.string(&value)),
            Some(XPathConversion::Boolean) => XValue::Boolean(evaluation.boolean(&value)),
        })
    }

    /// The (namespace, prefix, local name) of an element's attribute at
    /// `index`, for the platform's Attr objects.
    pub(crate) fn xpath_attribute_name(
        &self,
        element: NodeId,
        index: usize,
    ) -> Option<(String, Option<String>, String)> {
        match &self.nodes.get(element)?.data {
            NodeData::Element { attrs, .. } => attrs.get(index).map(|a| {
                (
                    a.name.ns.to_string(),
                    a.name.prefix.as_ref().map(|p| p.to_string()),
                    a.name.local.to_string(),
                )
            }),
            _ => None,
        }
    }

    /// The index of an element's attribute by namespace and local name.
    pub(crate) fn xpath_attribute_index(
        &self,
        element: NodeId,
        namespace: &str,
        local: &str,
    ) -> Option<usize> {
        match &self.nodes.get(element)?.data {
            NodeData::Element { attrs, .. } => attrs
                .iter()
                .position(|a| &*a.name.ns == namespace && &*a.name.local == local),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::DOCUMENT;

    fn eval(dom: &Dom, source: &str) -> XValue {
        eval_at(dom, source, XNode::Node(DOCUMENT))
    }

    fn eval_at(dom: &Dom, source: &str, context: XNode) -> XValue {
        let mut expression = XPathExpression::parse(source).unwrap();
        expression
            .resolve(&|prefix| match prefix {
                "svg" => Some("http://www.w3.org/2000/svg".into()),
                "h" => Some(HTML_NS.into()),
                _ => None,
            })
            .unwrap();
        dom.xpath_evaluate(&expression, context).unwrap()
    }

    fn string(dom: &Dom, source: &str) -> String {
        match eval(dom, &format!("string({source})")) {
            XValue::String(s) => s,
            other => panic!("{other:?}"),
        }
    }

    fn number(dom: &Dom, source: &str) -> f64 {
        match eval(dom, source) {
            XValue::Number(n) => n,
            other => panic!("{other:?}"),
        }
    }

    fn names(dom: &Dom, source: &str) -> Vec<String> {
        match eval(dom, source) {
            XValue::NodeSet(nodes) => nodes
                .iter()
                .map(|node| match node {
                    XNode::Node(id) => dom
                        .attr(*id, "id")
                        .map(str::to_string)
                        .unwrap_or_else(|| dom.text_content(*id)),
                    XNode::Attribute(e, i) => {
                        format!("@{}", dom.xpath_attribute_name(*e, *i).unwrap().2)
                    }
                    XNode::Namespace(_, prefix) => format!("ns:{prefix}"),
                })
                .collect(),
            other => panic!("{other:?}"),
        }
    }

    fn page() -> Dom {
        Dom::parse_document(
            "<html><body><div id=a class=x><p id=p1>one</p><p id=p2 lang=en>two<b id=b1>bold</b></p>\
             <!--note--><span id=s1 title=t>three</span></div><div id=b><p id=p3>four</p></div>\
             <svg id=svg><rect id=r1 /></svg></body></html>",
        )
    }

    #[test]
    fn lexical_rules_disambiguate_operators_and_names() {
        // §3.7: `div` and `*` after an operand are operators; elsewhere names.
        let dom = page();
        assert_eq!(number(&dom, "count(//div)"), 2.0);
        assert_eq!(number(&dom, "6 div 3 * 2"), 4.0);
        assert_eq!(number(&dom, "count(//div/*)"), 4.0);
        assert_eq!(number(&dom, "count(//*[self::div or self::span])"), 3.0);
        assert_eq!(number(&dom, "5 mod -2"), 1.0);
        assert_eq!(number(&dom, "-5 mod 2"), -1.0);
        assert_eq!(number(&dom, "- - 3"), 3.0);
        assert_eq!(number(&dom, ".5 + 1."), 1.5);
        for bad in [
            "",
            "//",
            "foo(",
            "1 +",
            "child::",
            "bogus::x",
            "$x",
            "unknown()",
            "count()",
            "a[1",
            "'unterminated",
            "@",
            "p:q:r",
            "1 2",
        ] {
            assert!(XPathExpression::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn location_paths_axes_and_predicates() {
        let dom = page();
        assert_eq!(names(&dom, "//p"), ["p1", "p2", "p3"]);
        assert_eq!(names(&dom, "/html/body/div[2]/p"), ["p3"]);
        assert_eq!(names(&dom, "//div[@class='x']/p[last()]"), ["p2"]);
        assert_eq!(names(&dom, "//p[1]"), ["p1", "p3"]);
        assert_eq!(names(&dom, "(//p)[1]"), ["p1"]);
        assert_eq!(names(&dom, "/descendant::p[2]"), ["p2"]);
        // §2.4: reverse axes count proximity backwards.
        assert_eq!(names(&dom, "//b/ancestor::*[1]"), ["p2"]);
        assert_eq!(names(&dom, "//span/preceding-sibling::p[1]"), ["p2"]);
        assert_eq!(names(&dom, "//span/preceding::p"), ["p1", "p2"]);
        assert_eq!(
            names(&dom, "//p[@id='p2']/following::*"),
            ["s1", "b", "p3", "svg", "r1"]
        );
        assert_eq!(
            names(&dom, "//p[@id='p1']/following-sibling::*"),
            ["p2", "s1"]
        );
        assert_eq!(
            names(&dom, "//b/ancestor-or-self::*[@id]"),
            ["a", "p2", "b1"]
        );
        assert_eq!(names(&dom, "//span/@*"), ["@id", "@title"]);
        assert_eq!(names(&dom, "//span/@title/.."), ["s1"]);
        assert_eq!(names(&dom, "//comment()"), ["note"]);
        assert_eq!(names(&dom, "//p[@id='p2']/text()"), ["two"]);
        assert_eq!(names(&dom, "//p | //span | //p"), ["p1", "p2", "s1", "p3"]);
        assert_eq!(names(&dom, "id('b p1 none')"), ["p1", "b"]);
        assert_eq!(names(&dom, "//*[lang('EN')]"), Vec::<String>::new());
        assert_eq!(
            number(&dom, "count(//node())"),
            number(&dom, "count(//node())")
        );
    }

    #[test]
    fn html_documents_use_the_html_namespace_for_unprefixed_names() {
        // HTML #xpath-1.0-processors: unprefixed element name tests use the
        // HTML namespace; other namespaces need a prefix.
        let dom = page();
        assert_eq!(names(&dom, "//DIV"), ["a", "b"]);
        assert_eq!(names(&dom, "//rect"), Vec::<String>::new());
        assert_eq!(names(&dom, "//svg:rect"), ["r1"]);
        assert_eq!(names(&dom, "//h:p[2]"), ["p2"]);
        assert_eq!(names(&dom, "//svg:*"), ["svg", "r1"]);
        // Attribute names are not in a namespace.
        assert_eq!(names(&dom, "//*[@ID]"), Vec::<String>::new());
        let mut xml = Dom::parse_xml_page(
            "<root xmlns:a='urn:a'><Item/><a:item/><item xml:lang='en-GB'/></root>",
            "application/xml",
        );
        xml.set_doc_url(None);
        assert_eq!(number(&xml, "count(/*)"), 1.0);
        assert_eq!(number(&xml, "count(//item)"), 1.0);
        assert_eq!(number(&xml, "count(//Item)"), 1.0);
        assert_eq!(number(&xml, "count(//*[lang('en')])"), 1.0);
        assert_eq!(string(&xml, "namespace-uri(//item/@*)"), XML_NS);
        assert_eq!(number(&xml, "count(/root/namespace::*)"), 2.0);
        assert_eq!(string(&xml, "name(/root/*[2])"), "a:item");
        assert_eq!(string(&xml, "namespace-uri(/root/*[2])"), "urn:a");
        assert_eq!(string(&xml, "/root/namespace::a"), "urn:a");
    }

    #[test]
    fn core_functions_follow_the_recommendation() {
        let dom = page();
        assert_eq!(string(&dom, "substring('12345', 1.5, 2.6)"), "234");
        assert_eq!(string(&dom, "substring('12345', 0, 3)"), "12");
        assert_eq!(string(&dom, "substring('12345', 0 div 0, 3)"), "");
        assert_eq!(string(&dom, "substring('12345', 1, 0 div 0)"), "");
        assert_eq!(string(&dom, "substring('12345', -42, 1 div 0)"), "12345");
        assert_eq!(string(&dom, "substring('12345', -1 div 0, 1 div 0)"), "");
        assert_eq!(string(&dom, "substring-before('1999/04/01', '/')"), "1999");
        assert_eq!(
            string(&dom, "substring-after('1999/04/01', '19')"),
            "99/04/01"
        );
        assert_eq!(string(&dom, "translate('--aaa--', 'abc-', 'ABC')"), "AAA");
        assert_eq!(string(&dom, "normalize-space('  a \t b  ')"), "a b");
        assert_eq!(string(&dom, "concat('a', 1, true())"), "a1true");
        assert_eq!(string(&dom, "1 div 0"), "Infinity");
        assert_eq!(string(&dom, "0 div 0"), "NaN");
        assert_eq!(string(&dom, "-0"), "0");
        assert_eq!(
            string(&dom, "1000000 * 1000000 * 1000000 * 1000"),
            "1000000000000000000000"
        );
        assert_eq!(string(&dom, "1 div 10000000"), "0.0000001");
        assert_eq!(string(&dom, "3.50"), "3.5");
        assert_eq!(number(&dom, "number(' -12.5 ')"), -12.5);
        assert!(number(&dom, "number('1e2')").is_nan());
        assert!(number(&dom, "number('+1')").is_nan());
        assert_eq!(number(&dom, "round(2.5)"), 3.0);
        assert_eq!(number(&dom, "round(-2.5)"), -2.0);
        assert!(number(&dom, "round(-0.2)").is_sign_negative());
        assert_eq!(number(&dom, "string-length('\u{1F600}a')"), 2.0);
        assert_eq!(number(&dom, "sum(//p/@id[. = 'none'])"), 0.0);
        assert_eq!(string(&dom, "//div[1]"), "onetwoboldthree");
        assert_eq!(string(&dom, "local-name(//span/@title)"), "title");
        assert_eq!(string(&dom, "name(//p)"), "p");
        assert_eq!(number(&dom, "count(//p[contains(., 'o')])"), 3.0);
        assert_eq!(
            number(
                &dom,
                "count(//p[starts-with(@id, 'p')][position() = last()])"
            ),
            2.0
        );
        assert_eq!(
            number(
                &dom,
                "count((//p)[starts-with(@id, 'p')][position() = last()])"
            ),
            1.0
        );
        // §3.4: comparisons involving node-sets.
        assert_eq!(eval(&dom, "//p = 'four'"), XValue::Boolean(true));
        assert_eq!(eval(&dom, "//p != 'four'"), XValue::Boolean(true));
        assert_eq!(eval(&dom, "not(//p != //p)"), XValue::Boolean(false));
        assert_eq!(eval(&dom, "//nothing = //nothing"), XValue::Boolean(false));
        assert_eq!(eval(&dom, "3 > 2 > 1"), XValue::Boolean(false));
        assert_eq!(eval(&dom, "true() = 'x'"), XValue::Boolean(true));
        // Type errors.
        let expression = XPathExpression::parse("count(1)").unwrap();
        assert!(matches!(
            dom.xpath_evaluate(&expression, XNode::Node(DOCUMENT)),
            Err(XPathError::Type(_))
        ));
        let expression = XPathExpression::parse("(1)/p").unwrap();
        assert!(matches!(
            dom.xpath_evaluate(&expression, XNode::Node(DOCUMENT)),
            Err(XPathError::Type(_))
        ));
    }

    #[test]
    fn adjacent_text_nodes_are_one_logical_text_node() {
        // DOM L3 XPath §1.2.4: the first non-empty DOM node represents the
        // run; the context node stands for its whole logical text node.
        let mut dom = Dom::parse_document("<p id=p>a</p>");
        let p = dom.get_by_id("p").unwrap();
        let empty = dom.create_text("");
        let first = dom.nodes[p].first_child.unwrap();
        let second = dom.create_text("b");
        let cdata_less = dom.create_text("c");
        dom.insert_before(p, empty, Some(first));
        dom.append(p, second);
        dom.append(p, cdata_less);
        let texts = eval_at(&dom, "text()", XNode::Node(p));
        assert_eq!(texts, XValue::NodeSet(vec![XNode::Node(first)]));
        assert_eq!(
            eval_at(&dom, "string(.)", XNode::Node(second)),
            XValue::String("abc".into())
        );
        assert_eq!(
            eval_at(
                &dom,
                "count(preceding-sibling::node())",
                XNode::Node(second)
            ),
            XValue::Number(0.0)
        );
        let unresolved = XPathExpression::parse("//q:x").unwrap();
        assert_eq!(unresolved.prefixes(), ["q"]);
        let mut unresolved = unresolved;
        assert_eq!(
            unresolved.resolve(&|_| None),
            Err(XPathError::Namespace("q".into()))
        );
    }
}
