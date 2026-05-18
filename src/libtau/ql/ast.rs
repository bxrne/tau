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
}

/// Aggregation reducers for `REDUCE LENS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggFn {
    Sum,
    Avg,
    Min,
    Max,
    Count,
}

impl AggFn {
    pub fn as_str(&self) -> &'static str {
        match self {
            AggFn::Sum => "sum",
            AggFn::Avg => "avg",
            AggFn::Min => "min",
            AggFn::Max => "max",
            AggFn::Count => "count",
        }
    }
}

impl Type {
    /// String form used in WAL serialisation; parses back via the type
    /// parser.
    pub fn as_str(&self) -> &'static str {
        match self {
            Type::Int => "int",
            Type::Float => "float",
            Type::Str => "str",
            Type::Bool => "bool",
            Type::Bytes => "bytes",
        }
    }
}

impl Expr {
    /// Render an expression so the parser round-trips it.  Binary operators
    /// are always parenthesised to avoid relying on precedence.
    pub fn to_query_string(&self) -> String {
        match self {
            Expr::Lit(l) => match l {
                Literal::Int(i) => i.to_string(),
                Literal::Float(f) => {
                    // Always include a decimal point so the literal parses
                    // as a float (the parser requires `<digits>.<digits>`).
                    let s = format!("{f}");
                    if s.contains('.') { s } else { format!("{s}.0") }
                }
                Literal::Str(s) => format!("\"{s}\""),
                Literal::Bool(b) => {
                    if *b {
                        "true".into()
                    } else {
                        "false".into()
                    }
                }
                Literal::Null => "null".into(),
            },
            Expr::Ident(n) => n.clone(),
            Expr::Unary { op, expr } => match op {
                UnOp::Neg => format!("(-{})", expr.to_query_string()),
                UnOp::Not => format!("(!{})", expr.to_query_string()),
            },
            Expr::Binary { op, lhs, rhs } => {
                let op_s = match op {
                    BinOp::Add => "+",
                    BinOp::Sub => "-",
                    BinOp::Mul => "*",
                    BinOp::Div => "/",
                    BinOp::Mod => "%",
                    BinOp::Eq => "==",
                    BinOp::NotEq => "!=",
                    BinOp::Lt => "<",
                    BinOp::LtEq => "<=",
                    BinOp::Gt => ">",
                    BinOp::GtEq => ">=",
                    BinOp::And => "&&",
                    BinOp::Or => "||",
                };
                format!(
                    "({} {} {})",
                    lhs.to_query_string(),
                    op_s,
                    rhs.to_query_string()
                )
            }
        }
    }
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
    /// `COMPACT LENS <name>` — merge every layer of the lens into one
    /// canonical newest-wins layer.  No-op if the lens has fewer than two
    /// layers.
    Compact {
        name: String,
    },
    /// `REDUCE LENS <name> <start> <end> <fn>` — fold values across the
    /// half-open interval `[start, end)` using a built-in aggregator.
    Reduce {
        name: String,
        start: i64,
        end: i64,
        func: AggFn,
    },
}

impl Stmt {
    /// True for statements that only observe state (`AT`, `RANGE`, `REDUCE`).
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

    /// Serialise a DDL statement for the WAL.  Returns `None` for read-only
    /// queries and `Append` (which uses the dedicated data-entry path).
    pub fn to_wal_string(&self) -> Option<String> {
        match self {
            Stmt::CreateDatabase { name } => Some(format!("CREATE DATABASE {name}")),
            Stmt::DropDatabase { name } => Some(format!("DROP DATABASE {name}")),
            Stmt::UseDatabase { name } => Some(format!("USE DATABASE {name}")),
            Stmt::Create { name, ty } => Some(format!("CREATE LENS {name} {}", ty.as_str())),
            Stmt::Drop { name } => Some(format!("DROP LENS {name}")),
            Stmt::Derive { name, expr } => {
                Some(format!("DERIVE LENS {name} AS {}", expr.to_query_string()))
            }
            Stmt::Compact { name } => Some(format!("COMPACT LENS {name}")),
            // Read-only / data-path statements are not persisted as DDL.
            Stmt::Append { .. } | Stmt::At { .. } | Stmt::Range { .. } | Stmt::Reduce { .. } => {
                None
            }
        }
    }
}
