# CLI review gallery

Every captured card is listed below. Review annotations are outside the terminal.

- **real PTY**: the CLI command was executed; its exit code is recorded.
- **renderer fixture**: the production formatter rendered sample data; this does not claim a successful business operation.
- **PTY renderer fixture**: a production interactive prompt was invoked with sample data and cancelled, without saving credentials or granting access.
- Protocol-only entry points and shared target variants are explained in the [command coverage table](coverage.md) and [machine-readable inventory](coverage.json).

| Screenshot / card | Command or prompt entry point | Evidence | Exit |
| --- | --- | --- | --- |
| [cli-no-arguments](catalog-help-01.png) | `zg ` | real PTY | 0 |
| [cli-help-alias](catalog-help-02.png) | `zg -h` | real PTY | 0 |
| [cli-version-alias](catalog-help-02.png) | `zg -v` | real PTY | 0 |
| [cli-help](catalog-help-03.png) | `zg --help` | real PTY | 0 |
| [cli-help-search](catalog-help-04.png) | `zg --help search` | real PTY | 0 |
| [cli-help-index](catalog-help-05.png) | `zg --help index` | real PTY | 0 |
| [cli-help-status](catalog-help-06.png) | `zg --help status` | real PTY | 0 |
| [cli-help-config](catalog-help-06.png) | `zg --help config` | real PTY | 0 |
| [cli-help-auth](catalog-help-07.png) | `zg --help auth` | real PTY | 0 |
| [cli-help-server](catalog-help-08.png) | `zg --help server` | real PTY | 0 |
| [cli-help-install](catalog-help-09.png) | `zg --help install` | real PTY | 0 |
| [cli-help-uninstall](catalog-help-10.png) | `zg --help uninstall` | real PTY | 0 |
| [cli-help-help](catalog-help-10.png) | `zg --help help` | real PTY | 0 |
| [cli-help-version](catalog-help-10.png) | `zg --help version` | real PTY | 0 |
| [cli-help-models](catalog-help-11.png) | `zg --help models` | real PTY | 0 |
| [cli-help-file-types](catalog-help-12.png) | `zg --help file-types` | real PTY | 0 |
| [cli-help-environment](catalog-help-13.png) | `zg --help environment` | real PTY | 0 |
| [cli-help-env](catalog-help-14.png) | `zg --help env` | real PTY | 0 |
| [cli-version](catalog-help-15.png) | `zg --version` | real PTY | 0 |
| [cli-bad-argument](catalog-cli-failures-01.png) | `zg --not-a-zg-option` | real PTY | 1 |
| [cli-bad-help-topic](catalog-cli-failures-01.png) | `zg --help not-a-topic` | real PTY | 1 |
| [cli-drop-needs-confirmation](catalog-cli-failures-01.png) | `zg --index --drop` | real PTY | 1 |
| [cli-auth-invalid-scope](catalog-cli-failures-01.png) | `zg --auth grant --capability embedding --scope account` | real PTY | 2 |
| [cli-config-invalid-model](catalog-cli-failures-02.png) | `zg --config model set not-a-model --default` | real PTY | 1 |
| [cli-config-invalid-provider](catalog-cli-failures-02.png) | `zg --config provider set not-a-provider --api-key DEMO_NOT_A_CREDENTIAL` | real PTY | 1 |
| [upgrade-upgraded](catalog-integrations-01.png) | `zg --install --upgrade-installed` | renderer fixture | — |
| [upgrade-preview](catalog-integrations-01.png) | `zg --install --upgrade-installed --dry-run` | renderer fixture | — |
| [upgrade-compatible](catalog-integrations-01.png) | `zg --install --upgrade-installed` | renderer fixture | — |
| [upgrade-manual](catalog-integrations-01.png) | `zg --install --upgrade-installed` | renderer fixture | — |
| [upgrade-failed](catalog-integrations-02.png) | `zg --install --upgrade-installed` | renderer fixture | — |
| [upgrade-empty](catalog-integrations-02.png) | `zg --install --upgrade-installed` | renderer fixture | — |
| [upgrade-redirected](catalog-integrations-02.png) | `zg --install --upgrade-installed > upgrade.log` | renderer fixture | — |
| [cli-uninstall-picker](catalog-integrations-02.png) | `zg --uninstall` | real PTY | 0 |
| [cli-install-line-picker](catalog-integrations-03.png) | `zg --install` | real PTY | 0 |
| [cli-install-success](catalog-integrations-03.png) | `zg --install --target opencode --yes` | real PTY | 0 |
| [cli-uninstall-success](catalog-integrations-03.png) | `zg --uninstall --target opencode --yes` | real PTY | 0 |
| [cli-install-invalid-config](catalog-integrations-03.png) | `zg --install --target opencode --yes` | real PTY | 1 |
| [cli-uninstall-invalid-config](catalog-integrations-04.png) | `zg --uninstall --target opencode --yes` | real PTY | 1 |
| [cli-server-stopped](catalog-server-01.png) | `zg --server status --home /tmp/zg-cli-review/daemon` | real PTY | 0 |
| [cli-server-not-ready](catalog-server-01.png) | `zg --server status --home /tmp/zg-cli-review/daemon --check-ready` | real PTY | 1 |
| [cli-server-started](catalog-server-01.png) | `zg --server on --home /tmp/zg-cli-review/daemon --listen 127.0.0.1:64538` | real PTY | 0 |
| [cli-server-ready](catalog-server-01.png) | `zg --server status --home /tmp/zg-cli-review/daemon --check-ready` | real PTY | 0 |
| [cli-server-stopped-after-run](catalog-server-02.png) | `zg --server off --home /tmp/zg-cli-review/daemon` | real PTY | 0 |
| [cli-server-stop-verified](catalog-server-02.png) | `zg --server status --home /tmp/zg-cli-review/daemon --check-ready` | real PTY | 1 |
| [query-positional](catalog-query-01.png) | `zg 'refresh index'` | renderer fixture | — |
| [query-hybrid](catalog-query-01.png) | `zg --hybrid 'refresh index'` | renderer fixture | — |
| [query-fts](catalog-query-02.png) | `zg --fts 'refresh index'` | renderer fixture | — |
| [query-vector](catalog-query-02.png) | `zg --vector 'refresh index'` | renderer fixture | — |
| [query-groups](catalog-query-03.png) | `zg --hybrid 'refresh index' --hybrid 'update progress'` | renderer fixture | — |
| [query-fuse](catalog-query-03.png) | `zg --hybrid 'refresh index' --fts progress --fuse` | renderer fixture | — |
| [query-empty](catalog-query-03.png) | `zg --fts 'missing symbol'` | renderer fixture | — |
| [query-no-files](catalog-query-04.png) | `zg --fts 'refresh index'` | renderer fixture | — |
| [preview-none](catalog-query-04.png) | `zg --fuse --preview none 'refresh index'` | renderer fixture | — |
| [preview-short](catalog-query-04.png) | `zg --fuse --preview short 'refresh index'` | renderer fixture | — |
| [preview-full](catalog-query-04.png) | `zg --fuse --preview full 'refresh index'` | renderer fixture | — |
| [preview-detail](catalog-query-05.png) | `zg --fuse --preview detail 'refresh index'` | renderer fixture | — |
| [query-trace](catalog-query-05.png) | `zg --fuse --trace --preview none 'refresh index'` | renderer fixture | — |
| [lookup-files](catalog-paths-01.png) | `zg index.rs` | renderer fixture | — |
| [lookup-files-truncated](catalog-paths-01.png) | `zg --limit 1 index.rs` | renderer fixture | — |
| [lookup-text](catalog-paths-01.png) | `zg src/index.rs` | renderer fixture | — |
| [lookup-text-detail](catalog-paths-01.png) | `zg --preview detail src/index.rs` | renderer fixture | — |
| [lookup-text-truncated](catalog-paths-02.png) | `zg --limit 1 src/index.rs` | renderer fixture | — |
| [lookup-empty](catalog-paths-02.png) | `zg src/missing.rs` | renderer fixture | — |
| [lookup-no-files](catalog-paths-02.png) | `zg src/missing.rs` | renderer fixture | — |
| [rg-matches](catalog-paths-02.png) | `zg --rg -n progress src/progress.rs` | renderer fixture | — |
| [rg-empty](catalog-paths-03.png) | `zg --rg missing_symbol src/progress.rs` | renderer fixture | — |
| [files-newline](catalog-paths-03.png) | `zg --rg --files` | renderer fixture | — |
| [files-nul](catalog-paths-03.png) | `zg --rg --files -0` | renderer fixture | — |
| [files-empty](catalog-paths-03.png) | `zg --rg --files -g '*.missing'` | renderer fixture | — |
| [error](catalog-errors-01.png) | `zg --index .` | renderer fixture | — |
| [error-invalid-argument](catalog-errors-01.png) | `zg --index --endpoint invalid` | renderer fixture | — |
| [error-not-found](catalog-errors-01.png) | `zg --hybrid search --mode direct` | renderer fixture | — |
| [error-unsupported](catalog-errors-01.png) | `zg --hybrid search` | renderer fixture | — |
| [error-resource-busy](catalog-errors-02.png) | `zg --index .` | renderer fixture | — |
| [error-resource-closed](catalog-errors-02.png) | `EngineError::resource_closed fixture` | renderer fixture | — |
| [error-internal](catalog-errors-02.png) | `EngineError::internal fixture` | renderer fixture | — |
| [error-storage-permission](catalog-errors-02.png) | `zg --index .` | renderer fixture | — |
| [error-storage-disk](catalog-errors-03.png) | `zg --index .` | renderer fixture | — |
| [error-remote-authorization](catalog-errors-03.png) | `zg --index --embedding qwen/qwen3.7-text-embedding` | renderer fixture | — |
| [error-remote-authentication](catalog-errors-03.png) | `zg --hybrid search` | renderer fixture | — |
| [error-remote-network](catalog-errors-03.png) | `zg --hybrid search` | renderer fixture | — |
| [error-remote-timeout](catalog-errors-04.png) | `zg --hybrid search` | renderer fixture | — |
| [error-debug](catalog-errors-04.png) | `zg --index . --debug` | renderer fixture | — |
| [error-no-color](catalog-errors-04.png) | `zg --index . --no-color` | renderer fixture | — |
| [error-redirected](catalog-errors-04.png) | `zg --index . 2>error.log` | renderer fixture | — |
| [cancelled](catalog-errors-05.png) | `zg --hybrid search  [Esc during index creation]` | renderer fixture | — |
| [status](catalog-status-01.png) | `zg --status` | renderer fixture | — |
| [status-ready](catalog-status-01.png) | `zg --status` | renderer fixture | — |
| [status-stale](catalog-status-01.png) | `zg --status` | renderer fixture | — |
| [status-failed](catalog-status-02.png) | `zg --status` | renderer fixture | — |
| [status-disabled](catalog-status-02.png) | `zg --status` | renderer fixture | — |
| [status-missing](catalog-status-02.png) | `zg --status` | renderer fixture | — |
| [status-uninitialized](catalog-status-02.png) | `zg --status` | renderer fixture | — |
| [status-unknown](catalog-status-03.png) | `zg --status` | renderer fixture | — |
| [status-rebuild-required](catalog-status-03.png) | `zg --status` | renderer fixture | — |
| [indexing](catalog-index-01.png) | `zg --index .` | renderer fixture | — |
| [optimizing](catalog-index-01.png) | `zg --index .` | renderer fixture | — |
| [saving](catalog-index-01.png) | `zg --index .` | renderer fixture | — |
| [finishing](catalog-index-01.png) | `zg --index .` | renderer fixture | — |
| [ready](catalog-index-02.png) | `zg --index .` | renderer fixture | — |
| [partial](catalog-index-02.png) | `zg --index .` | renderer fixture | — |
| [drop](catalog-index-02.png) | `zg --index --drop --yes` | renderer fixture | — |
| [refresh](catalog-index-02.png) | `zg --refresh background 'index progress'` | renderer fixture | — |
| [refresh-off](catalog-index-03.png) | `zg --refresh off 'index progress'` | renderer fixture | — |
| [index-empty](catalog-index-03.png) | `zg --index .` | renderer fixture | — |
| [index-up-to-date](catalog-index-03.png) | `zg --index .` | renderer fixture | — |
| [index-failed](catalog-index-03.png) | `zg --index .` | renderer fixture | — |
| [index-rebuild](catalog-index-04.png) | `zg --index . --rebuild` | renderer fixture | — |
| [index-no-color](catalog-index-04.png) | `zg --index . --no-color` | renderer fixture | — |
| [cli-index-not-ready](catalog-index-04.png) | `zg --status --mode direct --check-ready` | real PTY | 1 |
| [config](catalog-configuration-01.png) | `zg --config model set local/potion-code-16m-v2 --default` | renderer fixture | — |
| [auth](catalog-configuration-01.png) | `zg --auth status` | renderer fixture | — |
| [auth-grant](catalog-configuration-01.png) | `zg --auth grant --capability embedding --scope workspace --embedding qwen/qwen3.7-text-embedding` | renderer fixture | — |
| [auth-status-authorized](catalog-configuration-01.png) | `zg --auth status` | renderer fixture | — |
| [auth-status-empty](catalog-configuration-02.png) | `zg --auth status` | renderer fixture | — |
| [auth-revoke-empty](catalog-configuration-02.png) | `zg --auth revoke` | renderer fixture | — |
| [auth-revoke](catalog-configuration-02.png) | `zg --auth revoke` | renderer fixture | — |
| [auth-revoke-removed](catalog-configuration-02.png) | `zg --auth revoke` | renderer fixture | — |
| [config-provider](catalog-configuration-03.png) | `zg --config provider set qwen --api-key <redacted>` | renderer fixture | — |
| [config-model](catalog-configuration-03.png) | `zg --config model set local/potion-code-16m-v2 --default` | renderer fixture | — |
| [cli-auth-status-empty](catalog-configuration-03.png) | `zg --auth status` | real PTY | 0 |
| [cli-auth-revoke-empty](catalog-configuration-03.png) | `zg --auth revoke` | real PTY | 0 |
| [consent](catalog-prompts-01.png) | `zg --index --embedding qwen/qwen3.7-text-embedding` | renderer fixture | — |
| [consent-query](catalog-prompts-01.png) | `zg --hybrid 'index progress'` | renderer fixture | — |
| [model-picker](catalog-prompts-01.png) | `zg --mode direct 'find authentication flow'` | real PTY | — |
| [install-picker](catalog-prompts-02.png) | `zg --install` | real PTY | — |
| [credential-empty](catalog-prompts-02.png) | `interactive_presentation --mode credential` | PTY renderer fixture | 130 |
| [credential-validation](catalog-prompts-02.png) | `interactive_presentation --mode credential` | PTY renderer fixture | 130 |
| [credential-masked](catalog-prompts-03.png) | `interactive_presentation --mode credential` | PTY renderer fixture | 130 |
| [authorization-once](catalog-prompts-03.png) | `interactive_presentation --mode authorization` | PTY renderer fixture | 130 |
| [authorization-workspace](catalog-prompts-03.png) | `interactive_presentation --mode authorization` | PTY renderer fixture | 130 |
