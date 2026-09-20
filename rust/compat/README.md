# Compatibility fixtures

This directory is the machine-readable contract between the TypeScript oracle
and the Rust rewrite. It contains data, not Rust test implementations.

Rules:

1. Capture an oracle case before changing Rust behavior.
2. Use repository-relative fixture roots and stable placeholders; never record a
   developer's absolute home path, token, API key, timing or random ID.
3. Normalize path separators, volatile IDs and timings in the differential
   runner rather than editing expected output after each run.
4. Every accepted Direct/Server or TypeScript/Rust difference must reference an
   ID in `allowed-differences.toml` or `mode-differences.toml`.
5. Rust tests load fixtures through `zg-testkit`; adapter white-box tests do not
   redefine compatibility semantics.

Layout grows only when real fixtures are captured:

```text
compat/
  cli/                    # argv/stdout/stderr/exit-code cases
  schema/                 # versioned fixture schemas
  mode-differences.toml   # allowed Direct/Server differences
  allowed-differences.toml
```

Future `mcp/`, `index/` and `models/` directories are added with their first
fixture rather than as empty placeholders.
