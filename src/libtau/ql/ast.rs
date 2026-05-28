//! Abstract syntax tree for the Tau query language.
//!
//! The grammar is deliberately minimal:
//!
//! ```text
//! stmt   := create | append | copy | derive | show | at | range | reduce | drop | use
//!         | create_user | drop_user | grant | revoke
//!
//! create := CREATE DATABASE <ident>
//!         | CREATE LENS <ident> <type>
//! append := APPEND LENS <ident> <int> <int> <literal> [, <int> <int> <literal> …]
//! copy   := COPY LENS <ident> FROM "<path>"
//! derive := DERIVE LENS <ident> AS <expr>
//! show   := SHOW DATABASES
//!         | SHOW LENSES
//!         | SHOW USERS
//!         | SHOW GRANTS [<ident>]
//! at     := AT     LENS <ident> <int>
//! range  := RANGE  LENS <ident> <int> <int> [WHERE <expr>]
//! reduce := REDUCE LENS <ident> <int> <int> USING <func>
//! drop   := DROP   LENS <ident>
//!         | DROP   DATABASE <ident>
//!         | DROP   USER <ident>
//! use    := USE    DATABASE <ident>
//! start  := START TRANSACTION
//! commit := COMMIT
//! rollback := ROLLBACK
//!
//! create_user := CREATE USER <ident> PASSWORD "<pass>"
//! grant       := GRANT  <perm-letters> ON <db-or-star> TO   <ident>
//! revoke      := REVOKE <perm-letters> ON <db-or-star> FROM <ident>
//!
//! type      := int | float | str | bool | bytes
//! func      := min | max | avg | sum | count
//! literal   := int | float | string | bool | null
//! expr      := disjunction      (full operator-precedence grammar; see parser)
//! perm-letters := any combo of CRUDA, or `*` for all, or `-` for none
//! db-or-star   := <ident> | `*`
//! ```
//!
//! Keywords are case-insensitive; identifiers and literals are not.

use std::sync::Arc;

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
///
/// `Str` uses `Arc<str>` so that cloning a literal (e.g. when an expression
/// is re-evaluated per query) is an atomic reference-count bump rather than a
/// heap allocation + copy.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Int(i64),
    Float(f64),
    Str(Arc<str>),
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
    /// `func(lens, rel_start, rel_end)` - aggregate `lens` over the window
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
    /// `START TRANSACTION` - begin a transaction.  Subsequent statements are
    /// buffered until a `COMMIT` or `ROLLBACK`.  Transactions are not nested;
    /// issuing `START` while a transaction is active is an error.
    StartTransaction,
    /// `COMMIT` - apply all buffered statements atomically.  Requires an active transaction.
    Commit,
    /// `ROLLBACK` - discard all buffered statements.  Requires an active transaction.
    Rollback,
    /// `CREATE DATABASE <name>` - registers a fresh, empty database.
    /// The first database created also becomes the active one.
    CreateDatabase {
        name: String,
    },
    /// `DROP DATABASE <name>` - removes a database and all its lenses.
    /// Clears the active database if it pointed at this one.
    DropDatabase {
        name: String,
    },
    /// `USE DATABASE <name>` - sets the active database for subsequent
    /// lens statements.
    UseDatabase {
        name: String,
    },
    Create {
        name: String,
        ty: Type,
    },
    /// `APPEND LENS <name> <s0> <e0> <v0> [, <s1> <e1> <v1> …]` - write one
    /// or more taus into a single layer.  Bulk form reduces per-write overhead.
    Append {
        name: String,
        taus: Vec<(i64, i64, Literal)>,
    },
    /// `COPY LENS <name> FROM "<path>"` - ingest taus from a CSV file where
    /// each line is `start,end,value`.
    Copy {
        name: String,
        path: String,
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
    /// `SHOW DATABASES` - list all registered database names.
    ShowDatabases,
    /// `SHOW LENSES` - list all lens names in the active database.
    ShowLenses,
    /// `REDUCE LENS <name> <start> <end> USING <func>` - collapse a lens over
    /// an absolute range to a single scalar via an aggregation function.
    Reduce {
        name: String,
        start: i64,
        end: i64,
        func: AggFunc,
    },
    /// `CREATE USER <name> PASSWORD "<pass>"` - add a new user with no
    /// permissions yet.  Requires global admin.
    CreateUser {
        name: String,
        password: String,
    },
    /// `DROP USER <name>` - delete a user.  Requires global admin.
    DropUser {
        name: String,
    },
    /// `GRANT <perms> ON <db|*> TO <user>` - grant per-database permissions.
    /// Requires admin on the target database (or global).
    Grant {
        perms: crate::libtau::users::Perm,
        database: String,
        user: String,
    },
    /// `REVOKE <perms> ON <db|*> FROM <user>` - strip per-database permissions.
    /// Requires admin on the target database (or global).
    Revoke {
        perms: crate::libtau::users::Perm,
        database: String,
        user: String,
    },
    /// `SHOW USERS` - list user names.  Requires global admin.
    ShowUsers,
    /// `SHOW GRANTS [<user>]` - list per-database permissions for a user
    /// (or for every user when no name given).
    ShowGrants {
        user: Option<String>,
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
            Stmt::At { .. }
                | Stmt::Range { .. }
                | Stmt::Reduce { .. }
                | Stmt::ShowDatabases
                | Stmt::ShowLenses
                | Stmt::ShowUsers
                | Stmt::ShowGrants { .. }
        )
    }
}

// Display impls - used to serialise schema DDL statements to the WAL so they
// survive a restart and can be replayed as text.

impl std::fmt::Display for Type {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Type::Int => write!(f, "int"),
            Type::Float => write!(f, "float"),
            Type::Str => write!(f, "str"),
            Type::Bool => write!(f, "bool"),
            Type::Bytes => write!(f, "bytes"),
        }
    }
}

impl std::fmt::Display for Literal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Literal::Int(v) => write!(f, "{}", v),
            Literal::Float(v) => write!(f, "{}", v),
            Literal::Str(s) => write!(f, "\"{}\"", s),
            Literal::Bool(b) => write!(f, "{}", b),
            Literal::Null => write!(f, "null"),
        }
    }
}

impl std::fmt::Display for AggFunc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AggFunc::Min => write!(f, "min"),
            AggFunc::Max => write!(f, "max"),
            AggFunc::Avg => write!(f, "avg"),
            AggFunc::Sum => write!(f, "sum"),
            AggFunc::Count => write!(f, "count"),
        }
    }
}

impl std::fmt::Display for BinOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
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
            BinOp::And => "and",
            BinOp::Or => "or",
        };
        write!(f, "{s}")
    }
}

impl std::fmt::Display for UnOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UnOp::Neg => write!(f, "-"),
            UnOp::Not => write!(f, "not "),
        }
    }
}

impl std::fmt::Display for Expr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Expr::Lit(lit) => write!(f, "{lit}"),
            Expr::Ident(name) => write!(f, "{name}"),
            Expr::Unary { op, expr } => write!(f, "{op}{expr}"),
            // Always parenthesize binary ops for unambiguous round-tripping.
            Expr::Binary { op, lhs, rhs } => write!(f, "({lhs} {op} {rhs})"),
            Expr::Agg {
                func,
                lens,
                rel_start,
                rel_end,
            } => write!(f, "{func}({lens}, {rel_start}, {rel_end})"),
        }
    }
}

impl std::fmt::Display for Stmt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // These two are the only statements replayed via the schema WAL.
            Stmt::Create { name, ty } => write!(f, "CREATE LENS {name} {ty}"),
            Stmt::Derive { name, expr } => write!(f, "DERIVE LENS {name} AS {expr}"),
            other => write!(f, "{other:?}"),
        }
    }
}
