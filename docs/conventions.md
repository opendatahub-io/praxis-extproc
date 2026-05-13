# Development Conventions

## Coding Style

### General Principles

- Brevity is a component of quality. Keep code lean and
  complete; no bloat.
- Small, composable, single-purpose functions are the
  default unit of organization. Split code into small
  files with focused responsibilities.
- Minimize side effects. Prefer pure transformations when
  feasible: data in, data out. Resist mutable state when
  feasible and outside the critical paths.
- Keep functions short enough to reason about in isolation.
- Prefer raw performance when reasonable. Reduce memory
  copies where feasible. Use references, borrowing, and
  in-place mutation when it avoids unnecessary cloning.

### Important Tools

- **Clippy**: Enforce idiomatic Rust and catch common
  mistakes
- **rustfmt**: Ensure consistent code formatting
- **cargo-audit**: Check for vulnerable dependencies
- **cargo-deny**: Enforce supply chain safety policies
- **rustdoc**: Generate the API documentation

### Comments vs Tracing

Comments answer **"why?"**, never **"what?"**.

**"What?" belongs in `tracing`**, not comments. If a
comment describes what the code is doing at runtime
("parse the config", "reject the request", "skip this
step"), replace it with a `tracing::debug!`,
`tracing::trace!`, or `tracing::info!` call. Runtime
narration (what the code did, what it decided, what it
skipped) is structured logging, not commentary.

**"Why?" belongs in comments**, but only when
non-obvious. A hidden constraint, a subtle invariant, a
workaround for a specific bug, or behavior that would
surprise a reader: these justify a comment. If removing
the comment would not confuse a future reader, do not
write it.

**"What?" at the code level needs neither.** Well-named
identifiers already explain what the code does. Do not
write comments that restate what names already convey.

### Testing

**New capabilities require all of the following:**

1. Unit tests covering the implementation
2. Integration tests proving end-to-end behavior

This is not optional. A feature without tests is not
complete.

Prefer more doctests when in doubt. Duplicative coverage
between doctests and unit/integration tests is fine.

Prefer assertion messages over inline comments. Put the
explanation in the assertion's message argument so it
prints on failure:

```rust
// Bad:
// ACL should block loopback
assert_eq!(status, 403);

// Good:
assert_eq!(status, 403, "ACL should block loopback");
```

### RFC Conformance

When implementing protocol-level behavior (gRPC,
ExtProc, HTTP semantics), identify the governing specs
and verify conformance against them.

- Cite the specific spec or RFC section in test names
  or doc comments for conformance tests.
- When in doubt about an edge case, the spec is the
  authority, not other implementations.

### Rules, Practices & Lints

Security is enforced at the lint level. See
`[lints]` in `Cargo.toml` for the full set.

- `#![deny(unsafe_code)]` in all crate roots (no
  exceptions; unsafe belongs upstream)
- Clippy runs with `-D warnings` (zero tolerance)
- Errors via `thiserror`
- Logging via `tracing`
- Keep dependencies light. Avoid new dependencies when
  feasible. Only add dependencies with well-established
  reputation.
- Always specify full semver versions with patch
  (e.g. `1.2.3`, not `1.2` or `1`)
- `cargo audit` and `cargo deny check` enforce supply
  chain safety

#### Type Design

Make invalid states unrepresentable. The type system
and serde should enforce constraints at parse time,
not at runtime.

- **Enums over strings for fixed value sets.** Never
  use `String` where the valid values are known. Use
  `#[serde(rename_all = "snake_case")]` enums.
- **Structs over maps for known keys.** Never use
  `BTreeMap`/`HashMap` for config deserialization when
  the key set is known. Use a struct with
  `#[serde(deny_unknown_fields)]`.
- **Enums over multiple `Option<T>` fields.** When
  exactly one of N fields must be set, use an N-variant
  enum.
- **`#[serde(default)]` over `Option<T>` with
  `unwrap_or`.** Use the concrete type with
  `#[serde(default = "fn_name")]` instead.
- **`#[serde(try_from)]` for constrained numerics.**
  Define an enum with `TryFrom` for fixed numeric
  values.
- **`#[serde(deny_unknown_fields)]` by default.** Apply
  to all config structs unless the struct intentionally
  accepts arbitrary keys.

#### Additional Coding Conventions

- Use separator comments to visually separate distinct
  sections of code.
- **No re-export-only files.** If a file exists solely
  to `pub use` items from another crate or module,
  inline the import at the call site instead.
- **Constants** must be at the top of the file (after
  imports), never inside functions or impl blocks.
  Give them their own separator comment
  (e.g. `// Constants`).
- **File ordering**:
  1. Constants (with separator comment)
  2. Public types, impls, and functions
  3. Private types and impls (below their public
     consumers)
  4. Private utility functions (with separator)
  5. `#[cfg(test)] mod tests` block (always last)
- **Field and method ordering**: Alphabetical, with
  `name` pinned first on structs and `new()`/`name()`
  pinned first in impl blocks.
- **Inside `#[cfg(test)] mod tests`**:
  1. Imports
  2. All test functions (`#[test]` / `#[tokio::test]`)
  3. Test utilities at the end (with `// Test Utilities`
     separator)
- Place a blank line between struct/enum fields that
  carry doc comments or attributes. Each documented
  field is a visual paragraph.
- Place a blank line between attribute blocks.
- Separate distinct logical actions with blank lines.
  Function calls, variable bindings that begin a new
  step, and expression blocks that perform a discrete
  operation should have some newline space.
- Prefer pre-computed numeric literals over expressions
  like `1024 * 10`. Always add a trailing comment with
  the human-readable size or meaning (e.g.
  `const MAX_BODY: usize = 10_485_760; // 10 MiB`).

#### Function Size

`clippy.toml` enforces a 30-line function threshold.
Do not suppress the `too_many_lines` lint in production
code except in rare cases where splitting genuinely
harms readability (document why in the `reason`).
Suppression is acceptable in test modules.

Extract focused helper functions instead:

- One logical step per function.
- Name the helper after what it does, not where
  it is called from.
- Prefer many small functions over few large ones.
  The same applies to files: split into many small
  files with focused responsibilities.

#### Separator Comments

All separator comments must be full-width (77 dashes),
never short-form:

```rust
// -----------------------------------------------------------------------------
// Section Name
// -----------------------------------------------------------------------------
```

Never: `// --- Section Name ---`

#### Test Conventions

- Never use inline comments inside test function bodies.
  All explanatory text must be either an assertion message
  or a `tracing::info!` / `debug!` / `trace!` call.
  Bad: `// ACL should block`.
  Good: `assert_eq!(status, 403, "ACL should block")`.
- Do not add doc comments (`///`) or regular comments
  (`//`) on test functions. The function name is the
  documentation.
- Do not add per-test separator comments. Use one
  full-width separator to mark where tests begin.
- Use "Test Utilities" in separator comments, not
  "Helpers". Test utility modules should use doc comments
  that say "test utilities", not "helpers".
- Test utilities must stay inside the `#[cfg(test)]` block
  so they compile only during testing.

#### Idiomatic Rust

- Prefer `to_owned()` over `to_string()` for `&str` to
  `String` conversions. Reserve `to_string()` for Display
  formatting on non-string types (integers, errors, enums).
- Prefer `String::new()` over `"".to_owned()` or
  `"".into()` for empty strings.
- Use inline format args: `format!("{var}")` not
  `format!("{}", var)`.
- Use `is_some_and()` instead of
  `.map(...).unwrap_or(false)`.
- Use let-chains for nested `if let`: prefer
  `if let Some(x) = e && cond { }` over
  `if let Some(x) = e { if cond { } }`.

### Documentation

- All functions, methods, structs, enums, and type aliases
  must have `///` doc comments (public and private).
  Enforced by `missing_docs` and
  `missing_docs_in_private_items` lints.
- Rustdoc **prose** must cover intent, interface, and
  example usage only. Do not explain internal mechanics
  unless they are critical for a caller to use the item
  correctly. If a sentence describes how the function
  works rather than what it does or when to call it,
  remove it.
- Do not over-explain standard patterns (Arc, Cow, early
  returns, option unwrapping) in prose.
- Do not add redundant "Default: X" lines when the default
  is already implied by the trait default or function body.
- Do not document memory efficiency in rustdoc (e.g.
  "avoids allocation", "zero-copy", "cheap clone").
  Correct memory use is expected; it does not need
  narration.
- **Prefer ample doctests.** When in doubt, add one.
  Doctests are valuable; keep them thorough. The
  restriction above is on prose text, not on the quantity
  of doctests.
- Use reference-style rustdoc links, not inline. Put link
  definitions at the bottom of the doc block:

  ```rust
  /// Uses [`Pipeline`] to execute the chain.
  ///
  /// [`Pipeline`]: crate::Pipeline
  ```

### Formatting

- Wrap lines at 80 characters in `.md` files. Code lines
  can be up to 120 characters. Code blocks inside markdown
  follow the 120 char limit.
- Always use the correct syntax highlighter on fenced code
  blocks in `.md` files: `console` for shell, `rust` for
  Rust, `yaml` for YAML, `toml` for TOML, etc. Never use
  bare triple backticks.
- Do not use em-dash or double-hyphen as separators in
  text. Use other punctuation (periods, colons, semicolons,
  parentheses) or restructure sentences instead.

## Code Responsibility

This project does not distinguish between code written by
hand, generated by a tool (e.g. lint), or produced by any
other means. **Every contributor is responsible for the
code they submit**, and *all* code MUST be human reviewed
before submission, or merging.

Before submitting or merging PRs, ensure that you have:

- Read every line of the diff. If you cannot explain why
  something exists, do not submit it.
- Verified that the change does what you intended and
  nothing more.
- Run the test suite *locally* first. The CI pipeline is
  not a substitute for local verification.
