# Source review of symbol-probe Gold

Reviewed 2026-11-13. The five questions and reference answers retain the source
facts authored for the diagnostic branch. Candidate MCP output, call counts,
scores, graph resolution and truncation observations are not Gold.

Manual inspection of the pinned source was cross-checked independently of the
candidate with Python's `ast` module. A benchmark maintainer should review these
facts with the PR. Review material stays outside the Harbor task directories.

| Task | Source facts checked |
|---|---|
| requests:cc1 | All 18 production Python files under `src/requests` were scanned. Only `Session.send` calls `resolve_redirects`, at sessions.py:723 and :740. |
| matplotlib:cc1 | `_plot_args` assigns `make_artist = self._make_line` at :504, then invokes `make_artist` at :531–533. Assignment itself is not a call. |
| sympy:cc1 | Four functions in systems.py call `ode_order`: `_get_func_order` :29, `linear_ode_to_matrix` :462, `_strong_component_solver` :1719, and nested `_get_coeffs_from_subs_expression` :1894. |
| matplotlib:cc2 | `_make_line` :334–338 calls `_getdefaults` then `_setdefaults` on :336, followed by `mlines.Line2D` on :337. AST call expressions and Python argument evaluation determine this order. |
| sympy:cc2 | `_get_func_order` :28–29 directly calls project function `ode_order` and built-in `max`; the dictionary/generator comprehensions are syntax, not additional callees. |

Pinned primary sources:

- [Requests sessions.py](https://github.com/psf/requests/blob/46e939b5525d9c72b677340985582b04b128478a/src/requests/sessions.py#L673)
  SHA256 `0a5d5da449ce7f0af3ccf6e4bbe7a67a935e37846dff4ff9f08cb6c7e2464e6f`.
- [Matplotlib _base.py](https://github.com/matplotlib/matplotlib/blob/a5e1f6086e92d5cc0a23f85685d03d85b89a8395/lib/matplotlib/axes/_base.py#L334)
  SHA256 `c01ea6c0d6bdda5b732014b6d19ac85d772fab1a1ad288c3a25b0f57cfe6df7e`.
- [Sympy systems.py](https://github.com/sympy/sympy/blob/3c817ed8ab551d71e36907ce869afe5e4ca789ff/sympy/solvers/ode/systems.py#L28)
  SHA256 `2a893600c3b8604297dfc7e2d8baf12e6e73a5370d5f3dfdf1bece06fad47b35`.

Requests production-file membership came from the untruncated Git tree for its
pinned commit; downloaded blobs were verified against their Git object hashes.
The reference's supplementary test callers lie outside the question's `src/`
scope and are not required in an answer. References are passed only to the judge;
the agent receives the question and repository source.
