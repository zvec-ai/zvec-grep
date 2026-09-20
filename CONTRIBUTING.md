# Contributing to zvec-grep

Thank you for helping make zvec-grep better.

This guide covers the TypeScript / Node.js implementation at the repository root.
For the Rust workspace in `rust/`, see the [Rust contributor guide](rust/CONTRIBUTING.md).
The two implementations have separate CI workflows; changes affecting both should
run both sets of checks.

## Development setup

zvec-grep requires Node.js 22 or newer.

```bash
git clone https://github.com/zvec-ai/zvec-grep.git
cd zvec-grep
npm ci
npm run build
```

## Before opening a pull request

- Keep the change focused.
- Add or update tests when behavior changes.
- Update the documentation when the user-facing interface changes.
- Run the complete check locally:

```bash
npm run check
```

Pull request titles should follow [Conventional Commits](https://www.conventionalcommits.org/), for example `fix: refresh stale indexes before search`.

For a larger change, open an [issue](https://github.com/zvec-ai/zvec-grep/issues) first so the approach can be discussed.
