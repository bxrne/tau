//! Abstract syntax tree for the Tau query language.
//!
//! The grammar is deliberately minimal:
//!
//! ```text
//! stmt   := create | append | derive | at | range | drop
//!
//! create := CREATE LENS <ident> <type>
//! append := APPEND LENS <ident> <int> <int> <literal>
//! derive := DERIVE LENS <ident> AS <expr>
//! at     := AT     LENS <ident> <int>
//! range  := RANGE  LENS <ident> <int> <int> [WHERE <expr>]
//! drop   := DROP   LENS <ident>
//!
//! type    := int | float | str | bool | bytes
//! literal := int | float | string | bool | null
//! expr    := disjunction      (full operator-precedence grammar; see parser)
//! ```
//!
//! Keywords are case-insensitive; identifiers and literals are not.

/// Declared value type for a base lens. Used only as a creation hint;
/// the storage engine itself is generic over `V`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Type {
    Int,
    Float,
    Str,
    Bool,
    Bytes,
}

/// A literal value embedded in a query.  Floats use a string form so we can
/// keep `Literal: Eq`, which is convenient for AST tests.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    Null,
}

/// Aggregation function applied to a lens over a time range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggFunc {
    Min,
    Max,
    Avg,
    Sum,
    Count,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
}

/// Expression used by `DERIVE` and `WHERE`.  Identifiers reference other
/// lenses by name.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Lit(Literal),
    Ident(String),
    Unary {
        op: UnOp,
        expr: Box<Expr>,
    },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// `func(lens, rel_start, rel_end)` — aggregate `lens` over the window
    /// `[t + rel_start, t + rel_end)` relative to the evaluation timestamp.
    Agg {
        func: AggFunc,
        lens: String,
        rel_start: i64,
        rel_end: i64,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    /// `CREATE DATABASE <name>` — registers a fresh, empty database.
    /// The first database created also becomes the active one.
    CreateDatabase {
        name: String,
    },
    /// `DROP DATABASE <name>` — removes a database and all its lenses.
    /// Clears the active database if it pointed at this one.
    DropDatabase {
        name: String,
    },
    /// `USE DATABASE <name>` — sets the active database for subsequent
    /// lens statements.
    UseDatabase {
        name: String,
    },
    Create {
        name: String,
        ty: Type,
    },
    Append {
        name: String,
        start: i64,
        end: i64,
        value: Literal,
    },
    Derive {
        name: String,
        expr: Expr,
    },
    At {
        name: String,
        t: i64,
    },
    Range {
        name: String,
        start: i64,
        end: i64,
        filter: Option<Expr>,
    },
    Drop {
        name: String,
    },
    /// `REDUCE LENS <name> <start> <end> USING <func>` — collapse a lens over
    /// an absolute range to a single scalar via an aggregation function.
    Reduce {
        name: String,
        start: i64,
        end: i64,
        func: AggFunc,
    },
}

impl Stmt {
    /// True for statements that only observe state (`AT`, `RANGE`).
    ///
    /// The TCP server uses this to route a query through a shared-read lock
    /// instead of an exclusive write lock, allowing concurrent point and
    /// range lookups.
    pub fn is_read_only(&self) -> bool {
        matches!(
            self,
            Stmt::At { .. } | Stmt::Range { .. } | Stmt::Reduce { .. }
        )
    }
}
