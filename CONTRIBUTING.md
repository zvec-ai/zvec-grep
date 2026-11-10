# Contributing to zvec-grep

Thank you for helping make zvec-grep better.

The native CLI and engine live in `rust/`. Start with the
[Rust contributor guide](rust/CONTRIBUTING.md) for development and checks, and
[architecture](rust/ARCHITECTURE.md) for API and ownership boundaries.

User documentation is maintained in
[zvec-web](https://github.com/zvec-ai/zvec-web/tree/main/content/docs) and published
on [zvec.org](https://zvec.org/en/docs/zvec-grep/). Update both English and Chinese
guides when changing the user-facing interface. This repository keeps concise
READMEs, contributor notes, and benchmark methodology.

The root TypeScript implementation remains for legacy development and
compatibility checks. The two implementations have separate CI workflows,
selected by changed paths:

- Changes under `rust/` or to `.github/workflows/rust-ci.yml` run Rust CI only.
- Other changes run the TypeScript CI only.
- Changes to shared `.github/actions/` or the root `.gitignore`, or changes
  affecting both implementations, run both workflows.

Markdown-only changes skip both workflows. The TypeScript CI also skips changes
limited to `LICENSE`, `.github/assets/`, or `.github/scripts/readme-demo.py`.
These filters apply to pull requests and pushes to each workflow's configured
branches; manual runs always run the selected workflow's full set of checks.

## Rust development setup

The workspace pins its Rust toolchain. See the
[native build prerequisites](rust/README.md#native-and-cuda-builds) before building.

```bash
git clone https://github.com/zvec-ai/zvec-grep.git
cd zvec-grep/rust
bash scripts/check.sh
```

## Legacy TypeScript development

Use Node.js 22 or newer for the root TypeScript development workflow:

```bash
# From the repository root
npm ci
npm run build
npm run check
```

## Before opening a pull request

- Keep the change focused.
- Add or update tests when behavior changes.
- Update the documentation when the user-facing interface changes.
- Run the checks for the implementation you changed: `bash scripts/check.sh`
  from `rust/`, or `npm run check` from the repository root for TypeScript.
- Check README and website links when moving or removing documentation.

Pull request titles should follow [Conventional Commits](https://www.conventionalcommits.org/), for example `fix: refresh stale indexes before search`.

For a larger change, open an [issue](https://github.com/zvec-ai/zvec-grep/issues) first so the approach can be discussed.
