# tauctl

`tauctl` is the interactive REPL for tau databases. It reads one statement per line, dispatches it through a command registry, and - if nothing matches and a TCP connection is active - forwards the line to that connection as a tauql statement. Every response is printed verbatim and timed; failures (built-in or server-side `ERR …`) surface as a red `[err 1 …]` footer.

```
τ: connect dev 127.0.0.1:7070
connected to 127.0.0.1:7070 as dev (plain)
[ok in 57µs]
τ: CREATE DATABASE demo
OK
[ok in 41ms]
τ: AT LENS temp 25
VAL f18
[ok in 41ms]
τ: exit
bye.
```

## Built-in commands

| name | what it does |
|---|---|
| `help`                                              | List every registered command. |
| `clear`                                             | Clear the screen (no-op when stdout is not a TTY). |
| `connect <name> <host:port> [tls] [<user> <pass>]`  | Open a connection. Optional `tls` keyword switches to TLS; the trailing pair runs `AUTH` on the new socket immediately. |
| `disconnect <name>`                                 | Close a TCP connection by name. |
| `use <name>`                                        | Switch the active connection. |
| `connections`                                       | List registered connections (active marked with `*`, scheme shown as `tcp`/`tls`). |
| `auth <user> <pass>`                                | Send `AUTH <user> <pass>` on the active connection. |
| `load <lens> <local-path> [chunk]`                  | Read a CSV from the **client's** filesystem and ship it to the active connection as batched `APPEND` statements. Use this when the file lives on your laptop and the server is remote / containerised. |
| `exit` / `quit` / Ctrl-D                            | Close the REPL. |

Anything else is treated as a tauql statement and sent to the active connection.

## Line editing

The REPL uses `rustyline` for input, so the standard readline shortcuts work out of the box:

- ←/→ to move within the line, Ctrl-A / Ctrl-E for home/end, Ctrl-W to delete the previous word
- ↑/↓ to recall earlier statements
- Bracketed paste: multi-line clipboard content is treated as a single line
- History is persisted across sessions to `$HOME/.tau_history` (override with `TAU_HISTORY_FILE`)
- Ctrl-C cancels the current line; Ctrl-D exits

## Client-side bulk load vs server-side `COPY`

There are two paths for ingesting a CSV, picked by where the file lives:

- **Server-side, file already on the server's filesystem (embedded mode or a Docker volume):**
  ```
  τ: COPY LENS temp FROM "/data/temperature.csv"
  ```
  Runs `COPY` as a tauql statement; the server reads the path directly with `std::fs`.

- **Client-side, file lives on your machine and the server is remote / containerised:**
  ```
  τ: load temp examples/data/temperature.csv
  loaded 48 rows into temp (1 chunk)
  ```
  The REPL reads the file from your filesystem, parses each row, and sends batched `APPEND LENS temp s0 e0 v0, s1 e1 v1, …` statements over the existing connection. No server-side path access required, no extra protocol state, works through TLS and auth like any other statement.

`load` chunks at 256 rows per `APPEND` by default; pass an explicit chunk size as the third argument when you want to tune it (`load temp big.csv 1024`).

## TLS

`connect … tls` opens a rustls TLS session. The SNI hostname is derived from the host part of `host:port`. **Certificate verification is disabled** (a no-verify `ServerCertVerifier`) so it works against the tau server's default ephemeral self-signed cert - appropriate for dev/internal traffic, not for untrusted networks.

```
τ: connect prod prod-host:7070 tls
connected to prod-host:7070 as prod (TLS)
[ok in 12ms]
```

## Authentication

When the server runs with `--auth`, the first message on every new connection must be `AUTH <user> <pass>`. Two ways to send it:

```
τ: connect dev 127.0.0.1:7070 admin s3cret      # inline at connect time
τ: connect dev 127.0.0.1:7070
τ: auth admin s3cret                            # or after, via the auth command
```

A failed AUTH surfaces as a red `[err 1 …: authentication failed]` footer.

## Multi-user workflow

The server's CRUDA grants are configured via tauql, so multi-user setup is just a sequence of normal commands sent through tauctl:

```
τ: connect prod prod-host:7070 tls admin s3cret
OK
[ok in 14ms]
τ: CREATE USER alice PASSWORD "p4ss"
OK
τ: GRANT R ON main TO alice
OK
τ: GRANT U ON staging TO alice
OK
τ: SHOW GRANTS alice
GRANTS 1; alice main:R staging:U
τ: GRANT A ON * TO alice                        # promote alice to global admin
OK
τ: SHOW USERS
NAMES 2; admin; alice
```

## Talking to multiple databases

The TCP manager holds a named pool - you can have several connections open and switch between them with `use`.

```
τ: connect prod prod-host:7070 tls admin s3cret
τ: connect stage stage-host:7070 admin s3cret
τ: connections
* prod       tls  prod-host:7070
  stage      tcp  stage-host:7070
τ: use stage
active connection now stage
τ: SHOW DATABASES
NAMES 2; users; events
```

## tauql coverage

The dispatcher is a transparent line forwarder - the server parses and executes the statement, then sends one response line back. **Every statement the executor accepts works through tauctl unchanged.**

### Data DDL

| statement | response | notes |
|---|---|---|
| `CREATE DATABASE <name>`           | `OK`         | First created becomes the active database. |
| `DROP DATABASE <name>`             | `OK`         | Drops the database and all its lenses. |
| `USE DATABASE <name>`              | `OK`         | Sets the active database for subsequent lens statements. |
| `SHOW DATABASES`                   | `NAMES n; …` | Sorted database names; filtered to those the caller has any grant on (admins see all). |
| `CREATE LENS <name> <type>`        | `OK`         | `type` ∈ `int` `float` `str` `bool` `bytes`. |
| `DROP LENS <name>`                 | `OK`         | Removes the lens from the active database. |
| `SHOW LENSES`                      | `NAMES n; …` | Sorted lens names in the active database. |
| `DERIVE LENS <name> AS <expr>`     | `OK`         | Lazy computed lens; see expressions below. |

### Transactions

| statement | response | notes |
|---|---|---|
| `START TRANSACTION` | `OK` | Begin buffering mutations. `ERR transaction already active` if one is already open. |
| `COMMIT`            | `OK` | Apply all buffered mutations atomically. `ERR no active transaction` if none open. |
| `ROLLBACK`          | `OK` | Discard all buffered mutations. `ERR no active transaction` if none open. |

Mutations issued inside a transaction (`APPEND`, `COPY`, `CREATE LENS`, etc.) are held in memory and invisible to other connections until `COMMIT`. `ROLLBACK` drops them entirely. Transactions are per-connection; nesting is not supported.

### Writes

| statement | response | notes |
|---|---|---|
| `APPEND LENS <name> <s> <e> <v>`                          | `OK` | Single tau. |
| `APPEND LENS <name> <s0> <e0> <v0>, <s1> <e1> <v1>, …`    | `OK` | Bulk - one layer, multiple taus. |
| `COPY LENS <name> FROM "<path>"`                          | `OK` | Server-side ingest from CSV (`start,end,value` per line). |

### Reads

| statement | response | notes |
|---|---|---|
| `AT LENS <name> <t>`                          | `VAL <v>` or `VAL NIL` | Point lookup. |
| `RANGE LENS <name> <s> <e>`                   | `RANGE n; <s>:<e>:<v>; …` | All taus that intersect `[s, e)`. |
| `RANGE LENS <name> <s> <e> WHERE <expr>`      | `RANGE n; …`              | Filter expression evaluated per segment. |
| `REDUCE LENS <name> <s> <e> USING <func>`     | `VAL <v>` | `func` ∈ `min` `max` `avg` `sum` `count`. `avg` is time-weighted. |

### Users & permissions (admin only)

| statement | response | notes |
|---|---|---|
| `CREATE USER <name> PASSWORD "<pass>"`  | `OK`            | Hashes with argon2id. |
| `DROP USER <name>`                      | `OK`            | |
| `GRANT <perms> ON <db|*> TO <user>`     | `OK`            | `perms` is any combination of `CRUDA`, or `*` (all), or `-` (none). |
| `REVOKE <perms> ON <db|*> FROM <user>`  | `OK`            | |
| `SHOW USERS`                            | `NAMES n; …`    | |
| `SHOW GRANTS [<user>]`                  | `GRANTS n; …`   | Every user with their grants, or just the named one. |

Non-admin callers receive `ERR permission denied: …` for any statement they lack the bits for.

### Expressions (`DERIVE` and `WHERE`)

Identifiers reference other lenses by name. Supported operators (precedence high → low):

| group       | operators |
|-------------|-----------|
| unary       | `-` `!`   |
| `* / %`     | multiplicative |
| `+ -`       | additive  |
| comparison  | `<` `<=` `>` `>=` |
| equality    | `==` `!=` |
| logical and | `&&`      |
| logical or  | `\|\|`    |
| parens      | `(expr)`  |
| literals    | int, float, string `"…"`, bool, `null` |

Aggregations are first-class expression nodes, available in `DERIVE` and `WHERE`:

```
avg(lens, rel_start, rel_end)
min(lens, rel_start, rel_end)
max(lens, rel_start, rel_end)
sum(lens, rel_start, rel_end)
count(lens, rel_start, rel_end)
```

`rel_start` and `rel_end` are offsets relative to the evaluation timestamp `t`, so `avg(temp, -60, 0)` at `t=100` averages `temp` over `[40, 100)`.

```
τ: DERIVE LENS fahrenheit AS temp * 9.0 / 5.0 + 32.0
OK
τ: DERIVE LENS smooth AS avg(temp, -20, 0)
OK
τ: DERIVE LENS hot AS temp > avg(temp, -300, 0)
OK
```

### Response encoding

| prefix | meaning |
|---|---|
| `OK`                                            | DDL / write succeeded |
| `VAL <codec>` / `VAL NIL`                       | Scalar result from `AT` / `REDUCE` |
| `RANGE n; <s>:<e>:<v>; …`                       | `n` segments from `RANGE` |
| `NAMES n; <name>; …`                            | Name list from `SHOW DATABASES` / `SHOW LENSES` / `SHOW USERS` |
| `GRANTS n; <user> <db>:<perms> …; …`            | Output of `SHOW GRANTS` |
| `ERR <message>`                                 | Parse, executor, or permission error (surfaces as red `[err 1 …]`) |

Values are tag-prefixed: `i<int>`, `f<float>`, `s<percent-escaped>`, `b<0|1>`, `n` (null).

## Adding a command

```rust
registry.register(commands::Command::new(
    "echo",
    "Echo the rest of the line.",
    |_registry, _repl, line| {
        println!("{}", line.trim_start_matches("echo").trim());
        Ok(())
    },
));
```

The action receives `&Registry` (so introspective commands like `help` work) and `&mut Repl` (so commands can read or grow the history, or use `repl.manager` to send TCP/TLS traffic), plus the raw input line. Return `Ok(())` for success or `Err(msg)` to fail - the message lands in the red `[err 1 …]` footer.

## Styling

Colors come from the basic ANSI 8-color foreground palette (`\x1b[3Xm`), so the colors you see - cyan prompt, dim echo, red errors - are whatever your terminal theme defines those colors to be. No RGB is forced. Disabled automatically when `NO_COLOR` is set or stdout is not a TTY.

## Status footer

| outcome | example |
|---|---|
| success         | `[ok in 1.351µs]` |
| client failure  | `[err 1 in 42µs: unknown command: foobar]` |
| server failure  | `[err 1 in 40ms: permission denied: user 'alice' lacks U on main]` |

Time is rendered in µs / ms / s based on magnitude.
