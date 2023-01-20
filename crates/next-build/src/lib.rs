use std::{
    collections::HashSet, env::current_dir, net::SocketAddr, path::MAIN_SEPARATOR, sync::Arc,
};

use anyhow::{anyhow, Result};
use next_core::{
    create_app_source, create_page_source, create_web_entry_source, env::load_env,
    manifest::DevManifestContentSource, next_config::load_next_config,
    next_image::NextImageContentSourceVc, source_map::NextSourceMapTraceContentSourceVc,
};
use turbo_tasks::{RawVc, TransientInstance, TransientValue, TurboTasks, Value};
use turbo_tasks_fs::{DiskFileSystemVc, FileSystemVc};
use turbo_tasks_memory::MemoryBackend;
use turbopack_cli_utils::issue::{ConsoleUi, ConsoleUiVc, LogOptions};
use turbopack_core::{
    environment::ServerAddr,
    issue::IssueSeverity,
    resolve::{parse::RequestVc, pattern::QueryMapVc},
    server_fs::ServerFileSystemVc,
};
use turbopack_dev_server::{
    introspect::IntrospectionSource,
    source::{
        combined::CombinedContentSourceVc, router::RouterContentSource,
        source_maps::SourceMapContentSourceVc, static_assets::StaticAssetsContentSourceVc,
        ContentSourceVc,
    },
};
use turbopack_node::execution_context::ExecutionContextVc;

pub struct NextBuildBuilder {
    turbo_tasks: Arc<TurboTasks<MemoryBackend>>,
    project_dir: String,
    root_dir: String,
    entry: Vec<EntryRequest>,
    show_all: bool,
    log_detail: bool,
    log_level: IssueSeverity,
    browserslist_query: String,
    server_addr: SocketAddr,
}

impl NextBuildBuilder {
    pub fn new(
        turbo_tasks: Arc<TurboTasks<MemoryBackend>>,
        project_dir: String,
        root_dir: String,
        server_addr: SocketAddr,
    ) -> Self {
        Self {
            turbo_tasks,
            project_dir,
            root_dir,
            entry: vec![],
            show_all: false,
            log_detail: false,
            log_level: IssueSeverity::Error,
            browserslist_query: "last 1 Chrome versions, last 1 Firefox versions, last 1 Safari \
                                 versions, last 1 Edge versions"
                .to_owned(),
            server_addr,
        }
    }

    pub async fn build(self) -> Result<()> {
        let log_options = LogOptions {
            current_dir: current_dir().unwrap(),
            show_all: self.show_all,
            log_detail: self.log_detail,
            log_level: self.log_level,
        };
        let console_ui = Arc::new(ConsoleUi::new(log_options));
        source(
            self.root_dir,
            self.project_dir,
            Arc::new(self.entry).into(),
            false,
            self.turbo_tasks.into(),
            console_ui.into(),
            self.browserslist_query,
            Arc::new(self.server_addr).into(),
        );
        Ok(())
    }
}

#[derive(Clone)]
pub enum EntryRequest {
    Relative(String),
    Module(String, String),
}

#[allow(clippy::too_many_arguments)]
#[turbo_tasks::function]
async fn source(
    root_dir: String,
    project_dir: String,
    entry_requests: TransientInstance<Vec<EntryRequest>>,
    eager_compile: bool,
    turbo_tasks: TransientInstance<TurboTasks<MemoryBackend>>,
    console_ui: TransientInstance<ConsoleUi>,
    browserslist_query: String,
    server_addr: TransientInstance<SocketAddr>,
) -> Result<ContentSourceVc> {
    let console_ui = (*console_ui).clone().cell();
    let output_fs = output_fs(&project_dir, console_ui);
    let fs = project_fs(&root_dir, console_ui);
    let project_relative = project_dir.strip_prefix(&root_dir).unwrap();
    let project_relative = project_relative
        .strip_prefix(MAIN_SEPARATOR)
        .unwrap_or(project_relative)
        .replace(MAIN_SEPARATOR, "/");
    let project_path = fs.root().join(&project_relative);

    let env = load_env(project_path);
    let build_output_root = output_fs.root().join(".next/build");

    let execution_context = ExecutionContextVc::new(project_path, build_output_root);

    let next_config = load_next_config(execution_context.join("next_config"));

    let output_root = output_fs.root().join(".next/server");
    let server_addr = ServerAddr::new(*server_addr).cell();

    let dev_server_fs = ServerFileSystemVc::new().as_file_system();
    let dev_server_root = dev_server_fs.root();
    let entry_requests = entry_requests
        .iter()
        .map(|r| match r {
            EntryRequest::Relative(p) => RequestVc::relative(Value::new(p.clone().into()), false),
            EntryRequest::Module(m, p) => {
                RequestVc::module(m.clone(), Value::new(p.clone().into()), QueryMapVc::none())
            }
        })
        .collect();

    let web_source = create_web_entry_source(
        project_path,
        execution_context,
        entry_requests,
        dev_server_root,
        env,
        eager_compile,
        &browserslist_query,
        next_config,
    );
    let page_source = create_page_source(
        project_path,
        execution_context,
        output_root.join("pages"),
        dev_server_root,
        env,
        &browserslist_query,
        next_config,
        server_addr,
    );
    let app_source = create_app_source(
        project_path,
        execution_context,
        output_root.join("app"),
        dev_server_root,
        env,
        &browserslist_query,
        next_config,
        server_addr,
    );
    let static_source =
        StaticAssetsContentSourceVc::new(String::new(), project_path.join("public")).into();
    let manifest_source = DevManifestContentSource {
        page_roots: vec![app_source, page_source],
    }
    .cell()
    .into();
    let main_source = CombinedContentSourceVc::new(vec![
        manifest_source,
        static_source,
        app_source,
        page_source,
        web_source,
    ]);
    let introspect = IntrospectionSource {
        roots: HashSet::from([main_source.into()]),
    }
    .cell()
    .into();
    let main_source = main_source.into();
    let source_maps = SourceMapContentSourceVc::new(main_source).into();
    let source_map_trace = NextSourceMapTraceContentSourceVc::new(main_source).into();
    let img_source = NextImageContentSourceVc::new(
        CombinedContentSourceVc::new(vec![static_source, page_source]).into(),
    )
    .into();
    let source = RouterContentSource {
        routes: vec![
            ("__turbopack__/".to_string(), introspect),
            (
                "__nextjs_original-stack-frame".to_string(),
                source_map_trace,
            ),
            // TODO: Load path from next.config.js
            ("_next/image".to_string(), img_source),
            ("__turbopack_sourcemap__/".to_string(), source_maps),
        ],
        fallback: main_source,
    }
    .cell()
    .into();

    handle_issues(dev_server_fs, console_ui).await?;
    handle_issues(web_source, console_ui).await?;
    handle_issues(page_source, console_ui).await?;

    Ok(source)
}

#[turbo_tasks::function]
async fn project_fs(project_dir: &str, console_ui: ConsoleUiVc) -> Result<FileSystemVc> {
    let disk_fs = DiskFileSystemVc::new("project".to_string(), project_dir.to_string());
    handle_issues(disk_fs, console_ui).await?;
    disk_fs.await?.start_watching()?;
    Ok(disk_fs.into())
}

#[turbo_tasks::function]
async fn output_fs(project_dir: &str, console_ui: ConsoleUiVc) -> Result<FileSystemVc> {
    let disk_fs = DiskFileSystemVc::new("output".to_string(), project_dir.to_string());
    handle_issues(disk_fs, console_ui).await?;
    disk_fs.await?.start_watching()?;
    Ok(disk_fs.into())
}

async fn handle_issues<T: Into<RawVc>>(source: T, console_ui: ConsoleUiVc) -> Result<()> {
    let state = console_ui
        .group_and_display_issues(TransientValue::new(source.into()))
        .await?;

    if state.has_fatal {
        Err(anyhow!("Fatal issue(s) occurred"))
    } else {
        Ok(())
    }
}

pub fn register() {
    next_core::register();
    include!(concat!(env!("OUT_DIR"), "/register.rs"));
}
