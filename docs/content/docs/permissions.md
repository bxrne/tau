+++
title = "Permissions"
date = 2026-05-30
template = "page.html"
+++

Tau's permission model is a five-bit capability bitmap applied per database. It governs every statement when `--auth` is enabled on the server.

---

## The CRUDA bitmap

| bit | letter | name | controls |
|-----|--------|------|----------|
| 4 | `C` | Create | `CREATE LENS`, `DERIVE LENS` |
| 3 | `R` | Read | `AT`, `RANGE`, `REDUCE`, `SHOW LENSES`, `HISTORY LENS`, `AT AS OF`, `AT LAYER`, `BACKUP DATABASE` |
| 2 | `U` | Update / write | `APPEND`, `COPY`, `BATCH APPEND` |
| 1 | `D` | Delete | `DROP LENS` |
| 0 | `A` | Admin | `CREATE DATABASE`, `DROP DATABASE`, `CREATE USER`, `DROP USER`, `GRANT`, `REVOKE`, `SHOW USERS`, `SHOW GRANTS` (others), `RESTORE DATABASE` |

Effective permission = `grants[db] | grants["*"]`. The wildcard key `"*"` applies to every database, including databases created after the grant.

---

## Permission required per statement

| statement | required permission |
|-----------|-------------------|
| `CREATE DATABASE` | `A` on `*` (global admin) |
| `DROP DATABASE` | `A` on the named database |
| `USE DATABASE` | any grant on the named database |
| `SHOW DATABASES` | any grant (output filtered per-user) |
| `CREATE LENS` | `C` on active database |
| `DROP LENS` | `D` on active database |
| `DERIVE LENS` | `C` on active database |
| `SHOW LENSES` | `R` on active database |
| `APPEND LENS` | `U` on active database |
| `BATCH APPEND LENS` | `U` on active database |
| `COPY LENS` | `U` on active database |
| `AT LENS` | `R` on active database |
| `AT LENS … AS OF` | `R` on active database |
| `AT LENS … LAYER` | `R` on active database |
| `RANGE LENS` | `R` on active database |
| `REDUCE LENS` | `R` on active database |
| `HISTORY LENS` | `R` on active database |
| `BACKUP DATABASE` | `R` on the named database |
| `RESTORE DATABASE` | `A` on `*` (global admin) |
| `START TRANSACTION` | checked at commit time per buffered statement |
| `CREATE USER` | `A` on `*` (global admin) |
| `DROP USER` | `A` on `*` (global admin) |
| `GRANT` | `A` on target database, or global admin |
| `REVOKE` | `A` on target database, or global admin |
| `SHOW USERS` | `A` on `*` (global admin) |
| `SHOW GRANTS` (self) | none — any authenticated user may view their own grants |
| `SHOW GRANTS` (other) | `A` on `*` (global admin) |
| `AUTH` | none — unauthenticated |
| `QUIT` / `EXIT` | none |

---

## Global admin

A user is a **global admin** when they hold `A` on `"*"`:

```
GRANT A ON * TO alice
```

Global admins can perform all operations on all databases, including databases that do not yet exist. The bootstrap user created with `--username` / `--password` on first start is automatically a global admin.

---

## Granting permissions

```
GRANT <perms> ON <db|*> TO <user>
```

`<perms>` is any combination of the five letters (`C`, `R`, `U`, `D`, `A`), the shorthand `*` (all bits), or `-` (no bits). Letters may appear in any order.

```
GRANT RU ON sensors TO alice        ← read + write on sensors
GRANT * ON * TO bob                 ← full access everywhere
GRANT A ON * TO carol               ← promote to global admin
GRANT - ON staging TO dave          ← revoke all bits on staging
```

---

## Revoking permissions

```
REVOKE <perms> ON <db|*> FROM <user>
```

`REVOKE` clears the specified bits. It does not affect grants on other databases.

```
REVOKE U ON sensors FROM alice      ← remove write access on sensors
```

---

## Wildcard grants

A grant on `"*"` is a wildcard. It combines with database-specific grants via bitwise OR:

- `alice` has `R` on `sensors` and `CRU` on `*`
- Effective permission on `sensors` = `R | CRU` = `CRUE` (no delete, no admin)
- Effective permission on any other database = `CRU`

---

## Embedded use bypasses auth

Library consumers calling `Executor::exec` directly never pass through `check_permission`. Auth is a server-side concern, enforced only when using `exec_as` / `exec_read_as` in the TCP server layer.

---

## Example: read-only analyst

```
CREATE USER analyst PASSWORD "hunter2"
GRANT R ON metrics TO analyst
```

`analyst` can execute `AT`, `RANGE`, `REDUCE`, `HISTORY LENS`, `SHOW LENSES`. They cannot `APPEND`, `CREATE LENS`, `DROP LENS`, or access other databases.

## Example: service account with write access

```
CREATE USER ingest PASSWORD "s3cr3t"
GRANT CRU ON * TO ingest
```

`ingest` can create lenses and append data on any database but cannot delete lenses (`D`) or perform admin operations (`A`).
