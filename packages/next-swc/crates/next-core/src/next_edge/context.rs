use anyhow::Result;
use indexmap::IndexMap;
use turbo_tasks::{Value, Vc};
use turbopack_binding::{
    turbo::{tasks_env::EnvMap, tasks_fs::FileSystemPath},
    turbopack::{
        core::{
            compile_time_defines,
            compile_time_info::{
                CompileTimeDefineValue, CompileTimeDefines, CompileTimeInfo, FreeVarReference,
                FreeVarReferences,
            },
            environment::{EdgeWorkerEnvironment, Environment, ExecutionEnvironment, ServerAddr},
            free_var_references,
        },
        dev::DevChunkingContext,
        ecmascript::chunk::EcmascriptChunkingContext,
        node::{debug::should_debug, execution_context::ExecutionContext},
        turbopack::resolve_options_context::ResolveOptionsContext,
    },
};

use crate::{
    mode::NextMode,
    next_client::context::get_client_assets_path,
    next_config::NextConfig,
    next_import_map::get_next_edge_import_map,
    next_server::context::ServerContextType,
    next_shared::resolve::{
        ModuleFeatureReportResolvePlugin, NextSharedRuntimeResolvePlugin,
        UnsupportedModulesResolvePlugin,
    },
    util::foreign_code_context_condition,
};

fn defines(mode: NextMode, define_env: &IndexMap<String, String>) -> CompileTimeDefines {
    let mut defines = compile_time_defines!(
        process.turbopack = true,
        process.env.NEXT_RUNTIME = "edge",
        process.env.NODE_ENV = mode.node_env(),
        process.env.TURBOPACK = true,
    );

    for (k, v) in define_env {
        defines
            .0
            .entry(k.split('.').map(|s| s.to_string()).collect())
            .or_insert_with(|| CompileTimeDefineValue::JSON(v.clone()));
    }

    defines
}

#[turbo_tasks::function]
async fn next_edge_defines(
    mode: NextMode,
    define_env: Vc<EnvMap>,
) -> Result<Vc<CompileTimeDefines>> {
    Ok(defines(mode, &*define_env.await?).cell())
}

#[turbo_tasks::function]
async fn next_edge_free_vars(
    mode: NextMode,
    project_path: Vc<FileSystemPath>,
    define_env: Vc<EnvMap>,
) -> Result<Vc<FreeVarReferences>> {
    let error_message = "A Node.js API is used which is not supported in the Edge Runtime. Learn more: https://nextjs.org/docs/api-reference/edge-runtime".to_string();

    let unsupported_runtime_apis = match mode {
        NextMode::Build => {
            (
                // Mirrors warnForUnsupportedApi in middleware-plugin.ts
                clearImmediate = FreeVarReference::Error(error_message.clone()),
                setImmediate = FreeVarReference::Error(error_message.clone()),
                BroadcastChannel = FreeVarReference::Error(error_message.clone()),
                ByteLengthQueuingStrategy = FreeVarReference::Error(error_message.clone()),
                CompressionStream = FreeVarReference::Error(error_message.clone()),
                CountQueuingStrategy = FreeVarReference::Error(error_message.clone()),
                DecompressionStream = FreeVarReference::Error(error_message.clone()),
                DomException = FreeVarReference::Error(error_message.clone()),
                MessageChannel = FreeVarReference::Error(error_message.clone()),
                MessageEvent = FreeVarReference::Error(error_message.clone()),
                MessagePort = FreeVarReference::Error(error_message.clone()),
                ReadableByteStreamController = FreeVarReference::Error(error_message.clone()),
                ReadableStreamBYOBRequest = FreeVarReference::Error(error_message.clone()),
                ReadableStreamDefaultController = FreeVarReference::Error(error_message.clone()),
                TransformStreamDefaultController = FreeVarReference::Error(error_message.clone()),
                WritableStreamDefaultController = FreeVarReference::Error(error_message.clone()),
                // TODO: Implement warnForUnsupportedProcessApi from
                // middleware-plugin.ts That function implements
                // a check for `process.something` where `something` could be
                // anything in the process object except for `env` as that is
                // excluded.
            )
        }
        NextMode::Development => (),
    };
    Ok(free_var_references!(
        ..defines(mode, &*define_env.await?).into_iter(),
        Buffer = FreeVarReference::EcmaScriptModule {
            request: "next/dist/compiled/buffer".to_string(),
            lookup_path: Some(project_path),
            export: Some("Buffer".to_string()),
        },
        process = FreeVarReference::EcmaScriptModule {
            request: "next/dist/build/polyfills/process".to_string(),
            lookup_path: Some(project_path),
            export: Some("default".to_string()),
        },
        // ..unsupported_runtime_apis
    ))
    .cell()
}

#[turbo_tasks::function]
pub fn get_edge_compile_time_info(
    mode: NextMode,
    project_path: Vc<FileSystemPath>,
    server_addr: Vc<ServerAddr>,
    define_env: Vc<EnvMap>,
) -> Vc<CompileTimeInfo> {
    CompileTimeInfo::builder(Environment::new(Value::new(
        ExecutionEnvironment::EdgeWorker(EdgeWorkerEnvironment { server_addr }.into()),
    )))
    .defines(next_edge_defines(mode, define_env))
    .free_var_references(next_edge_free_vars(mode, project_path, define_env))
    .cell()
}

#[turbo_tasks::function]
pub async fn get_edge_resolve_options_context(
    project_path: Vc<FileSystemPath>,
    ty: Value<ServerContextType>,
    mode: NextMode,
    next_config: Vc<NextConfig>,
    execution_context: Vc<ExecutionContext>,
) -> Result<Vc<ResolveOptionsContext>> {
    let next_edge_import_map =
        get_next_edge_import_map(project_path, ty, mode, next_config, execution_context);

    let ty = ty.into_value();

    // https://github.com/vercel/next.js/blob/bf52c254973d99fed9d71507a2e818af80b8ade7/packages/next/src/build/webpack-config.ts#L96-L102
    let mut custom_conditions = vec![
        mode.node_env().to_string(),
        "edge-light".to_string(),
        "worker".to_string(),
    ];

    match ty {
        ServerContextType::AppRSC { .. } => custom_conditions.push("react-server".to_string()),
        ServerContextType::AppRoute { .. }
        | ServerContextType::Pages { .. }
        | ServerContextType::PagesData { .. }
        | ServerContextType::AppSSR { .. }
        | ServerContextType::Middleware { .. } => {}
    };

    let resolve_options_context = ResolveOptionsContext {
        enable_node_modules: Some(project_path.root().resolve().await?),
        custom_conditions,
        import_map: Some(next_edge_import_map),
        module: true,
        browser: true,
        plugins: vec![
            Vc::upcast(ModuleFeatureReportResolvePlugin::new(project_path)),
            Vc::upcast(UnsupportedModulesResolvePlugin::new(project_path)),
            Vc::upcast(NextSharedRuntimeResolvePlugin::new(project_path)),
        ],
        ..Default::default()
    };

    Ok(ResolveOptionsContext {
        enable_typescript: true,
        enable_react: true,
        rules: vec![(
            foreign_code_context_condition(next_config, project_path).await?,
            resolve_options_context.clone().cell(),
        )],
        ..resolve_options_context
    }
    .cell())
}

#[turbo_tasks::function]
pub fn get_edge_chunking_context(
    project_path: Vc<FileSystemPath>,
    node_root: Vc<FileSystemPath>,
    client_root: Vc<FileSystemPath>,
    environment: Vc<Environment>,
) -> Vc<Box<dyn EcmascriptChunkingContext>> {
    Vc::upcast(
        DevChunkingContext::builder(
            project_path,
            node_root.join("server/edge".to_string()),
            node_root.join("server/edge/chunks".to_string()),
            get_client_assets_path(client_root),
            environment,
        )
        .reference_chunk_source_maps(should_debug("edge"))
        .build(),
    )
}
