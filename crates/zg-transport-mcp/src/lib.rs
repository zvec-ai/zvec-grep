//! MCP transport adapter for the public agent and full toolsets.
//!
//! This crate owns MCP schemas and formatting only. It translates tool input
//! into typed requests executed by the engine or the resident workspace provider.

use std::{
    fmt::{self, Write as _},
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use chrono::{DateTime, Local, NaiveDate, TimeZone};
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, tool::ToolCallContext, wrapper::Parameters},
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerInfo,
    },
    service::RequestContext,
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use zg_cli::parse_managed_rg_args;
use zg_engine::{
    EngineError, ErrorReport, ErrorSite, ZvecGrep,
    api::{
        context::{
            ContextOptions, ContextResult,
            options::{ContextRoute, ContextRouteMode, RefreshPolicy, SymbolType},
            result::{ContentRange, ContextItem, ContextItemStatus, MatchedBy},
        },
        index::{
            IndexOptions,
            options::{
                Device, DiscoveryOptions as IndexDiscoveryOptions, EmbeddingModelSpec, RootPath,
            },
        },
        info::{
            InfoOptions, InfoResult,
            result::{InfoSource, WorkspaceIndexPolicy},
        },
    },
};

pub const AGENT_TOOL_NAME: &str = "zvec_grep_search";
pub const FULL_TOOL_NAMES: [&str; 6] = [
    "zvec_grep_index",
    "zvec_grep_index_drop",
    "zvec_grep_index_status",
    "zvec_grep_rg",
    "zvec_grep_search",
    "zvec_grep_server_status",
];

pub const AGENT_INSTRUCTIONS: &str = concat!(
    "Use zvec-grep with these workspace retrieval rules:\n",
    "- Use the current workspace as the evidence source when the user asks about local material, prior context establishes it as relevant, or the question concerns how the current project works—even if the workspace is not mentioned explicitly.\n",
    "- A workspace may contain any mix of code, documents, configuration, and data.\n",
    "- Do not use workspace retrieval for unrelated open-world questions, current external facts, or web content that does not depend on local evidence.\n",
    "- Use native Grep or rg first only when exact lookup alone is sufficient, such as locating one definition, literal, filename, configuration key, error message, regex match, or exhaustive occurrence list.\n",
    "- Use zvec_grep_search first when wording or location is unknown, or when the answer requires architecture, lifecycle, call relationships, dependencies, data or control flow, design rationale, comparison, or synthesis across files or components.\n",
    "- When user-provided or verified exact symbols are present but the answer spans multiple files, components, stages, implementations, or relationships, treat the task as mixed: call zvec_grep_search with the semantic intent and those anchors, then use Read, Grep, or rg for focused verification.\n",
    "- For a semantic or mixed workspace task, start discovery with focused zvec_grep_search before broad file discovery.\n",
    "- Preserve the question's concepts, relationships, and constraints from the user request and established context in semantic queries. Treat inferred names as supplemental hypotheses, not replacements for or constraints on the stated intent.\n",
    "- `query` creates one primary hybrid FTS-plus-vector group; `queries` creates one or more primary hybrid groups; `fts` and `vector` add supplemental lexical-only or semantic-only route groups. These are retrieval routes, not hard constraints. Without `fuse`, the response is one deduplicated and reranked list with query-group metadata; set `fuse: true` to collapse every group into one ranked search plan.\n",
    "- For a fused mixed search, use arguments such as {\"root\":\"/absolute/workspace\",\"query\":\"how are results ranked and fused\",\"fts\":[\"RRF\",\"score\"],\"fuse\":true}.\n",
    "- Search results include bounded source snippets. Treat a sufficient snippet as already-read evidence, and open only the cited file or range when a required detail falls outside it.\n",
    "- If semantic retrieval remains irrelevant, fall back to native Grep or rg.\n",
    "- Stop searching once the available evidence is sufficient for the requested task. Continue only to resolve a material gap or ambiguity; do not repeat similar searches or broaden the investigation merely to reconfirm what is already established.\n",
    "- Do not launch a sub-agent solely to locate workspace material.\n",
    "- Every workspace operation requires an absolute root path visible to the daemon.\n",
    "- Read freshness and background_refresh directly from zvec_grep_search responses without a status preflight.\n",
    "- When results are served_from_current_index, use them immediately when they are sufficient; do not perform extra diagnostics merely because a background refresh is active.\n",
    "- When an index is missing and literal or regex search can answer the task, use native Grep or rg. Creating or rebuilding a persistent index requires explicit user authorization.",
);

pub const FULL_INSTRUCTIONS: &str = concat!(
    "Use zvec-grep with these workspace retrieval and lifecycle rules:\n",
    "- Use zvec_grep_rg first only when exact lookup alone is sufficient, such as locating one definition, literal, filename, configuration key, error message, regex match, or exhaustive occurrence list.\n",
    "- Use zvec_grep_search first when wording or location is unknown, or when the answer requires architecture, lifecycle, call relationships, dependencies, data or control flow, design rationale, comparison, or synthesis across files or components.\n",
    "- For mixed tasks, call zvec_grep_search with the semantic intent and verified exact anchors, then use Read or zvec_grep_rg for focused verification.\n",
    "- Every workspace operation requires an absolute root path visible to the daemon.\n",
    "- Read freshness and background_refresh from zvec_grep_search without a status preflight. Call zvec_grep_index_status only for a missing index, failed or cancelled indexing, diagnostics, or explicit progress monitoring.\n",
    "- Call zvec_grep_index only when persistent indexing or index deletion is explicitly requested. Never silently create, rebuild, or drop an index.\n",
    "- For a new index, use a user-selected embedding or omit it only when a server default model is known; never guess a model.\n",
    "- zvec_grep_index wait defaults to false. Poll zvec_grep_index_status for background progress and set wait to true only when completion is required before continuing.\n",
    "- Use zvec_grep_index with drop: true, or zvec_grep_index_drop, only when index deletion is explicitly requested.\n",
    "- Call zvec_grep_server_status only for daemon diagnostics, not before ordinary searches.\n",
    "- Stop searching once the available evidence is sufficient.\n",
);

const MAX_QUERY_GROUPS: usize = 32;
const MAX_QUERY_CHARS: usize = 4_000;
const MAX_PATH_FILTERS: usize = 128;
const MAX_PATH_CHARS: usize = 1_024;
const MAX_SEARCH_LIMIT: usize = 50;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpToolset {
    #[default]
    Agent,
    Full,
}

impl fmt::Display for McpToolset {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Agent => "agent",
            Self::Full => "full",
        })
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ServerStatusSnapshot {
    pub version: String,
    pub uptime_ms: u64,
    pub shutting_down: bool,
    pub active_runtimes: usize,
    pub queued_jobs: usize,
    pub running_jobs: usize,
    pub loaded_models: usize,
    pub active_model_leases: usize,
}

pub trait ServerStatusProvider: Send + Sync {
    fn snapshot(&self) -> ServerStatusSnapshot;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IndexOperationState {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexOperationError {
    pub report: ErrorReport,
    pub retryable: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexOperationResult {
    pub root: PathBuf,
    pub job_id: String,
    pub state: IndexOperationState,
    pub reused: bool,
    pub error: Option<IndexOperationError>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexRuntimeSnapshot {
    pub watcher_active: bool,
    pub dirty_revision: u64,
    pub indexed_revision: u64,
    pub active_job_id: Option<String>,
    pub job_state: Option<IndexOperationState>,
    pub progress: Option<zg_engine::api::index::progress::IndexProgress>,
    pub error: Option<IndexOperationError>,
}

#[async_trait]
pub trait IndexOperationProvider: Send + Sync {
    async fn submit_index(
        &self,
        options: IndexOptions,
        wait: bool,
    ) -> Result<IndexOperationResult, EngineError>;

    async fn drop_index(&self, options: InfoOptions) -> Result<bool, EngineError>;

    async fn search(
        &self,
        engine: &ZvecGrep,
        request: ContextOptions,
    ) -> Result<ContextResult, EngineError> {
        engine.context(request).await
    }

    fn runtime_snapshot(&self, _root: &Path) -> Option<IndexRuntimeSnapshot> {
        None
    }
}

struct DirectIndexOperationProvider {
    engine: Arc<ZvecGrep>,
}

#[async_trait]
impl IndexOperationProvider for DirectIndexOperationProvider {
    async fn submit_index(
        &self,
        options: IndexOptions,
        _wait: bool,
    ) -> Result<IndexOperationResult, EngineError> {
        let root = request_root(options.root.as_deref());
        let result = self.engine.index(options).await?;
        Ok(IndexOperationResult {
            root,
            job_id: format!("generation-{}", result.generation),
            state: IndexOperationState::Succeeded,
            reused: false,
            error: None,
        })
    }

    async fn drop_index(&self, options: InfoOptions) -> Result<bool, EngineError> {
        self.engine.drop_index(options).await
    }
}

#[derive(Clone)]
pub struct ZvecGrepMcpServer {
    engine: Arc<ZvecGrep>,
    index_operations: Arc<dyn IndexOperationProvider>,
    status: Option<Arc<dyn ServerStatusProvider>>,
    toolset: McpToolset,
    router: ToolRouter<Self>,
}

impl ZvecGrepMcpServer {
    #[must_use]
    pub fn agent(engine: Arc<ZvecGrep>) -> Self {
        Self::build(engine, McpToolset::Agent, None)
    }

    #[must_use]
    pub fn full(engine: Arc<ZvecGrep>, status: Arc<dyn ServerStatusProvider>) -> Self {
        Self::build(engine, McpToolset::Full, Some(status))
    }

    #[must_use]
    pub fn full_with_index_operations(
        engine: Arc<ZvecGrep>,
        status: Arc<dyn ServerStatusProvider>,
        index_operations: Arc<dyn IndexOperationProvider>,
    ) -> Self {
        Self::build_with_index_operations(engine, McpToolset::Full, Some(status), index_operations)
    }

    fn build(
        engine: Arc<ZvecGrep>,
        toolset: McpToolset,
        status: Option<Arc<dyn ServerStatusProvider>>,
    ) -> Self {
        let index_operations: Arc<dyn IndexOperationProvider> =
            Arc::new(DirectIndexOperationProvider {
                engine: Arc::clone(&engine),
            });
        Self::build_with_index_operations(engine, toolset, status, index_operations)
    }

    fn build_with_index_operations(
        engine: Arc<ZvecGrep>,
        toolset: McpToolset,
        status: Option<Arc<dyn ServerStatusProvider>>,
        index_operations: Arc<dyn IndexOperationProvider>,
    ) -> Self {
        let mut router = Self::tool_router();
        if toolset == McpToolset::Agent {
            for name in FULL_TOOL_NAMES {
                if name != AGENT_TOOL_NAME {
                    router.disable_route(name);
                }
            }
        }
        Self {
            engine,
            index_operations,
            status,
            toolset,
            router,
        }
    }

    #[must_use]
    pub fn listed_tools(&self) -> Vec<rmcp::model::Tool> {
        self.router.list_all()
    }
}

#[tool_router]
impl ZvecGrepMcpServer {
    #[tool(
        name = "zvec_grep_search",
        description = "Search an existing workspace index for semantic, relational, cross-file, or multi-hop evidence such as architecture, call chains, dependencies, lifecycle, data or control flow, design rationale, and comparisons. Use it when exact lookup alone cannot answer a workspace-grounded question. Results include bounded source snippets and query-group metadata; treat sufficient snippets as already-read evidence. Use native Grep or rg instead when exact lookup alone is sufficient. Read freshness and background_refresh from the response without a status preflight; when results are served_from_current_index, use them if sufficient.",
        annotations(
            title = "Search with zvec-grep",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn zvec_grep_search(
        &self,
        Parameters(input): Parameters<SearchInput>,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let request = input
            .into_request()
            .map_err(|message| ErrorData::invalid_params(message, None))?;
        let result = self.index_operations.search(&self.engine, request).await;

        Ok(match result {
            Ok(reply) => context_result_to_tool_result(&reply),
            Err(error) => error_result(&error),
        })
    }

    #[tool(
        name = "zvec_grep_index",
        description = "Activate an absolute workspace root to create, incrementally update, rebuild, or explicitly drop its index. Do not call this tool to create, rebuild, or drop an index unless the user requested persistent indexing or index deletion.",
        output_schema = rmcp::handler::server::tool::schema_for_type::<IndexOutput>(),
        annotations(
            title = "Ensure or drop zvec-grep index",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn zvec_grep_index(
        &self,
        Parameters(input): Parameters<IndexInput>,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let request = input
            .into_request()
            .map_err(|message| ErrorData::invalid_params(message, None))?;
        Ok(match request {
            IndexToolRequest::Index { options, wait } => {
                match self.index_operations.submit_index(*options, wait).await {
                    Ok(reply) => index_operation_to_result(&reply),
                    Err(error) => error_result(&error),
                }
            }
            IndexToolRequest::Drop(request) => {
                let root = request_root(request.root.as_deref());
                match self.index_operations.drop_index(request).await {
                    Ok(removed) => drop_result_to_index_result(&root, removed),
                    Err(error) => error_result(&error),
                }
            }
        })
    }

    #[tool(
        name = "zvec_grep_index_drop",
        description = "Delete the persisted index for an absolute workspace root and release its daemon runtime.",
        output_schema = rmcp::handler::server::tool::schema_for_type::<IndexDropOutput>(),
        annotations(
            title = "Drop zvec-grep workspace index",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn zvec_grep_index_drop(
        &self,
        Parameters(input): Parameters<RootInput>,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let root = absolute_root(&input.root)
            .map_err(|message| ErrorData::invalid_params(message, None))?;
        let request = InfoOptions {
            root: Some(root.clone()),
            include_status: false,
        };
        Ok(match self.index_operations.drop_index(request).await {
            Ok(removed) => drop_result_to_result(&root, removed),
            Err(error) => error_result(&error),
        })
    }

    #[tool(
        name = "zvec_grep_rg",
        description = "Run exhaustive ripgrep across workspace material without an index. Pass a command starting with `rg`; it is parsed as arguments and never executed by a shell. Results are exhaustive unless a trailing `| head -N` explicitly bounds them.",
        annotations(
            title = "Search with managed ripgrep",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn zvec_grep_rg(
        &self,
        Parameters(input): Parameters<RgInput>,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let request = input
            .into_request()
            .map_err(|message| ErrorData::invalid_params(message, None))?;
        Ok(match self.engine.context(request).await {
            Ok(reply) => context_result_to_tool_result(&reply),
            Err(error) => error_result(&error),
        })
    }

    #[tool(
        name = "zvec_grep_index_status",
        description = "Read persisted index status for an absolute root. Use only after a missing-index response, indexing failure or cancellation, explicit progress monitoring, or daemon diagnostics.",
        output_schema = rmcp::handler::server::tool::schema_for_type::<IndexStatusOutput>(),
        annotations(
            title = "Inspect zvec-grep index status",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn zvec_grep_index_status(
        &self,
        Parameters(input): Parameters<RootInput>,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let root = absolute_root(&input.root)
            .map_err(|message| ErrorData::invalid_params(message, None))?;
        let request = InfoOptions {
            root: Some(root),
            include_status: true,
        };
        Ok(match self.engine.info(request).await {
            Ok(reply) => {
                let runtime = self.index_operations.runtime_snapshot(&reply.root);
                info_result_to_tool_result(reply, runtime)
            }
            Err(error) => error_result(&error),
        })
    }

    #[tool(
        name = "zvec_grep_server_status",
        description = "Read daemon version, queue, runtime and model-pool summary without exposing repository paths.",
        output_schema = rmcp::handler::server::tool::schema_for_type::<ServerStatusOutput>(),
        annotations(
            title = "Inspect zvec-grep server status",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn zvec_grep_server_status(
        &self,
        Parameters(_input): Parameters<EmptyInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let status = self.status.as_ref().ok_or_else(|| {
            ErrorData::internal_error("server status provider is unavailable", None)
        })?;
        Ok(structured_result(ServerStatusOutput::from(
            status.snapshot(),
        )))
    }
}

impl ServerHandler for ZvecGrepMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(rmcp::model::Implementation::new(
                "zvec-grep",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                match self.toolset {
                    McpToolset::Agent => AGENT_INSTRUCTIONS,
                    McpToolset::Full => FULL_INSTRUCTIONS,
                }
                .to_owned(),
            )
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        self.router
            .call(ToolCallContext::new(self, request, context))
            .await
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> + Send {
        std::future::ready(Ok(ListToolsResult {
            tools: self.router.list_all(),
            ..ListToolsResult::default()
        }))
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SearchInput {
    /// Absolute workspace root visible to the daemon.
    #[schemars(length(min = 1, max = 1024))]
    pub root: String,
    /// One-request embedding provider API key override.
    #[schemars(length(min = 1, max = 8192))]
    pub api_key: Option<String>,
    /// One-request local embedding device override.
    pub device: Option<DeviceInput>,
    /// One primary hybrid-search group.
    #[schemars(length(max = 4000))]
    pub query: Option<String>,
    /// One or more primary hybrid-search groups.
    pub queries: Option<QueryListInput>,
    /// Supplemental lexical-route groups.
    pub fts: Option<QueryListInput>,
    /// Supplemental semantic/vector-route groups.
    pub vector: Option<QueryListInput>,
    /// Maximum returned items per query group or fused plan.
    #[schemars(range(min = 1, max = 50))]
    pub limit: Option<usize>,
    /// Ordered case-sensitive rg-style glob rules.
    pub globs: Option<PathListInput>,
    /// Ordered case-insensitive rg-style glob rules.
    pub insensitive_globs: Option<PathListInput>,
    /// Included ripgrep file type names.
    pub file_types: Option<PathListInput>,
    /// Excluded ripgrep file type names.
    pub excluded_file_types: Option<PathListInput>,
    /// Include hidden paths.
    pub hidden: Option<bool>,
    /// Ignore repository ignore files.
    pub no_ignore: Option<bool>,
    /// Additional ignore files relative to the workspace root.
    pub ignore_files: Option<PathListInput>,
    /// Maximum recursive directory depth.
    pub max_depth: Option<usize>,
    /// Maximum indexed file size in bytes.
    #[schemars(range(min = 1))]
    pub max_file_size_bytes: Option<u64>,
    /// Follow symbolic links.
    pub follow: Option<bool>,
    /// Embedding requests processed concurrently during updates.
    #[schemars(range(min = 1))]
    pub embedding_concurrency: Option<usize>,
    /// Collapse all query groups into one ranked plan.
    pub fuse: Option<bool>,
    /// Prefer exact indexed symbols.
    pub prefer_symbol: Option<bool>,
    /// Restrict indexed results to symbol types.
    #[serde(default)]
    #[schemars(length(max = 6))]
    pub symbol_types: Vec<SymbolTypeInput>,
    /// Only query files modified after this time.
    pub modified_after: Option<TimeInput>,
    /// Only query files modified before this time.
    pub modified_before: Option<TimeInput>,
    /// Include per-hit search trace.
    pub trace: Option<bool>,
    /// Search now or wait for the active index to become fresh.
    #[serde(default)]
    pub freshness: FreshnessInput,
    /// Allow eventual search to schedule a background index update.
    #[serde(default = "default_auto_update")]
    pub auto_update: bool,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct IndexInput {
    /// Absolute workspace root visible to the daemon.
    #[schemars(length(min = 1, max = 1024))]
    pub root: String,
    /// One-request embedding provider API key override.
    #[schemars(length(min = 1, max = 8192))]
    pub api_key: Option<String>,
    /// One-request local embedding device override.
    pub device: Option<DeviceInput>,
    /// Remote embedding endpoint override.
    #[schemars(length(max = 2048))]
    pub endpoint: Option<String>,
    /// Permanently remove the workspace index.
    pub drop: Option<bool>,
    /// Embedding model reference for a new index.
    #[schemars(length(min = 1, max = 256))]
    pub embedding: Option<String>,
    /// Explicitly rebuild the existing index.
    pub rebuild: Option<bool>,
    /// Replace the index root-path configuration.
    pub reset_paths: Option<bool>,
    pub globs: Option<PathListInput>,
    pub insensitive_globs: Option<PathListInput>,
    pub file_types: Option<PathListInput>,
    pub excluded_file_types: Option<PathListInput>,
    pub hidden: Option<bool>,
    pub no_ignore: Option<bool>,
    pub ignore_files: Option<PathListInput>,
    pub max_depth: Option<usize>,
    #[schemars(range(min = 1))]
    pub max_file_size_bytes: Option<u64>,
    pub follow: Option<bool>,
    /// Embedding batch tasks processed concurrently during this update.
    #[schemars(range(min = 1))]
    pub embedding_concurrency: Option<usize>,
    /// Include bounded skipped-file diagnostics after completion.
    pub debug: Option<bool>,
    /// Wait for the submitted index job to finish.
    pub wait: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct RootInput {
    /// Absolute workspace root visible to the daemon.
    #[schemars(length(min = 1, max = 1024))]
    pub root: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct RgInput {
    /// Absolute workspace root visible to the daemon.
    #[schemars(length(min = 1, max = 1024))]
    pub root: String,
    /// A command beginning with `rg`; parsed without a shell.
    #[schemars(length(min = 1, max = 4000))]
    pub command: String,
}

enum IndexToolRequest {
    Index {
        options: Box<IndexOptions>,
        wait: bool,
    },
    Drop(InfoOptions),
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
pub struct EmptyInput {}

#[derive(Clone, Copy, Debug, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case")]
enum IndexJobState {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case")]
enum IndexActionOutput {
    Index,
    Drop,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
struct IndexOutput {
    root: String,
    job_id: String,
    state: IndexJobState,
    reused: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    action: Option<IndexActionOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dropped: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<IndexJobErrorOutput>,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
struct IndexJobErrorOutput {
    code: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    help: Option<String>,
    origin: ErrorSiteOutput,
    #[serde(skip_serializing_if = "Option::is_none")]
    reported_at: Option<ErrorSiteOutput>,
    retryable: bool,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
struct ErrorSiteOutput {
    file: String,
    line: u32,
    column: u32,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
struct IndexDropOutput {
    root: String,
    removed: bool,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
struct IndexStatusOutput {
    root: String,
    indexed: bool,
    index_policy: String,
    source: String,
    persistent: PersistentIndexStatusOutput,
    #[serde(skip_serializing_if = "Option::is_none")]
    runtime: Option<IndexRuntimeStatusOutput>,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
struct IndexRuntimeStatusOutput {
    watcher_active: bool,
    dirty_revision: u64,
    indexed_revision: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    active_job_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    job_state: Option<IndexJobState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    progress: Option<IndexJobProgressOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<IndexJobErrorOutput>,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
struct IndexJobProgressOutput {
    phase: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    files_total: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    files_indexed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    files_failed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
struct PersistentIndexStatusOutput {
    home: String,
    index_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_index: Option<WorkspaceIndexOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    files: Option<IndexFilesOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    suggestion: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
struct WorkspaceIndexOutput {
    id: String,
    name: String,
    path: String,
    root_paths: Vec<RootSpecOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    embedding: Option<IndexedEmbeddingOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    index_version: Option<u32>,
    created_time: u64,
    updated_time: u64,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
#[allow(clippy::struct_excessive_bools)]
struct RootSpecOutput {
    absolute_path: String,
    recursive: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    include: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    exclude: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    globs: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    insensitive_globs: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    file_types: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    excluded_file_types: Vec<String>,
    #[serde(skip_serializing_if = "is_false")]
    hidden: bool,
    #[serde(skip_serializing_if = "is_false")]
    no_ignore: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    ignore_files: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_depth: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_file_size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "is_false")]
    follow: bool,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
struct IndexedEmbeddingOutput {
    provider: String,
    model: String,
    dimension: usize,
    metric: String,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
struct IndexFilesOutput {
    stored: usize,
    scanned: usize,
    indexed: usize,
    pending: usize,
    failed: usize,
    added: usize,
    modified: usize,
    deleted: usize,
    unchanged: usize,
    entities: usize,
    truncated_fragments: usize,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
struct ServerStatusOutput {
    version: String,
    uptime_ms: u64,
    shutting_down: bool,
    active_runtimes: usize,
    queued_jobs: usize,
    running_jobs: usize,
    models: ModelStatusOutput,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
struct ModelStatusOutput {
    loaded: usize,
    active_leases: usize,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum QueryListInput {
    One(#[schemars(length(max = 4000))] String),
    Many(#[schemars(length(max = 32), inner(length(max = 4000)))] Vec<String>),
}

impl QueryListInput {
    fn normalized(self, name: &str) -> Result<Vec<String>, String> {
        let values = match self {
            Self::One(value) => vec![value],
            Self::Many(values) => values,
        };
        if values.len() > MAX_QUERY_GROUPS {
            return Err(format!("{name} accepts at most {MAX_QUERY_GROUPS} values"));
        }
        Ok(values
            .into_iter()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .collect())
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum PathListInput {
    One(#[schemars(length(max = 1024))] String),
    Many(#[schemars(length(max = 128), inner(length(max = 1024)))] Vec<String>),
}

impl PathListInput {
    fn normalized(self, name: &str) -> Result<Vec<String>, String> {
        let values = match self {
            Self::One(value) => vec![value],
            Self::Many(values) => values,
        };
        if values.len() > MAX_PATH_FILTERS {
            return Err(format!("{name} accepts at most {MAX_PATH_FILTERS} values"));
        }
        Ok(values
            .into_iter()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .collect())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeviceInput {
    Auto,
    Cpu,
    Metal,
    Vulkan,
    Cuda,
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SymbolTypeInput {
    Module,
    Class,
    Interface,
    Function,
    Value,
    Alias,
}

impl From<SymbolTypeInput> for SymbolType {
    fn from(value: SymbolTypeInput) -> Self {
        match value {
            SymbolTypeInput::Module => Self::Module,
            SymbolTypeInput::Class => Self::Class,
            SymbolTypeInput::Interface => Self::Interface,
            SymbolTypeInput::Function => Self::Function,
            SymbolTypeInput::Value => Self::Value,
            SymbolTypeInput::Alias => Self::Alias,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessInput {
    #[default]
    Eventual,
    WaitForFresh,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum TimeInput {
    EpochMillis(u64),
    Text(#[schemars(length(max = 128))] String),
}

const fn default_auto_update() -> bool {
    true
}

impl SearchInput {
    fn into_request(self) -> Result<ContextOptions, String> {
        let root = absolute_root(&self.root)?;
        if let Some(api_key) = &self.api_key {
            validate_text("apiKey", api_key, 1, 8_192)?;
        }
        if self.device.is_some() {
            return Err(
                "device is not available until the local embedding runtime is implemented"
                    .to_owned(),
            );
        }
        if self
            .limit
            .is_some_and(|limit| limit == 0 || limit > MAX_SEARCH_LIMIT)
        {
            return Err(format!("limit must be between 1 and {MAX_SEARCH_LIMIT}"));
        }
        if self.embedding_concurrency == Some(0) {
            return Err("embeddingConcurrency must be greater than zero".to_owned());
        }
        if self.max_file_size_bytes == Some(0) {
            return Err("maxFileSizeBytes must be greater than zero".to_owned());
        }
        if self.symbol_types.len() > 6 {
            return Err("symbolTypes accepts at most 6 values".to_owned());
        }

        let mut queries = normalize_optional_one(self.query, "query")?;
        queries.extend(normalize_query_list(self.queries, "queries")?);
        let fts = normalize_query_list(self.fts, "fts")?;
        let vector = normalize_query_list(self.vector, "vector")?;
        if queries.is_empty() && fts.is_empty() && vector.is_empty() {
            return Err("zvec_grep_search requires query, queries, fts, or vector".to_owned());
        }

        let routes = fts
            .into_iter()
            .map(|query| ContextRoute {
                mode: ContextRouteMode::Fts,
                query,
            })
            .chain(vector.into_iter().map(|query| ContextRoute {
                mode: ContextRouteMode::Vector,
                query,
            }))
            .collect();
        let auto_update =
            self.auto_update || matches!(self.freshness, FreshnessInput::WaitForFresh);
        let ignore_files = normalize_path_list(self.ignore_files, "ignoreFiles")?
            .into_iter()
            .map(PathBuf::from)
            .collect();

        let request = ContextOptions {
            query: None,
            root: Some(root.clone()),
            queries,
            routes,
            fuse: self.fuse.unwrap_or(false),
            limit: self.limit,
            auto_update,
            refresh: Some(if matches!(self.freshness, FreshnessInput::WaitForFresh) {
                RefreshPolicy::Wait
            } else if self.auto_update {
                RefreshPolicy::Background
            } else {
                RefreshPolicy::Off
            }),
            trace: self.trace.unwrap_or(false),
            prefer_symbol: self.prefer_symbol.unwrap_or(false),
            symbol_types: self.symbol_types.into_iter().map(Into::into).collect(),
            globs: normalize_path_list(self.globs, "globs")?,
            insensitive_globs: normalize_path_list(self.insensitive_globs, "insensitiveGlobs")?,
            file_types: normalize_path_list(self.file_types, "fileTypes")?,
            excluded_file_types: normalize_path_list(
                self.excluded_file_types,
                "excludedFileTypes",
            )?,
            hidden: self.hidden.unwrap_or(false),
            no_ignore: self.no_ignore.unwrap_or(false),
            ignore_files,
            max_depth: self.max_depth,
            max_file_size_bytes: self.max_file_size_bytes,
            follow: self.follow.unwrap_or(false),
            modified_after_epoch_ms: parse_optional_time(self.modified_after, "modifiedAfter")?,
            modified_before_epoch_ms: parse_optional_time(self.modified_before, "modifiedBefore")?,
            api_key: self.api_key,
            embedding_concurrency: self.embedding_concurrency,
            ..ContextOptions::default()
        };
        if request
            .modified_after_epoch_ms
            .zip(request.modified_before_epoch_ms)
            .is_some_and(|(after, before)| after > before)
        {
            return Err("modifiedAfter must not be later than modifiedBefore".to_owned());
        }
        validate_scoped_paths(&root, &request.ignore_files, "ignore file")?;
        Ok(request)
    }
}

impl From<DeviceInput> for Device {
    fn from(value: DeviceInput) -> Self {
        match value {
            DeviceInput::Auto => Self::Auto,
            DeviceInput::Cpu => Self::Cpu,
            DeviceInput::Metal => Self::Metal,
            DeviceInput::Vulkan => Self::Vulkan,
            DeviceInput::Cuda => Self::Cuda,
        }
    }
}

impl IndexInput {
    fn into_request(self) -> Result<IndexToolRequest, String> {
        let root = absolute_root(&self.root)?;
        if self.drop.unwrap_or(false) {
            if self.has_index_options() {
                return Err(
                    "drop: true cannot be combined with indexing, model, filter, wait, or debug options"
                        .to_owned(),
                );
            }
            return Ok(IndexToolRequest::Drop(InfoOptions {
                root: Some(root),
                include_status: false,
            }));
        }
        if let Some(api_key) = &self.api_key {
            validate_text("apiKey", api_key, 1, 8_192)?;
        }
        if self.embedding_concurrency == Some(0) {
            return Err("embeddingConcurrency must be greater than zero".to_owned());
        }
        if self.max_file_size_bytes == Some(0) {
            return Err("maxFileSizeBytes must be greater than zero".to_owned());
        }
        if let Some(endpoint) = &self.endpoint {
            validate_text("endpoint", endpoint, 1, 2_048)?;
            if self.embedding.is_none() {
                return Err("endpoint requires an explicit embedding model".to_owned());
            }
        }
        if self.device.is_some() && self.embedding.is_none() {
            return Err("device requires an explicit embedding model".to_owned());
        }

        let discovery = IndexDiscoveryOptions {
            globs: normalize_path_list(self.globs, "globs")?,
            insensitive_globs: normalize_path_list(self.insensitive_globs, "insensitiveGlobs")?,
            file_types: normalize_path_list(self.file_types, "fileTypes")?,
            excluded_file_types: normalize_path_list(
                self.excluded_file_types,
                "excludedFileTypes",
            )?,
            hidden: self.hidden.unwrap_or(false),
            no_ignore: self.no_ignore.unwrap_or(false),
            ignore_files: normalize_path_list(self.ignore_files, "ignoreFiles")?
                .into_iter()
                .map(PathBuf::from)
                .collect(),
            max_depth: self.max_depth,
            max_file_size_bytes: self.max_file_size_bytes,
            follow: self.follow.unwrap_or(false),
            ..IndexDiscoveryOptions::default()
        };
        validate_scoped_paths(&root, &discovery.ignore_files, "ignore file")?;
        let root_spec = RootPath {
            path: root.clone(),
            recursive: true,
            discovery: discovery.clone(),
        };
        let embedding = if let Some(reference) = self.embedding {
            let reference = reference.trim().to_owned();
            validate_text("embedding", &reference, 1, 256)?;
            Some(EmbeddingModelSpec {
                reference,
                revision: None,
                cache_dir: None,
                endpoint: self.endpoint,
                device: self.device.map_or(Device::Auto, Into::into),
            })
        } else {
            None
        };
        Ok(IndexToolRequest::Index {
            options: Box::new(IndexOptions {
                root: Some(root),
                roots: vec![root_spec],
                rebuild: self.rebuild.unwrap_or(false),
                reset_paths: self.reset_paths.unwrap_or(false),
                discovery,
                embedding,
                api_key: self.api_key,
                embedding_concurrency: self.embedding_concurrency,
                ..IndexOptions::default()
            }),
            wait: self.wait.unwrap_or(false),
        })
    }

    fn has_index_options(&self) -> bool {
        self.api_key.is_some()
            || self.device.is_some()
            || self.endpoint.is_some()
            || self.embedding.is_some()
            || self.rebuild.is_some()
            || self.reset_paths.is_some()
            || self.globs.is_some()
            || self.insensitive_globs.is_some()
            || self.file_types.is_some()
            || self.excluded_file_types.is_some()
            || self.hidden.is_some()
            || self.no_ignore.is_some()
            || self.ignore_files.is_some()
            || self.max_depth.is_some()
            || self.max_file_size_bytes.is_some()
            || self.follow.is_some()
            || self.embedding_concurrency.is_some()
            || self.debug.is_some()
            || self.wait.is_some()
    }
}

impl RgInput {
    fn into_request(self) -> Result<ContextOptions, String> {
        let root = absolute_root(&self.root)?;
        validate_text("command", &self.command, 1, MAX_QUERY_CHARS)?;
        let (args, limit) = parse_rg_command(&self.command)?;
        let mut request = parse_managed_rg_args(&args).map_err(|error| error.to_string())?;
        request.limit = limit;
        validate_scoped_paths(&root, &request.rg_paths, "search path")?;
        validate_scoped_paths(&root, &request.rg_options.pattern_files, "pattern file")?;
        validate_scoped_paths(&root, &request.ignore_files, "ignore file")?;
        request.root = Some(root);
        Ok(request)
    }
}

fn absolute_root(value: &str) -> Result<PathBuf, String> {
    validate_text("root", value, 1, MAX_PATH_CHARS)?;
    let root = PathBuf::from(value.trim());
    if !root.is_absolute() {
        return Err("root must be an absolute path".to_owned());
    }
    if root
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Err("root must not contain parent-directory components".to_owned());
    }
    Ok(root)
}

fn validate_scoped_paths(root: &Path, paths: &[PathBuf], kind: &str) -> Result<(), String> {
    for path in paths {
        if path
            .components()
            .any(|component| component == Component::ParentDir)
        {
            return Err(format!(
                "{kind} {} escapes the workspace root",
                path.display()
            ));
        }
        if path.is_absolute() && !path.starts_with(root) {
            return Err(format!(
                "{kind} {} is outside the workspace root",
                path.display()
            ));
        }
    }
    Ok(())
}

fn parse_rg_command(command: &str) -> Result<(Vec<String>, Option<usize>), String> {
    let mut tokens = scan_rg_command(command)?;
    let mut limit = None;
    if let Some(pipe) = tokens.iter().rposition(|token| token == "|") {
        limit = Some(parse_head_limit(&tokens[pipe + 1..])?);
        tokens.truncate(pipe);
    }
    if tokens.ends_with(&["2".to_owned(), ">".to_owned(), "/dev/null".to_owned()]) {
        tokens.truncate(tokens.len() - 3);
    }
    if let Some(operator) = tokens
        .iter()
        .find(|token| matches!(token.as_str(), "|" | ">"))
    {
        return Err(format!(
            "rg command does not support shell operator {operator:?}"
        ));
    }
    if tokens.first().map(String::as_str) != Some("rg") {
        return Err("rg command must start with \"rg\"".to_owned());
    }
    if tokens.len() == 1 {
        return Err("rg command requires a pattern".to_owned());
    }
    Ok((tokens.split_off(1), limit))
}

fn scan_rg_command(command: &str) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut token_started = false;
    let mut quote = None;
    let mut escaping = false;
    let mut characters = command.chars().peekable();

    while let Some(character) = characters.next() {
        if character == '\0' {
            return Err("rg command cannot contain NUL characters".to_owned());
        }
        if matches!(character, '\n' | '\r') {
            return Err("rg command must be a single command on one line".to_owned());
        }
        if escaping {
            token.push(character);
            token_started = true;
            escaping = false;
            continue;
        }
        if quote == Some('\'') {
            if character == '\'' {
                quote = None;
            } else {
                token.push(character);
            }
            token_started = true;
            continue;
        }
        if quote == Some('"') {
            if character == '"' {
                quote = None;
            } else if character == '\\' {
                let Some(next) = characters.peek().copied() else {
                    return Err("rg command ends with an incomplete escape".to_owned());
                };
                if matches!(next, '"' | '\\' | '$' | '`') {
                    token.push(next);
                    characters.next();
                } else {
                    token.push(character);
                }
            } else if character == '`'
                || (character == '$' && matches!(characters.peek(), Some('(' | '{')))
            {
                return Err("rg command does not support shell expansion".to_owned());
            } else {
                token.push(character);
            }
            token_started = true;
            continue;
        }
        if character.is_whitespace() {
            finish_token(&mut tokens, &mut token, &mut token_started);
            continue;
        }
        if matches!(character, '\'' | '"') {
            quote = Some(character);
            token_started = true;
            continue;
        }
        if character == '\\' {
            escaping = true;
            token_started = true;
            continue;
        }
        if matches!(character, '|' | '>') {
            finish_token(&mut tokens, &mut token, &mut token_started);
            tokens.push(character.to_string());
            continue;
        }
        if matches!(character, '&' | ';' | '<' | '(' | ')') {
            return Err(format!(
                "rg command does not support shell operator {character:?}"
            ));
        }
        if character == '`' || (character == '$' && matches!(characters.peek(), Some('(' | '{'))) {
            return Err("rg command does not support shell expansion".to_owned());
        }
        token.push(character);
        token_started = true;
    }
    if escaping {
        return Err("rg command ends with an incomplete escape".to_owned());
    }
    if let Some(quote) = quote {
        return Err(format!("rg command has an unclosed {quote} quote"));
    }
    finish_token(&mut tokens, &mut token, &mut token_started);
    Ok(tokens)
}

fn finish_token(tokens: &mut Vec<String>, token: &mut String, started: &mut bool) {
    if *started {
        tokens.push(std::mem::take(token));
        *started = false;
    }
}

fn parse_head_limit(tokens: &[String]) -> Result<usize, String> {
    if tokens.first().map(String::as_str) != Some("head") {
        return Err("rg command only supports a trailing | head output bound".to_owned());
    }
    let raw = match tokens {
        [_] => "10",
        [_, value] if value.starts_with('-') => &value[1..],
        [_, option, value] if option == "-n" => value,
        _ => return Err("rg command only supports a trailing | head output bound".to_owned()),
    };
    let limit = raw
        .parse::<usize>()
        .map_err(|_| "rg command head limit must be a positive integer".to_owned())?;
    if limit == 0 {
        return Err("rg command head limit must be a positive integer".to_owned());
    }
    Ok(limit)
}

fn normalize_optional_one(value: Option<String>, name: &str) -> Result<Vec<String>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    validate_text(name, &value, 0, MAX_QUERY_CHARS)?;
    let value = value.trim();
    Ok((!value.is_empty())
        .then(|| value.to_owned())
        .into_iter()
        .collect())
}

fn normalize_query_list(value: Option<QueryListInput>, name: &str) -> Result<Vec<String>, String> {
    let values = value.map_or_else(|| Ok(Vec::new()), |value| value.normalized(name))?;
    for value in &values {
        validate_text(name, value, 0, MAX_QUERY_CHARS)?;
    }
    Ok(values)
}

fn normalize_path_list(value: Option<PathListInput>, name: &str) -> Result<Vec<String>, String> {
    let values = value.map_or_else(|| Ok(Vec::new()), |value| value.normalized(name))?;
    for value in &values {
        validate_text(name, value, 0, MAX_PATH_CHARS)?;
    }
    Ok(values)
}

fn validate_text(name: &str, value: &str, min: usize, max: usize) -> Result<(), String> {
    let length = value.chars().count();
    if length < min || length > max {
        return Err(format!(
            "{name} must contain between {min} and {max} characters"
        ));
    }
    Ok(())
}

fn parse_optional_time(value: Option<TimeInput>, name: &str) -> Result<Option<u64>, String> {
    value.map(|value| parse_time(value, name)).transpose()
}

fn parse_time(value: TimeInput, name: &str) -> Result<u64, String> {
    if let TimeInput::EpochMillis(value) = value {
        return Ok(value);
    }
    let TimeInput::Text(value) = value else {
        unreachable!();
    };
    validate_text(name, &value, 0, 128)?;
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        return value
            .parse()
            .map_err(|_| format!("{name} requires a valid epoch millisecond value"));
    }
    if let Ok(value) = DateTime::parse_from_rfc3339(&value) {
        return epoch_millis(value.timestamp_millis(), name);
    }
    if let Ok(value) = DateTime::parse_from_rfc2822(&value) {
        return epoch_millis(value.timestamp_millis(), name);
    }
    if let Ok(date) = NaiveDate::parse_from_str(&value, "%Y-%m-%d") {
        let local = date
            .and_hms_opt(0, 0, 0)
            .and_then(|date_time| {
                Local
                    .from_local_datetime(&date_time)
                    .single()
                    .or_else(|| Local.from_local_datetime(&date_time).earliest())
            })
            .ok_or_else(|| format!("{name} is not a valid local date"))?;
        return epoch_millis(local.timestamp_millis(), name);
    }
    Err(format!(
        "{name} requires an epoch millisecond value or an RFC 3339/RFC 2822/date value"
    ))
}

fn epoch_millis(value: i64, name: &str) -> Result<u64, String> {
    u64::try_from(value).map_err(|_| format!("{name} must not be before the Unix epoch"))
}

fn error_result(error: &EngineError) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(format!(
        "{}\nretryable: {}",
        error.report(),
        error.is_retryable()
    ))])
}

fn structured_result(value: impl Serialize) -> CallToolResult {
    match serde_json::to_value(value) {
        Ok(value) => {
            let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
            let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
            result.structured_content = Some(value);
            result
        }
        Err(error) => CallToolResult::error(vec![ContentBlock::text(format!(
            "error_code: {}\nerror_message: failed to serialize tool result: {error}",
            EngineError::INTERNAL
        ))]),
    }
}

fn request_root(root: Option<&Path>) -> PathBuf {
    root.map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

fn index_operation_to_result(reply: &IndexOperationResult) -> CallToolResult {
    structured_result(IndexOutput {
        root: reply.root.display().to_string(),
        job_id: reply.job_id.clone(),
        state: match reply.state {
            IndexOperationState::Queued => IndexJobState::Queued,
            IndexOperationState::Running => IndexJobState::Running,
            IndexOperationState::Succeeded => IndexJobState::Succeeded,
            IndexOperationState::Failed => IndexJobState::Failed,
            IndexOperationState::Cancelled => IndexJobState::Cancelled,
        },
        reused: reply.reused,
        action: Some(IndexActionOutput::Index),
        dropped: None,
        error: reply.error.as_ref().map(index_job_error_output),
    })
}

fn drop_result_to_index_result(root: &Path, removed: bool) -> CallToolResult {
    structured_result(IndexOutput {
        root: root.display().to_string(),
        job_id: "drop".to_owned(),
        state: IndexJobState::Succeeded,
        reused: false,
        action: Some(IndexActionOutput::Drop),
        dropped: Some(removed),
        error: None,
    })
}

fn drop_result_to_result(root: &Path, removed: bool) -> CallToolResult {
    structured_result(IndexDropOutput {
        root: root.display().to_string(),
        removed,
    })
}

fn info_result_to_tool_result(
    reply: InfoResult,
    runtime: Option<IndexRuntimeSnapshot>,
) -> CallToolResult {
    let mut output = IndexStatusOutput::from(reply);
    output.runtime = runtime.map(IndexRuntimeStatusOutput::from);
    structured_result(output)
}

impl From<InfoResult> for IndexStatusOutput {
    fn from(reply: InfoResult) -> Self {
        let workspace_index = reply.workspace_index.map(|info| WorkspaceIndexOutput {
            id: info.id,
            name: info.name,
            path: info.path.display().to_string(),
            root_paths: info.roots.into_iter().map(RootSpecOutput::from).collect(),
            embedding: info.embedding.map(|embedding| IndexedEmbeddingOutput {
                provider: embedding.provider,
                model: embedding.model,
                dimension: embedding.dimension,
                metric: embedding.metric,
            }),
            index_version: info.index_version,
            created_time: info.created_epoch_ms,
            updated_time: info.updated_epoch_ms,
        });
        let files = reply.status.map(|status| IndexFilesOutput {
            stored: status.files_stored,
            scanned: status.files_scanned,
            indexed: status.files_indexed,
            pending: status.files_pending,
            failed: status.files_failed,
            added: status.files_added,
            modified: status.files_modified,
            deleted: status.files_deleted,
            unchanged: status.files_unchanged,
            entities: status.entities_indexed,
            truncated_fragments: status.fragments_truncated,
        });
        Self {
            root: reply.root.display().to_string(),
            indexed: reply.indexed,
            index_policy: index_policy_label(reply.index_policy).to_owned(),
            source: match reply.source {
                InfoSource::Index => "index",
                InfoSource::Unindexed => "unindexed",
            }
            .to_owned(),
            persistent: PersistentIndexStatusOutput {
                home: reply.home.display().to_string(),
                index_path: reply.index_path.display().to_string(),
                workspace_index,
                files,
                suggestion: reply.suggestion,
            },
            runtime: None,
        }
    }
}

impl From<IndexRuntimeSnapshot> for IndexRuntimeStatusOutput {
    fn from(runtime: IndexRuntimeSnapshot) -> Self {
        Self {
            watcher_active: runtime.watcher_active,
            dirty_revision: runtime.dirty_revision,
            indexed_revision: runtime.indexed_revision,
            active_job_id: runtime.active_job_id,
            job_state: runtime.job_state.map(index_job_state),
            progress: runtime.progress.map(|progress| IndexJobProgressOutput {
                phase: match progress.phase {
                    zg_engine::api::index::progress::IndexProgressPhase::Scanning => "scanning",
                    zg_engine::api::index::progress::IndexProgressPhase::Indexing => "indexing",
                    zg_engine::api::index::progress::IndexProgressPhase::Done => "done",
                }
                .to_owned(),
                files_total: progress.files_total,
                files_indexed: progress.files_indexed,
                files_failed: progress.files_failed,
                detail: progress.detail,
            }),
            error: runtime.error.as_ref().map(index_job_error_output),
        }
    }
}

fn index_job_error_output(error: &IndexOperationError) -> IndexJobErrorOutput {
    IndexJobErrorOutput {
        code: error.report.code.clone(),
        message: error.report.message.clone(),
        help: error.report.help.clone(),
        origin: error_site_output(&error.report.origin),
        reported_at: error.report.reported_at.as_ref().map(error_site_output),
        retryable: error.retryable,
    }
}

fn error_site_output(site: &ErrorSite) -> ErrorSiteOutput {
    ErrorSiteOutput {
        file: site.file.clone(),
        line: site.line,
        column: site.column,
    }
}

const fn index_job_state(state: IndexOperationState) -> IndexJobState {
    match state {
        IndexOperationState::Queued => IndexJobState::Queued,
        IndexOperationState::Running => IndexJobState::Running,
        IndexOperationState::Succeeded => IndexJobState::Succeeded,
        IndexOperationState::Failed => IndexJobState::Failed,
        IndexOperationState::Cancelled => IndexJobState::Cancelled,
    }
}

impl From<RootPath> for RootSpecOutput {
    fn from(root: RootPath) -> Self {
        Self {
            absolute_path: root.path.display().to_string(),
            recursive: root.recursive,
            include: root.discovery.include_paths,
            exclude: root.discovery.exclude_paths,
            globs: root.discovery.globs,
            insensitive_globs: root.discovery.insensitive_globs,
            file_types: root.discovery.file_types,
            excluded_file_types: root.discovery.excluded_file_types,
            hidden: root.discovery.hidden,
            no_ignore: root.discovery.no_ignore,
            ignore_files: root
                .discovery
                .ignore_files
                .into_iter()
                .map(|path| path.display().to_string())
                .collect(),
            max_depth: root.discovery.max_depth,
            max_file_size_bytes: root.discovery.max_file_size_bytes,
            follow: root.discovery.follow,
        }
    }
}

impl From<ServerStatusSnapshot> for ServerStatusOutput {
    fn from(status: ServerStatusSnapshot) -> Self {
        Self {
            version: status.version,
            uptime_ms: status.uptime_ms,
            shutting_down: status.shutting_down,
            active_runtimes: status.active_runtimes,
            queued_jobs: status.queued_jobs,
            running_jobs: status.running_jobs,
            models: ModelStatusOutput {
                loaded: status.loaded_models,
                active_leases: status.active_model_leases,
            },
        }
    }
}

fn index_policy_label(policy: WorkspaceIndexPolicy) -> &'static str {
    match policy {
        WorkspaceIndexPolicy::Enabled => "enabled",
        WorkspaceIndexPolicy::Disabled => "disabled",
        WorkspaceIndexPolicy::Undecided => "undecided",
    }
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_false(value: &bool) -> bool {
    !*value
}

fn context_result_to_tool_result(reply: &ContextResult) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(format_context_result(reply))])
}

fn format_context_result(reply: &ContextResult) -> String {
    let freshness = if reply
        .items
        .iter()
        .any(|item| item.status == ContextItemStatus::PossiblyStale)
    {
        "possibly_stale"
    } else {
        "fresh"
    };
    let freshness = reply.freshness.as_deref().unwrap_or(freshness);
    let mut output = format!("freshness: {freshness}");
    if let Some(refresh) = &reply.background_refresh {
        let _ = write!(output, "\nbackground_refresh: {refresh}");
    }
    if reply.items.is_empty() {
        let _ = write!(output, "\nNo matches.");
        return output;
    }
    let mut items: Vec<&ContextItem> = reply.items.iter().collect();
    items.sort_by_key(|item| item.rank);
    for item in items {
        let _ = write!(
            output,
            "\n\n#{} matchedBy={} {}:{}",
            item.rank,
            matched_by_label(item.matched_by),
            item.relative_path.display(),
            range_label(&item.range)
        );
        if let Some(outline) = &item.outline {
            for line in outline.lines().take(7) {
                let _ = write!(output, "\noutline: {}", truncate_line(line));
            }
        }
        output.push_str("\nsource:");
        for line in item.content.lines().take(10) {
            let _ = write!(output, "\n  {}", truncate_line(line));
        }
    }
    output
}

fn matched_by_label(value: MatchedBy) -> &'static str {
    match value {
        MatchedBy::Fts => "fts",
        MatchedBy::Vector => "vector",
        MatchedBy::FtsAndVector => "fts+vector",
        MatchedBy::Lexical => "lexical",
    }
}

fn range_label(range: &ContentRange) -> String {
    match range {
        ContentRange::File => "file".to_owned(),
        ContentRange::Text {
            start_line,
            end_line,
            ..
        }
        | ContentRange::LineColumn {
            start_line,
            end_line,
            ..
        } if start_line == end_line => start_line.to_string(),
        ContentRange::Text {
            start_line,
            end_line,
            ..
        }
        | ContentRange::LineColumn {
            start_line,
            end_line,
            ..
        } => format!("{start_line}-{end_line}"),
        ContentRange::Byte {
            start_offset,
            end_offset,
        } => format!("bytes:{start_offset}-{end_offset}"),
        ContentRange::Page { page } => format!("page:{page}"),
        ContentRange::PageText { page, .. } | ContentRange::PageRegion { page, .. } => {
            format!("page:{page}")
        }
    }
}

fn truncate_line(line: &str) -> String {
    const MAX_LINE_CHARS: usize = 160;
    let mut chars = line.chars();
    let prefix: String = chars.by_ref().take(MAX_LINE_CHARS).collect();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc};

    use rmcp::ServerHandler;
    use rmcp::model::ContentBlock;
    use zg_engine::{EngineError, ZvecGrep};

    use super::{
        AGENT_TOOL_NAME, FULL_TOOL_NAMES, FreshnessInput, IndexInput, IndexToolRequest,
        PathListInput, QueryListInput, RgInput, SearchInput, ServerStatusProvider,
        ServerStatusSnapshot, ZvecGrepMcpServer, error_result,
    };

    struct FixedStatus;

    impl ServerStatusProvider for FixedStatus {
        fn snapshot(&self) -> ServerStatusSnapshot {
            ServerStatusSnapshot {
                version: "test".to_owned(),
                ..ServerStatusSnapshot::default()
            }
        }
    }

    fn test_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("workspace")
    }

    fn input() -> SearchInput {
        SearchInput {
            root: test_root().display().to_string(),
            api_key: None,
            device: None,
            query: Some("call chain".to_owned()),
            queries: None,
            fts: Some(QueryListInput::One("run".to_owned())),
            vector: None,
            limit: Some(8),
            globs: Some(PathListInput::Many(vec!["*.rs".to_owned()])),
            insensitive_globs: None,
            file_types: None,
            excluded_file_types: None,
            hidden: Some(true),
            no_ignore: None,
            ignore_files: None,
            max_depth: None,
            max_file_size_bytes: None,
            follow: None,
            embedding_concurrency: Some(2),
            fuse: Some(true),
            prefer_symbol: Some(true),
            symbol_types: Vec::new(),
            modified_after: None,
            modified_before: None,
            trace: Some(true),
            freshness: FreshnessInput::Eventual,
            auto_update: true,
        }
    }

    #[test]
    fn engine_error_retryability_matches_the_shared_contract() {
        let busy = error_result(&EngineError::resource_busy("workspace is locked"));
        let storage = error_result(&EngineError::storage_failure("manifest is corrupt"));

        let ContentBlock::Text(busy) = &busy.content[0] else {
            panic!("error output must be text");
        };
        let ContentBlock::Text(storage) = &storage.content[0] else {
            panic!("error output must be text");
        };
        assert!(busy.text.contains("retryable: true"));
        assert!(storage.text.contains("retryable: false"));
    }

    #[test]
    fn wait_for_fresh_survives_request_mapping() {
        let mut search = input();
        search.freshness = FreshnessInput::WaitForFresh;
        search.auto_update = false;
        let request = search.into_request().expect("search input should map");
        let value = serde_json::to_value(request).expect("request should serialize");
        assert_eq!(value["refresh"], "wait");
    }

    #[test]
    fn maps_agent_search_to_context_options() {
        let request = input().into_request().expect("search input should map");
        assert_eq!(request.root, Some(test_root()));
        assert_eq!(request.queries, ["call chain"]);
        assert_eq!(request.routes.len(), 1);
        assert!(request.auto_update);
        assert_eq!(request.globs, ["*.rs"]);
        assert!(request.fuse);
        assert!(request.prefer_symbol);
        assert!(request.trace);
    }

    #[test]
    fn agent_server_exposes_only_search() {
        let server = ZvecGrepMcpServer::agent(Arc::new(ZvecGrep::new()));
        let tools = server.listed_tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, AGENT_TOOL_NAME);
        assert!(server.get_info().instructions.is_some());
    }

    #[test]
    fn full_server_exposes_all_six_tools() {
        let server = ZvecGrepMcpServer::full(Arc::new(ZvecGrep::new()), Arc::new(FixedStatus));
        let names = server
            .listed_tools()
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect::<Vec<_>>();
        assert_eq!(names, FULL_TOOL_NAMES);
        assert!(
            server
                .get_info()
                .instructions
                .is_some_and(|instructions| instructions.contains("zvec_grep_index"))
        );
    }

    #[test]
    fn maps_full_index_to_index_request() {
        let request = index_input()
            .into_request()
            .expect("index input should map");
        let IndexToolRequest::Index {
            options: request,
            wait,
        } = request
        else {
            panic!("index tool must create an index request");
        };
        assert!(wait);
        assert_eq!(request.root, Some(test_root()));
        assert_eq!(request.roots.len(), 1);
        assert_eq!(request.discovery.globs, ["*.rs"]);
        assert_eq!(
            request
                .embedding
                .as_ref()
                .map(|model| model.reference.as_str()),
            Some("potion-base-8M")
        );
    }

    #[test]
    fn maps_full_rg_without_using_a_shell_and_enforces_root_scope() {
        let request = RgInput {
            root: test_root().display().to_string(),
            command: "rg -n -F 'resident manager' src | head -5".to_owned(),
        }
        .into_request()
        .expect("managed rg should map");
        assert_eq!(request.root, Some(test_root()));
        assert_eq!(request.query.as_deref(), Some("resident manager"));
        assert_eq!(request.rg_paths, [PathBuf::from("src")]);
        assert_eq!(request.limit, Some(5));
        assert!(request.rg_options.fixed_strings);

        assert!(
            RgInput {
                root: test_root().display().to_string(),
                command: "rg needle ../secret".to_owned(),
            }
            .into_request()
            .is_err()
        );
        assert!(
            RgInput {
                root: test_root().display().to_string(),
                command: "rg $(whoami)".to_owned(),
            }
            .into_request()
            .is_err()
        );
    }

    #[test]
    fn rejects_relative_roots_and_empty_queries() {
        let mut relative = input();
        relative.root = PathBuf::from("workspace").display().to_string();
        assert!(relative.into_request().is_err());

        let mut empty = input();
        empty.query = Some("  ".to_owned());
        empty.fts = None;
        assert!(empty.into_request().is_err());
    }

    fn index_input() -> IndexInput {
        IndexInput {
            root: test_root().display().to_string(),
            api_key: None,
            device: None,
            endpoint: None,
            drop: None,
            embedding: Some("potion-base-8M".to_owned()),
            rebuild: Some(false),
            reset_paths: None,
            globs: Some(PathListInput::One("*.rs".to_owned())),
            insensitive_globs: None,
            file_types: None,
            excluded_file_types: None,
            hidden: None,
            no_ignore: None,
            ignore_files: None,
            max_depth: None,
            max_file_size_bytes: None,
            follow: None,
            embedding_concurrency: Some(2),
            debug: Some(true),
            wait: Some(true),
        }
    }
}
