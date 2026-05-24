use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::env;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use async_channel::unbounded;
use clap::Parser;
use clap::Subcommand;
use clap::ValueEnum;
use codex_config::CloudRequirementsLoader;
use codex_config::ConfigLoadOptions;
use codex_config::Constrained;
use codex_config::LoaderOverrides;
use codex_config::NoopThreadConfigLoader;
use codex_config::config_toml::ConfigToml;
use codex_config::loader::load_config_layers_state;
use codex_config::types::AuthCredentialsStoreMode;
use codex_exec_server::EnvironmentManager;
use codex_exec_server::ExecServerRuntimePaths;
use codex_exec_server::LOCAL_FS;
use codex_features::Feature;
use codex_features::FeatureConfigSource;
use codex_features::FeatureOverrides;
use codex_features::FeatureToml;
use codex_features::Features;
use codex_login::AuthManager;
use codex_login::AuthManagerConfig;
use codex_login::CodexAuth;
use codex_mcp::McpConfig;
use codex_mcp::McpConnectionManager;
use codex_mcp::McpRuntimeContext;
use codex_mcp::codex_apps_tools_cache_key;
use codex_mcp::compute_auth_statuses;
use codex_mcp::effective_mcp_servers;
use codex_mcp::host_owned_codex_apps_enabled;
use codex_mcp::tool_plugin_provenance;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::McpAuthStatus;
use codex_utils_absolute_path::AbsolutePathBufGuard;
use codex_utils_cli::CliConfigOverrides;
use codex_utils_home_dir::find_codex_home;
use rmcp::ServerHandler;
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::model::CallToolResult as RmcpCallToolResult;
use rmcp::model::ElicitationCapability;
use rmcp::model::ErrorData as McpError;
use rmcp::model::FormElicitationCapability;
use rmcp::model::Implementation;
use rmcp::model::ListToolsResult;
use rmcp::model::PaginatedRequestParams;
use rmcp::model::ReadResourceRequestParams;
use rmcp::model::ServerCapabilities;
use rmcp::model::ServerInfo;
use rmcp::model::Tool;
use rmcp::model::UrlElicitationCapability;
use rmcp::service::RequestContext;
use rmcp::service::RoleServer;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Parser)]
#[command(name = "cxporter")]
#[command(about = "Direct MCP access through Codex-owned config and auth.")]
struct Cli {
    #[clap(flatten)]
    config_overrides: CliConfigOverrides,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = "List configured MCP servers and tools.")]
    List {
        #[arg(long, help = "Filter to one MCP server.")]
        server: Option<String>,

        #[arg(
            long,
            value_enum,
            default_value = "tools",
            help = "Output tools only or full MCP metadata."
        )]
        detail: Detail,
    },
    #[command(about = "Call a tool through Codex-managed MCP transports.")]
    Call {
        #[arg(
            long,
            help = "Skip local required-property checks and send arguments directly."
        )]
        no_preflight: bool,
        #[arg(help = "MCP server name, for example codex_apps.")]
        server: String,
        #[arg(help = "Tool name.")]
        tool: String,
        #[arg(
            default_value = "{}",
            help = "JSON object passed as MCP tool arguments."
        )]
        arguments_json: String,
    },
    #[command(about = "Show the JSON input schema for a tool.")]
    Schema {
        #[arg(help = "MCP server name, for example codex_apps.")]
        server: String,
        #[arg(help = "Tool name.")]
        tool: String,
    },
    #[command(about = "Read a resource from a configured MCP server.")]
    Resource {
        #[arg(help = "MCP server name.")]
        server: String,
        #[arg(help = "Resource URI.")]
        uri: String,
    },
    #[command(about = "List Codex apps/connectors exposed by the internal codex_apps MCP server.")]
    Apps {
        #[arg(long, help = "Bypass app caches.")]
        force: bool,
    },
    #[command(about = "Run as a stdio MCP server that proxies Codex-authenticated MCP tools.")]
    Serve {
        #[arg(
            long,
            value_name = "SERVER",
            help = "Only export tools from this MCP server. Repeat to include multiple servers."
        )]
        server: Vec<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Detail {
    Full,
    Tools,
}

struct CodexState {
    mcp_config: McpConfig,
    auth: Option<CodexAuth>,
    runtime_context: McpRuntimeContext,
}

#[derive(Clone, Debug)]
struct RuntimeConfig {
    codex_home: PathBuf,
    cwd: PathBuf,
    codex_self_exe: Option<PathBuf>,
    codex_linux_sandbox_exe: Option<PathBuf>,
    cli_auth_credentials_store_mode: AuthCredentialsStoreMode,
    forced_chatgpt_workspace_id: Option<Vec<String>>,
    chatgpt_base_url: String,
}

impl AuthManagerConfig for RuntimeConfig {
    fn codex_home(&self) -> PathBuf {
        self.codex_home.clone()
    }

    fn cli_auth_credentials_store_mode(&self) -> AuthCredentialsStoreMode {
        self.cli_auth_credentials_store_mode
    }

    fn forced_chatgpt_workspace_id(&self) -> Option<Vec<String>> {
        self.forced_chatgpt_workspace_id.clone()
    }

    fn chatgpt_base_url(&self) -> String {
        self.chatgpt_base_url.clone()
    }
}

struct ManagerBundle {
    manager: McpConnectionManager,
    cancel_token: CancellationToken,
    server_names: Vec<String>,
    auth_statuses: HashMap<String, McpAuthStatus>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ServerStatus {
    name: String,
    tools: BTreeMap<String, Value>,
    resources: Vec<Value>,
    resource_templates: Vec<Value>,
    auth_status: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AppConnector {
    id: String,
    name: String,
    description: Option<String>,
    link_id: Option<String>,
    tools: Vec<String>,
    is_accessible: bool,
    is_enabled: bool,
}

#[derive(Debug)]
struct ExportedToolRoute {
    server: String,
    tool: String,
}

#[derive(Debug)]
struct ExportedToolIndex {
    tools: Vec<Tool>,
    tools_by_name: HashMap<String, Tool>,
    routes: HashMap<String, ExportedToolRoute>,
}

struct CxporterMcpServer {
    bundle: Arc<Mutex<Option<ManagerBundle>>>,
    index: Arc<ExportedToolIndex>,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("cxporter: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let state = load_state(&cli.config_overrides).await?;

    match cli.command {
        Command::List { server, detail } => {
            let statuses = list_servers(&state, server.as_deref(), detail).await?;
            print_json(&statuses)?;
        }
        Command::Call {
            no_preflight,
            server,
            tool,
            arguments_json,
        } => {
            let arguments = parse_arguments_json(&arguments_json)?;
            let result = call_tool(&state, &server, &tool, arguments, !no_preflight).await?;
            print_json(&result)?;
        }
        Command::Schema { server, tool } => {
            let schema = tool_schema(&state, &server, &tool).await?;
            print_json(&schema)?;
        }
        Command::Resource { server, uri } => {
            let result = read_resource(&state, &server, &uri).await?;
            print_json(&result)?;
        }
        Command::Apps { force } => {
            let connectors = list_apps(&state, force).await?;
            print_json(&connectors)?;
        }
        Command::Serve { server } => {
            serve_mcp_server(&state, &server).await?;
        }
    }

    Ok(())
}

async fn load_state(config_overrides: &CliConfigOverrides) -> Result<CodexState> {
    let overrides = config_overrides
        .parse_overrides()
        .map_err(|error| anyhow!("failed to parse -c/--config override: {error}"))?;
    let codex_home = find_codex_home().context("failed to resolve Codex home")?;
    let config_layer_stack = load_config_layers_state(
        LOCAL_FS.as_ref(),
        &codex_home,
        /*cwd*/ None,
        &overrides,
        ConfigLoadOptions {
            loader_overrides: LoaderOverrides::default(),
            strict_config: false,
        },
        CloudRequirementsLoader::default(),
        &NoopThreadConfigLoader,
    )
    .await
    .context("failed to load Codex config layers")?;
    let config_toml = deserialize_config_toml(config_layer_stack.effective_config(), &codex_home)?;
    let runtime_config = runtime_config_from_toml(&config_toml, codex_home.to_path_buf())?;
    let mcp_config = mcp_config_from_toml(&config_toml, &runtime_config);
    let auth_manager =
        AuthManager::shared_from_config(&runtime_config, /*enable_codex_api_key_env*/ false).await;
    let auth = auth_manager.auth().await;
    let runtime_context = load_runtime_context(&runtime_config).await?;

    Ok(CodexState {
        mcp_config,
        auth,
        runtime_context,
    })
}

fn deserialize_config_toml(
    root_value: codex_config::TomlValue,
    codex_home: &Path,
) -> Result<ConfigToml> {
    let _guard = AbsolutePathBufGuard::new(codex_home);
    root_value
        .try_into()
        .context("failed to deserialize Codex config.toml")
}

fn runtime_config_from_toml(config: &ConfigToml, codex_home: PathBuf) -> Result<RuntimeConfig> {
    let cwd = env::current_dir().context("failed to resolve current working directory")?;
    Ok(RuntimeConfig {
        codex_home,
        cwd,
        codex_self_exe: resolve_codex_self_exe(),
        codex_linux_sandbox_exe: None,
        cli_auth_credentials_store_mode: config.cli_auth_credentials_store.unwrap_or_default(),
        forced_chatgpt_workspace_id: config
            .forced_chatgpt_workspace_id
            .clone()
            .map(codex_config::config_toml::ForcedChatgptWorkspaceIds::into_vec),
        chatgpt_base_url: config
            .chatgpt_base_url
            .clone()
            .unwrap_or_else(|| "https://chatgpt.com/backend-api/".to_string()),
    })
}

fn mcp_config_from_toml(config: &ConfigToml, runtime: &RuntimeConfig) -> McpConfig {
    let features = Features::from_sources(
        FeatureConfigSource {
            features: config.features.as_ref(),
            experimental_use_unified_exec_tool: config.experimental_use_unified_exec_tool,
        },
        FeatureConfigSource::default(),
        FeatureOverrides::default(),
    );

    McpConfig {
        chatgpt_base_url: runtime.chatgpt_base_url.clone(),
        apps_mcp_path_override: apps_mcp_path_override(config),
        apps_mcp_product_sku: config.apps_mcp_product_sku.clone(),
        codex_home: runtime.codex_home.clone(),
        mcp_oauth_credentials_store_mode: config.mcp_oauth_credentials_store.unwrap_or_default(),
        mcp_oauth_callback_port: config.mcp_oauth_callback_port,
        mcp_oauth_callback_url: config.mcp_oauth_callback_url.clone(),
        skill_mcp_dependency_install_enabled: features.enabled(Feature::SkillMcpDependencyInstall),
        approval_policy: Constrained::allow_any(
            config.approval_policy.unwrap_or(AskForApproval::OnRequest),
        ),
        codex_linux_sandbox_exe: runtime.codex_linux_sandbox_exe.clone(),
        use_legacy_landlock: features.use_legacy_landlock(),
        apps_enabled: features.enabled(Feature::Apps),
        client_elicitation_capability: client_elicitation_capability(&features),
        configured_mcp_servers: config.mcp_servers.clone(),
        plugin_ids_by_mcp_server_name: HashMap::new(),
        plugin_capability_summaries: Vec::new(),
    }
}

fn apps_mcp_path_override(config: &ConfigToml) -> Option<String> {
    match config.features.as_ref()?.apps_mcp_path_override.as_ref()? {
        FeatureToml::Enabled(_) => None,
        FeatureToml::Config(config) => config.path.clone(),
    }
}

fn client_elicitation_capability(features: &Features) -> ElicitationCapability {
    if features.enabled(Feature::AuthElicitation) {
        ElicitationCapability {
            form: Some(FormElicitationCapability::default()),
            url: Some(UrlElicitationCapability::default()),
        }
    } else {
        ElicitationCapability::default()
    }
}

fn resolve_codex_self_exe() -> Option<PathBuf> {
    env::var_os("CXPORTER_CODEX_SELF_EXE")
        .map(PathBuf::from)
        .or_else(|| find_on_path("codex"))
        .or_else(|| env::current_exe().ok())
}

fn find_on_path(binary_name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|directory| directory.join(binary_name))
        .find(|candidate| candidate.is_file())
}

async fn load_runtime_context(config: &RuntimeConfig) -> Result<McpRuntimeContext> {
    let local_runtime_paths = ExecServerRuntimePaths::from_optional_paths(
        config.codex_self_exe.clone(),
        config.codex_linux_sandbox_exe.clone(),
    )?;
    let environment_manager =
        EnvironmentManager::from_codex_home(config.codex_home.clone(), Some(local_runtime_paths))
            .await?;
    Ok(McpRuntimeContext::new(
        Arc::new(environment_manager),
        config.cwd.clone(),
    ))
}

async fn manager_bundle(state: &CodexState, server: Option<&str>) -> Result<ManagerBundle> {
    let servers = server
        .map(|server_name| vec![server_name.to_string()])
        .unwrap_or_default();
    manager_bundle_for_servers(state, &servers).await
}

async fn manager_bundle_for_servers(
    state: &CodexState,
    servers: &[String],
) -> Result<ManagerBundle> {
    let mut mcp_servers = effective_mcp_servers(&state.mcp_config, state.auth.as_ref());
    if !servers.is_empty() {
        let wanted_servers = servers.iter().cloned().collect::<HashSet<_>>();
        for server_name in &wanted_servers {
            if !mcp_servers.contains_key(server_name) {
                bail!("unknown MCP server '{server_name}'");
            }
        }
        mcp_servers.retain(|name, _| wanted_servers.contains(name));
    }

    let mut server_names = mcp_servers.keys().cloned().collect::<Vec<_>>();
    server_names.sort();

    let auth_entries = compute_auth_statuses(
        mcp_servers.iter(),
        state.mcp_config.mcp_oauth_credentials_store_mode,
        state.auth.as_ref(),
    )
    .await;
    let auth_statuses = auth_entries
        .iter()
        .map(|(name, entry)| (name.clone(), entry.auth_status))
        .collect::<HashMap<_, _>>();

    let (tx_event, rx_event) = unbounded();
    drop(rx_event);
    let (manager, cancel_token) = McpConnectionManager::new(
        &mcp_servers,
        state.mcp_config.mcp_oauth_credentials_store_mode,
        auth_entries,
        &state.mcp_config.approval_policy,
        "cxporter".to_string(),
        tx_event,
        PermissionProfile::default(),
        state.runtime_context.clone(),
        state.mcp_config.codex_home.clone(),
        codex_apps_tools_cache_key(state.auth.as_ref()),
        host_owned_codex_apps_enabled(&state.mcp_config, state.auth.as_ref()),
        state.mcp_config.client_elicitation_capability.clone(),
        tool_plugin_provenance(&state.mcp_config),
        state.auth.as_ref(),
        /*elicitation_reviewer*/ None,
    )
    .await;

    Ok(ManagerBundle {
        manager,
        cancel_token,
        server_names,
        auth_statuses,
    })
}

async fn serve_mcp_server(state: &CodexState, servers: &[String]) -> Result<()> {
    let bundle = manager_bundle_for_servers(state, servers).await?;
    let index = build_exported_tool_index(&bundle).await?;
    let bundle = Arc::new(Mutex::new(Some(bundle)));
    let server = CxporterMcpServer {
        bundle: Arc::clone(&bundle),
        index: Arc::new(index),
    };
    let service = match server
        .serve_with_ct(rmcp::transport::stdio(), CancellationToken::new())
        .await
    {
        Ok(service) => service,
        Err(error) => {
            shutdown_shared_bundle(bundle).await;
            return Err(error).context("failed to initialize cxporter MCP stdio server");
        }
    };
    let wait_result = service.waiting().await;
    shutdown_shared_bundle(bundle).await;
    wait_result.context("cxporter MCP server task failed")?;
    Ok(())
}

async fn shutdown_shared_bundle(bundle: Arc<Mutex<Option<ManagerBundle>>>) {
    if let Some(bundle) = bundle.lock().await.take() {
        shutdown(bundle).await;
    }
}

async fn build_exported_tool_index(bundle: &ManagerBundle) -> Result<ExportedToolIndex> {
    let mut tool_infos = bundle.manager.list_all_tools().await;
    tool_infos.sort_by(|left, right| {
        let left_name = exported_tool_base_name(
            &left.server_name,
            left.connector_name.as_deref(),
            &left.tool.name,
        );
        let right_name = exported_tool_base_name(
            &right.server_name,
            right.connector_name.as_deref(),
            &right.tool.name,
        );
        left_name
            .cmp(&right_name)
            .then_with(|| left.server_name.cmp(&right.server_name))
            .then_with(|| left.tool.name.cmp(&right.tool.name))
    });

    let mut tools = Vec::new();
    let mut tools_by_name = HashMap::new();
    let mut routes = HashMap::new();
    let mut used_names = HashMap::<String, usize>::new();

    for tool_info in tool_infos {
        let base_name = exported_tool_base_name(
            &tool_info.server_name,
            tool_info.connector_name.as_deref(),
            &tool_info.tool.name,
        );
        let exported_name = unique_exported_tool_name(&base_name, &mut used_names);
        let raw_tool_name = tool_info.tool.name.to_string();
        let mut exported_tool = tool_info.tool.clone();
        exported_tool.name = exported_name.clone().into();
        routes.insert(
            exported_name.clone(),
            ExportedToolRoute {
                server: tool_info.server_name,
                tool: raw_tool_name,
            },
        );
        tools_by_name.insert(exported_name, exported_tool.clone());
        tools.push(exported_tool);
    }

    tools.sort_by(|left, right| left.name.cmp(&right.name));

    Ok(ExportedToolIndex {
        tools,
        tools_by_name,
        routes,
    })
}

impl ServerHandler for CxporterMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "cxporter".to_string(),
                title: Some("cxporter".to_string()),
                version: env!("CARGO_PKG_VERSION").to_string(),
                description: Some(
                    "Proxy for Codex-authenticated MCP tools and codex_apps connectors."
                        .to_string(),
                ),
                icons: None,
                website_url: None,
            },
            instructions: Some(
                "Tools are exposed as <mcp_server>.<connector_or_namespace>.<tool>, for example codex_apps.github.fetch_pr."
                    .to_string(),
            ),
            ..ServerInfo::default()
        }
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(self.index.tools.clone()))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.index.tools_by_name.get(name).cloned()
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<RmcpCallToolResult, McpError> {
        let route = self
            .index
            .routes
            .get(request.name.as_ref())
            .ok_or_else(|| {
                McpError::invalid_params(
                    format!("unknown cxporter-exported tool '{}'", request.name),
                    None,
                )
            })?;
        let arguments = Some(Value::Object(request.arguments.unwrap_or_default()));
        let meta = request.meta.map(|meta| Value::Object(meta.0));
        let bundle_guard = self.bundle.lock().await;
        let bundle = bundle_guard.as_ref().ok_or_else(|| {
            McpError::internal_error("cxporter MCP server is shutting down", None)
        })?;
        let result = bundle
            .manager
            .call_tool(&route.server, &route.tool, arguments, meta)
            .await
            .map_err(|error| {
                McpError::internal_error(
                    format!(
                        "tool call failed for `{}/{}`: {error:#}",
                        route.server, route.tool
                    ),
                    None,
                )
            })?;
        codex_call_tool_result_to_rmcp(result)
    }
}

fn codex_call_tool_result_to_rmcp(
    result: codex_protocol::mcp::CallToolResult,
) -> Result<RmcpCallToolResult, McpError> {
    let value = serde_json::to_value(result).map_err(|error| {
        McpError::internal_error(
            format!("failed to serialize Codex tool result: {error}"),
            None,
        )
    })?;
    serde_json::from_value(value).map_err(|error| {
        McpError::internal_error(format!("failed to decode Codex tool result: {error}"), None)
    })
}

fn exported_tool_base_name(
    server_name: &str,
    connector_name: Option<&str>,
    tool_name: &str,
) -> String {
    let server = sanitize_export_component(server_name);
    let tool = sanitize_export_component(tool_name);
    if let Some(connector) = connector_name
        .map(sanitize_export_component)
        .filter(|connector| !connector.is_empty())
    {
        let stripped_tool = strip_export_prefix(&tool, &connector);
        format!("{server}.{connector}.{stripped_tool}")
    } else {
        format!("{server}.{tool}")
    }
}

fn unique_exported_tool_name(base_name: &str, used_names: &mut HashMap<String, usize>) -> String {
    let count = used_names.entry(base_name.to_string()).or_insert(0);
    *count += 1;
    if *count == 1 {
        base_name.to_string()
    } else {
        format!("{base_name}.{}", *count)
    }
}

fn strip_export_prefix(tool: &str, prefix: &str) -> String {
    for separator in ["_", "-"] {
        let prefix = format!("{prefix}{separator}");
        if let Some(stripped) = tool.strip_prefix(&prefix)
            && !stripped.is_empty()
        {
            return stripped.to_string();
        }
    }
    tool.to_string()
}

fn sanitize_export_component(value: &str) -> String {
    let mut result = String::new();
    let mut previous_was_separator = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            result.push(character.to_ascii_lowercase());
            previous_was_separator = false;
        } else if !previous_was_separator {
            result.push('_');
            previous_was_separator = true;
        }
    }
    let result = result.trim_matches('_');
    if result.is_empty() {
        "unnamed".to_string()
    } else {
        result.to_string()
    }
}

async fn list_servers(
    state: &CodexState,
    server: Option<&str>,
    detail: Detail,
) -> Result<Vec<ServerStatus>> {
    let bundle = manager_bundle(state, server).await?;
    let mut tools_by_server: HashMap<String, BTreeMap<String, Value>> = HashMap::new();
    for tool_info in bundle.manager.list_all_tools().await {
        let tool_name = tool_info.tool.name.to_string();
        let tool_json = serde_json::to_value(tool_info.tool)?;
        tools_by_server
            .entry(tool_info.server_name)
            .or_default()
            .insert(tool_name, tool_json);
    }

    let (resources_by_server, templates_by_server) = if detail == Detail::Full {
        (
            bundle.manager.list_all_resources().await,
            bundle.manager.list_all_resource_templates().await,
        )
    } else {
        (HashMap::new(), HashMap::new())
    };

    let statuses = bundle
        .server_names
        .iter()
        .map(|name| ServerStatus {
            name: name.clone(),
            tools: tools_by_server.remove(name).unwrap_or_default(),
            resources: values_to_json(resources_by_server.get(name)),
            resource_templates: values_to_json(templates_by_server.get(name)),
            auth_status: auth_status_to_wire(
                *bundle
                    .auth_statuses
                    .get(name)
                    .unwrap_or(&McpAuthStatus::Unsupported),
            ),
        })
        .collect::<Vec<_>>();

    shutdown(bundle).await;
    Ok(statuses)
}

async fn call_tool(
    state: &CodexState,
    server: &str,
    tool: &str,
    arguments: Value,
    preflight: bool,
) -> Result<codex_protocol::mcp::CallToolResult> {
    let bundle = manager_bundle(state, Some(server)).await?;
    if preflight {
        let schema = tool_schema_from_bundle(&bundle, server, tool).await?;
        validate_required_arguments(server, tool, &arguments, &schema)?;
    }
    let result = bundle
        .manager
        .call_tool(server, tool, Some(arguments), /*meta*/ None)
        .await;
    shutdown(bundle).await;
    result
}

async fn read_resource(
    state: &CodexState,
    server: &str,
    uri: &str,
) -> Result<rmcp::model::ReadResourceResult> {
    let bundle = manager_bundle(state, Some(server)).await?;
    let result = bundle
        .manager
        .read_resource(
            server,
            ReadResourceRequestParams {
                meta: None,
                uri: uri.to_string(),
            },
        )
        .await;
    shutdown(bundle).await;
    result
}

async fn tool_schema(state: &CodexState, server: &str, tool: &str) -> Result<Value> {
    let bundle = manager_bundle(state, Some(server)).await?;
    let schema = tool_schema_from_bundle(&bundle, server, tool).await;
    shutdown(bundle).await;
    schema
}

async fn tool_schema_from_bundle(
    bundle: &ManagerBundle,
    server: &str,
    tool: &str,
) -> Result<Value> {
    let mut schema = None;

    for tool_info in bundle.manager.list_all_tools().await {
        if tool_info.server_name == server && tool_info.tool.name == tool {
            let tool_json = serde_json::to_value(tool_info.tool)?;
            schema = tool_json.get("inputSchema").cloned();
            break;
        }
    }

    schema.ok_or_else(|| anyhow!("unknown tool '{tool}' on MCP server '{server}'"))
}

fn validate_required_arguments(
    server: &str,
    tool: &str,
    arguments: &Value,
    schema: &Value,
) -> Result<()> {
    let Some(arguments_object) = arguments.as_object() else {
        bail!("tool arguments must be a JSON object");
    };

    let required_properties = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    let missing_properties = required_properties
        .iter()
        .copied()
        .filter(|property| !arguments_object.contains_key(*property))
        .collect::<Vec<_>>();

    if missing_properties.is_empty() {
        return Ok(());
    }

    let known_properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|properties| properties.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    let unknown_properties = if known_properties.is_empty() {
        Vec::new()
    } else {
        arguments_object
            .keys()
            .filter(|property| !known_properties.contains(property))
            .cloned()
            .collect::<Vec<_>>()
    };

    let mut message = format!(
        "arguments do not satisfy {server}/{tool} input schema: missing required properties: {}",
        missing_properties.join(", ")
    );
    if !unknown_properties.is_empty() {
        message.push_str(&format!(
            "; unknown provided properties: {}",
            unknown_properties.join(", ")
        ));
    }
    if !known_properties.is_empty() {
        message.push_str(&format!(
            "; known properties: {}",
            known_properties.join(", ")
        ));
    }
    message.push_str(&format!(
        "; inspect schema with: cxporter schema {server} {tool}; bypass this local check with --no-preflight"
    ));
    bail!(message)
}

async fn list_apps(state: &CodexState, force: bool) -> Result<Vec<AppConnector>> {
    let bundle = manager_bundle(state, Some("codex_apps")).await?;
    let tool_infos = if force {
        match bundle.manager.hard_refresh_codex_apps_tools_cache().await {
            Ok(tools) => tools,
            Err(_) => bundle.manager.list_all_tools().await,
        }
    } else {
        bundle.manager.list_all_tools().await
    };

    let mut connectors = BTreeMap::<String, AppConnector>::new();
    for tool_info in tool_infos {
        let tool_name = tool_info.tool.name.to_string();
        let tool_json = serde_json::to_value(tool_info.tool)?;
        let Some(metadata) = tool_json.get("_meta").and_then(Value::as_object) else {
            continue;
        };
        let Some(id) = metadata_string(metadata, "connector_id") else {
            continue;
        };
        let name = metadata_string(metadata, "connector_name").unwrap_or_else(|| id.clone());
        let description = metadata_string(metadata, "connector_description");
        let link_id = metadata_string(metadata, "link_id");
        let connector = connectors
            .entry(id.clone())
            .or_insert_with(|| AppConnector {
                id,
                name,
                description,
                link_id,
                tools: Vec::new(),
                is_accessible: true,
                is_enabled: true,
            });
        connector.tools.push(tool_name);
    }

    for connector in connectors.values_mut() {
        connector.tools.sort();
        connector.tools.dedup();
    }

    shutdown(bundle).await;
    Ok(connectors.into_values().collect())
}

async fn shutdown(mut bundle: ManagerBundle) {
    bundle.cancel_token.cancel();
    bundle.manager.shutdown().await;
}

fn parse_arguments_json(arguments_json: &str) -> Result<Value> {
    let value: Value = serde_json::from_str(arguments_json)
        .with_context(|| "tool arguments must be a JSON value")?;
    if !value.is_object() {
        bail!("tool arguments must be a JSON object");
    }
    Ok(value)
}

fn values_to_json<T: Serialize>(values: Option<&Vec<T>>) -> Vec<Value> {
    values
        .into_iter()
        .flatten()
        .filter_map(|value| serde_json::to_value(value).ok())
        .collect()
}

fn metadata_string(metadata: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    metadata
        .get(key)
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn auth_status_to_wire(status: McpAuthStatus) -> &'static str {
    match status {
        McpAuthStatus::Unsupported => "unsupported",
        McpAuthStatus::NotLoggedIn => "notLoggedIn",
        McpAuthStatus::BearerToken => "bearerToken",
        McpAuthStatus::OAuth => "oAuth",
    }
}

fn print_json<T: Serialize>(value: &T) -> Result<()> {
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    serde_json::to_writer_pretty(&mut handle, value)?;
    writeln!(&mut handle)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exported_tool_base_name_groups_codex_apps_by_connector() {
        assert_eq!(
            exported_tool_base_name("codex_apps", Some("GitHub"), "github_fetch_pr"),
            "codex_apps.github.fetch_pr"
        );
    }

    #[test]
    fn exported_tool_base_name_sanitizes_connector_names() {
        assert_eq!(
            exported_tool_base_name("codex_apps", Some("Google Drive"), "google drive_search"),
            "codex_apps.google_drive.search"
        );
    }

    #[test]
    fn exported_tool_base_name_keeps_registered_servers_flat() {
        assert_eq!(
            exported_tool_base_name("my-server", None, "lookup.item"),
            "my_server.lookup_item"
        );
    }
}
