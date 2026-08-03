# Contributing to zvec-grep

Thank you for helping make zvec-grep better.

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
