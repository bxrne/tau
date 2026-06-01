//! Abstract syntax tree for the Tau query language.
//!
//! The grammar is deliberately minimal:
//!
//! ```text
//! stmt   := create | append | batch_append | copy | derive | show | at | range
//!         | reduce | drop | use | create_user | drop_user | grant | revoke
//!         | history | backup | restore
//!
//! create := CREATE DATABASE <ident>
//!         | CREATE LENS <ident> <type>
//! append := APPEND LENS <ident> <int> <int> <literal> [, <int> <int> <literal> …]
//! batch_append := BATCH APPEND LENS <ident> { <int> <int> <literal> [; <int> <int> <literal> …] }
//! copy   := COPY LENS <ident> FROM "<path>"
//! derive := DERIVE LENS <ident> AS <expr>
//! show   := SHOW DATABASES
//!         | SHOW LENSES
//!         | SHOW USERS
//!         | SHOW GRANTS [<ident>]
//! at     := AT     LENS <ident> <int>
//!         | AT     LENS <ident> <int> AS OF <int>
//!         | AT     LENS <ident> <int> LAYER <uint>
//! range  := RANGE  LENS <ident> <int> <int> [WHERE <expr>]
//! reduce := REDUCE LENS <ident> <int> <int> USING <func>
//! history := HISTORY LENS <ident> [<int> <int>]
//! drop   := DROP   LENS <ident>
//!         | DROP   DATABASE <ident>
//!         | DROP   USER <ident>
//! use    := USE    DATABASE <ident>
//! start  := START TRANSACTION
//! commit := COMMIT
//! rollback := ROLLBACK
//! backup := BACKUP DATABASE <ident> TO "<path>"
//! restore := RESTORE DATABASE <ident> FROM "<path>"
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
    /// `BATCH APPEND LENS <name> { <s> <e> <v> ; … }` - block-syntax bulk
    /// ingest.  Semantically identical to `Append` but the brace-delimited
    /// block syntax is more ergonomic for large payloads and multi-line input.
    /// All taus are committed as a single layer (one WAL write, one fsync).
    BatchAppend {
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
    /// `AT LENS <name> <t> AS OF <timestamp>` - point query against the state of
    /// the data as it existed at the given wall-clock time (milliseconds since
    /// Unix epoch).  Uses `written_at` timestamps recorded in the WAL.
    AtAsOf {
        name: String,
        t: i64,
        as_of: i64,
    },
    /// `AT LENS <name> <t> LAYER <id>` - low-level audit query against a specific
    /// layer by ID.  Returns the value that layer records at time `t`, or NIL if
    /// the layer does not cover `t`.
    AtLayer {
        name: String,
        t: i64,
        layer_id: u64,
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
        perms: crate::users::Perm,
        database: String,
        user: String,
    },
    /// `REVOKE <perms> ON <db|*> FROM <user>` - strip per-database permissions.
    /// Requires admin on the target database (or global).
    Revoke {
        perms: crate::users::Perm,
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
    /// `HISTORY LENS <name> [<start> <end>]` - list every layer that covers at
    /// least part of the time range `[start, end)`.  Without range arguments,
    /// lists all layers.  Returns layer IDs, write timestamps, and boundary
    /// information — the user-facing audit API.
    HistoryLens {
        name: String,
        range: Option<(i64, i64)>,
    },
    /// `BACKUP DATABASE <name> TO "<path>"` - quiesce the WAL, write a
    /// self-contained snapshot (schema + data) to `path` in WAL wire format,
    /// then resume.  Atomic from the caller's perspective.
    BackupDatabase {
        name: String,
        path: String,
    },
    /// `RESTORE DATABASE <name> FROM "<path>"` - replay a backup snapshot
    /// (written by `BACKUP DATABASE`) into the running server as a new
    /// database named `name`.  WAL consistency checks are applied during
    /// replay.
    RestoreDatabase {
        name: String,
        path: String,
    },
}

impl Stmt {
    /// True for statements that only observe state (`AT`, `RANGE`, etc.).
    ///
    /// The TCP server uses this to route a query through a shared-read lock
    /// instead of an exclusive write lock, allowing concurrent point and
    /// range lookups.
    pub fn is_read_only(&self) -> bool {
        matches!(
            self,
            Stmt::At { .. }
                | Stmt::AtAsOf { .. }
                | Stmt::AtLayer { .. }
                | Stmt::HistoryLens { .. }
                | Stmt::Range { .. }
                | Stmt::Reduce { .. }
                | Stmt::ShowDatabases
                | Stmt::ShowLenses
                | Stmt::ShowUsers
                | Stmt::ShowGrants { .. }
                | Stmt::BackupDatabase { .. }
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

/// Returns `true` for statements that require the global executor write lock
/// because they mutate registry state (`databases`, `active`, `users`, or
/// `pending`).  All other write statements use a per-database write lock and
/// only need the executor read lock.
pub fn needs_registry_lock(stmt: &Stmt) -> bool {
    matches!(
        stmt,
        Stmt::CreateDatabase { .. }
            | Stmt::DropDatabase { .. }
            | Stmt::UseDatabase { .. }
            | Stmt::CreateUser { .. }
            | Stmt::DropUser { .. }
            | Stmt::Grant { .. }
            | Stmt::Revoke { .. }
            | Stmt::StartTransaction
            | Stmt::Commit
            | Stmt::Rollback
            | Stmt::RestoreDatabase { .. }
    )
}

impl std::fmt::Display for Stmt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // These two are the only statements replayed via the schema WAL.
            Stmt::Create { name, ty } => write!(f, "CREATE LENS {name} {ty}"),
            Stmt::Derive { name, expr } => write!(f, "DERIVE LENS {name} AS {expr}"),
            _ => unreachable!("Stmt::Display is only implemented for WAL-persisted DDL variants"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ql::parse;
    use hegel::TestCase;
    use hegel::generators as gs;
    use hegel::generators::Generator;
    use pretty_assertions::assert_eq;

    fn ident_gen() -> impl Generator<String> {
        gs::from_regex("[a-z][a-z0-9_]{0,12}").fullmatch(true)
    }

    // CREATE LENS + DERIVE LENS are the two statements that round-trip through
    // Display (they're persisted in the schema WAL and replayed on startup).
    #[hegel::test]
    fn create_lens_display_roundtrips(tc: TestCase) {
        let name = tc.draw(ident_gen());
        let ty = tc.draw(gs::sampled_from(vec![
            Type::Int,
            Type::Float,
            Type::Str,
            Type::Bool,
        ]));
        let stmt = Stmt::Create {
            name: name.clone(),
            ty: ty.clone(),
        };
        let line = stmt.to_string();
        let (rest, parsed) = parse(&line).expect("Display output must re-parse");
        assert!(rest.trim().is_empty(), "trailing input: {rest:?}");
        assert_eq!(parsed, stmt);
    }

    #[hegel::test]
    fn derive_lens_display_roundtrips(tc: TestCase) {
        let name = tc.draw(ident_gen());
        let src = tc.draw(ident_gen());
        let stmt = Stmt::Derive {
            name: name.clone(),
            expr: Expr::Ident(src.clone()),
        };
        let line = stmt.to_string();
        let (rest, parsed) = parse(&line).expect("Display output must re-parse");
        assert!(rest.trim().is_empty(), "trailing input: {rest:?}");
        assert_eq!(parsed, stmt);
    }

    #[test]
    fn type_display_covers_all_variants() {
        assert_eq!(Type::Int.to_string(), "int");
        assert_eq!(Type::Float.to_string(), "float");
        assert_eq!(Type::Str.to_string(), "str");
        assert_eq!(Type::Bool.to_string(), "bool");
        assert_eq!(Type::Bytes.to_string(), "bytes");
    }

    #[test]
    fn aggfunc_display_covers_all_variants() {
        assert_eq!(AggFunc::Min.to_string(), "min");
        assert_eq!(AggFunc::Max.to_string(), "max");
        assert_eq!(AggFunc::Avg.to_string(), "avg");
        assert_eq!(AggFunc::Sum.to_string(), "sum");
        assert_eq!(AggFunc::Count.to_string(), "count");
    }

    #[test]
    fn literal_display_int_roundtrip() {
        assert_eq!(Literal::Int(42).to_string(), "42");
        assert_eq!(Literal::Bool(true).to_string(), "true");
        assert_eq!(Literal::Null.to_string(), "null");
        assert_eq!(Literal::Str(Arc::from("hi")).to_string(), "\"hi\"");
    }

    #[test]
    fn expr_display_ident() {
        let e = Expr::Ident("temp".into());
        assert_eq!(e.to_string(), "temp");
    }

    #[test]
    fn expr_display_binary() {
        let e = Expr::Binary {
            op: BinOp::Add,
            lhs: Box::new(Expr::Ident("a".into())),
            rhs: Box::new(Expr::Lit(Literal::Int(1))),
        };
        assert_eq!(e.to_string(), "(a + 1)");
    }
}
