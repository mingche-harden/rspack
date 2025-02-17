use std::path::PathBuf;
use std::sync::Arc;

use derivative::Derivative;
use rspack_error::{Diagnostic, IntoTWithDiagnosticArray, Result};
use rspack_sources::BoxSource;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::{
  cache::Cache, BoxDependency, BuildContext, BuildResult, Compilation, CompilerContext,
  CompilerOptions, Context, Module, ModuleFactory, ModuleFactoryCreateData, ModuleFactoryResult,
  ModuleGraph, ModuleGraphModule, ModuleIdentifier, ModuleProfile, Resolve, ResolverFactory,
  SharedPluginDriver, WorkerQueue,
};
use crate::{BoxModule, DependencyId, ExecuteModuleResult, ExportInfo, ExportsInfo, UsageState};

#[derive(Debug)]
pub enum TaskResult {
  Factorize(Box<FactorizeTaskResult>),
  Add(Box<AddTaskResult>),
  Build(Box<BuildTaskResult>),
  ProcessDependencies(Box<ProcessDependenciesResult>),
}

#[async_trait::async_trait]
pub trait WorkerTask {
  async fn run(self) -> Result<TaskResult>;
}

#[derive(Derivative)]
#[derivative(Debug)]
pub struct FactorizeTask {
  pub module_factory: Arc<dyn ModuleFactory>,
  pub original_module_identifier: Option<ModuleIdentifier>,
  pub original_module_source: Option<BoxSource>,
  pub original_module_context: Option<Box<Context>>,
  pub issuer: Option<Box<str>>,
  pub dependency: BoxDependency,
  pub dependencies: Vec<DependencyId>,
  pub is_entry: bool,
  pub resolve_options: Option<Box<Resolve>>,
  pub resolver_factory: Arc<ResolverFactory>,
  pub loader_resolver_factory: Arc<ResolverFactory>,
  pub options: Arc<CompilerOptions>,
  pub lazy_visit_modules: std::collections::HashSet<String>,
  pub plugin_driver: SharedPluginDriver,
  pub cache: Arc<Cache>,
  pub current_profile: Option<Box<ModuleProfile>>,
  pub connect_origin: bool,
  #[derivative(Debug = "ignore")]
  pub callback: Option<ModuleCreationCallback>,
}

/// a struct temporarily used creating ExportsInfo
#[derive(Debug)]
pub struct ExportsInfoRelated {
  pub exports_info: ExportsInfo,
  pub other_exports_info: ExportInfo,
  pub side_effects_info: ExportInfo,
}

#[derive(Derivative)]
#[derivative(Debug)]
pub struct FactorizeTaskResult {
  pub dependency: DependencyId,
  pub original_module_identifier: Option<ModuleIdentifier>,
  /// Result will be available if [crate::ModuleFactory::create] returns `Ok`.
  pub factory_result: Option<ModuleFactoryResult>,
  pub dependencies: Vec<DependencyId>,
  pub is_entry: bool,
  pub current_profile: Option<Box<ModuleProfile>>,
  pub exports_info_related: ExportsInfoRelated,

  pub file_dependencies: HashSet<PathBuf>,
  pub context_dependencies: HashSet<PathBuf>,
  pub missing_dependencies: HashSet<PathBuf>,
  pub diagnostics: Vec<Diagnostic>,
  #[derivative(Debug = "ignore")]
  pub callback: Option<ModuleCreationCallback>,
  pub connect_origin: bool,
}

impl FactorizeTaskResult {
  fn with_factory_result(mut self, factory_result: Option<ModuleFactoryResult>) -> Self {
    self.factory_result = factory_result;
    self
  }

  fn with_diagnostics(mut self, diagnostics: Vec<Diagnostic>) -> Self {
    self.diagnostics = diagnostics;
    self
  }

  fn with_file_dependencies(mut self, files: impl IntoIterator<Item = PathBuf>) -> Self {
    self.file_dependencies = files.into_iter().collect();
    self
  }

  fn with_context_dependencies(mut self, contexts: impl IntoIterator<Item = PathBuf>) -> Self {
    self.context_dependencies = contexts.into_iter().collect();
    self
  }

  fn with_missing_dependencies(mut self, missing: impl IntoIterator<Item = PathBuf>) -> Self {
    self.missing_dependencies = missing.into_iter().collect();
    self
  }
}

#[async_trait::async_trait]
impl WorkerTask for FactorizeTask {
  async fn run(self) -> Result<TaskResult> {
    if let Some(current_profile) = &self.current_profile {
      current_profile.mark_factory_start();
    }
    let dependency = self.dependency;
    let dep_id = *dependency.id();

    let context = if let Some(context) = dependency.get_context() {
      context
    } else if let Some(context) = &self.original_module_context {
      context
    } else {
      &self.options.context
    }
    .clone();

    let other_exports_info = ExportInfo::new(None, UsageState::Unknown, None);
    let side_effects_only_info = ExportInfo::new(
      Some("*side effects only*".into()),
      UsageState::Unknown,
      None,
    );
    let exports_info = ExportsInfo::new(other_exports_info.id, side_effects_only_info.id);
    let factorize_task_result = FactorizeTaskResult {
      dependency: dep_id,
      original_module_identifier: self.original_module_identifier,
      factory_result: None,
      dependencies: self.dependencies,
      is_entry: self.is_entry,
      current_profile: self.current_profile,
      exports_info_related: ExportsInfoRelated {
        exports_info,
        other_exports_info,
        side_effects_info: side_effects_only_info,
      },
      file_dependencies: Default::default(),
      context_dependencies: Default::default(),
      missing_dependencies: Default::default(),
      diagnostics: Default::default(),
      connect_origin: self.connect_origin,
      callback: self.callback,
    };

    // Error and result are not mutually exclusive in webpack module factorization.
    // Rspack puts results that need to be shared in both error and ok in [ModuleFactoryCreateData].
    let mut create_data = ModuleFactoryCreateData {
      resolve_options: self.resolve_options,
      context,
      dependency,
      issuer: self.issuer,
      issuer_identifier: self.original_module_identifier,

      file_dependencies: Default::default(),
      missing_dependencies: Default::default(),
      context_dependencies: Default::default(),
      diagnostics: Default::default(),
    };

    match self.module_factory.create(&mut create_data).await {
      Ok(result) => {
        if let Some(current_profile) = &factorize_task_result.current_profile {
          current_profile.mark_factory_end();
        }
        let diagnostics = create_data.diagnostics.drain(..).collect();
        Ok(TaskResult::Factorize(Box::new(
          factorize_task_result
            .with_factory_result(Some(result))
            .with_diagnostics(diagnostics)
            .with_file_dependencies(create_data.file_dependencies.drain())
            .with_missing_dependencies(create_data.missing_dependencies.drain())
            .with_context_dependencies(create_data.context_dependencies.drain()),
        )))
      }
      Err(mut e) => {
        if let Some(current_profile) = &factorize_task_result.current_profile {
          current_profile.mark_factory_end();
        }
        // Wrap source code if available
        if let Some(s) = self.original_module_source {
          e = e.with_source_code(s.source().to_string());
        }
        // Bail out if `options.bail` set to `true`,
        // which means 'Fail out on the first error instead of tolerating it.'
        if self.options.bail {
          return Err(e);
        }
        let mut diagnostics = Vec::with_capacity(create_data.diagnostics.len() + 1);
        diagnostics.push(e.into());
        diagnostics.append(&mut create_data.diagnostics);
        // Continue bundling if `options.bail` set to `false`.
        Ok(TaskResult::Factorize(Box::new(
          factorize_task_result
            .with_diagnostics(diagnostics)
            .with_file_dependencies(create_data.file_dependencies.drain())
            .with_missing_dependencies(create_data.missing_dependencies.drain())
            .with_context_dependencies(create_data.context_dependencies.drain()),
        )))
      }
    }
  }
}

pub type FactorizeQueue = WorkerQueue<FactorizeTask>;

#[derive(Derivative)]
#[derivative(Debug)]
pub struct AddTask {
  pub original_module_identifier: Option<ModuleIdentifier>,
  pub module: Box<dyn Module>,
  pub module_graph_module: Box<ModuleGraphModule>,
  pub dependencies: Vec<DependencyId>,
  pub is_entry: bool,
  pub current_profile: Option<Box<ModuleProfile>>,
  pub connect_origin: bool,
  #[derivative(Debug = "ignore")]
  pub callback: Option<ModuleCreationCallback>,
}

#[derive(Debug)]
pub enum AddTaskResult {
  ModuleReused {
    module: Box<dyn Module>,
  },
  ModuleAdded {
    module: Box<dyn Module>,
    current_profile: Option<Box<ModuleProfile>>,
  },
}

impl AddTask {
  pub fn run(self, compilation: &mut Compilation) -> Result<TaskResult> {
    if let Some(current_profile) = &self.current_profile {
      current_profile.mark_integration_start();
    }

    if self.module.as_self_module().is_some() && self.connect_origin {
      let issuer = self
        .module_graph_module
        .get_issuer()
        .identifier()
        .expect("self module should have issuer");

      set_resolved_module(
        &mut compilation.module_graph,
        self.original_module_identifier,
        self.dependencies,
        *issuer,
      )?;

      return Ok(TaskResult::Add(Box::new(AddTaskResult::ModuleReused {
        module: self.module,
      })));
    }

    let module_identifier = self.module.identifier();

    if self.connect_origin
      && compilation
        .module_graph
        .module_graph_module_by_identifier(&module_identifier)
        .is_some()
    {
      set_resolved_module(
        &mut compilation.module_graph,
        self.original_module_identifier,
        self.dependencies,
        module_identifier,
      )?;

      if let Some(callback) = self.callback {
        callback(&self.module);
      }

      return Ok(TaskResult::Add(Box::new(AddTaskResult::ModuleReused {
        module: self.module,
      })));
    }

    compilation
      .module_graph
      .add_module_graph_module(*self.module_graph_module);

    if self.connect_origin {
      set_resolved_module(
        &mut compilation.module_graph,
        self.original_module_identifier,
        self.dependencies,
        module_identifier,
      )?;
    }

    if self.is_entry {
      compilation
        .entry_module_identifiers
        .insert(module_identifier);
    }

    if let Some(current_profile) = &self.current_profile {
      current_profile.mark_integration_end();
    }

    if let Some(callback) = self.callback {
      callback(&self.module);
    }

    Ok(TaskResult::Add(Box::new(AddTaskResult::ModuleAdded {
      module: self.module,
      current_profile: self.current_profile,
    })))
  }
}

fn set_resolved_module(
  module_graph: &mut ModuleGraph,
  original_module_identifier: Option<ModuleIdentifier>,
  dependencies: Vec<DependencyId>,
  module_identifier: ModuleIdentifier,
) -> Result<()> {
  for dependency in dependencies {
    module_graph.set_resolved_module(original_module_identifier, dependency, module_identifier)?;
  }
  Ok(())
}

pub type AddQueue = WorkerQueue<AddTask>;

#[derive(Debug)]
pub struct BuildTask {
  pub module: Box<dyn Module>,
  pub resolver_factory: Arc<ResolverFactory>,
  pub compiler_options: Arc<CompilerOptions>,
  pub plugin_driver: SharedPluginDriver,
  pub cache: Arc<Cache>,
  pub current_profile: Option<Box<ModuleProfile>>,
  pub queue_handler: Option<QueueHandler>,
}

#[derive(Debug)]
pub struct BuildTaskResult {
  pub module: Box<dyn Module>,
  pub build_result: Box<BuildResult>,
  pub diagnostics: Vec<Diagnostic>,
  pub current_profile: Option<Box<ModuleProfile>>,
  pub from_cache: bool,
}

#[async_trait::async_trait]
impl WorkerTask for BuildTask {
  async fn run(self) -> Result<TaskResult> {
    if let Some(current_profile) = &self.current_profile {
      current_profile.mark_building_start();
    }

    let mut module = self.module;
    let compiler_options = self.compiler_options;
    let resolver_factory = self.resolver_factory;
    let cache = self.cache;
    let plugin_driver = self.plugin_driver;

    let (build_result, is_cache_valid) = match cache
      .build_module_occasion
      .use_cache(&mut module, |module| async {
        plugin_driver
          .build_module(module.as_mut())
          .await
          .unwrap_or_else(|e| panic!("Run build_module hook failed: {}", e));

        let result = module
          .build(
            BuildContext {
              compiler_context: CompilerContext {
                options: compiler_options.clone(),
                resolver_factory: resolver_factory.clone(),
                module: module.identifier(),
                module_context: module.as_normal_module().and_then(|m| m.get_context()),
                module_source_map_kind: module.get_source_map_kind().clone(),
                queue_handler: self.queue_handler.clone(),
                plugin_driver: plugin_driver.clone(),
                cache: cache.clone(),
              },
              plugin_driver: plugin_driver.clone(),
              compiler_options: &compiler_options,
            },
            None,
          )
          .await;

        plugin_driver
          .succeed_module(&**module)
          .await
          .unwrap_or_else(|e| panic!("Run succeed_module hook failed: {}", e));

        result.map(|t| {
          let diagnostics = module
            .clone_diagnostics()
            .into_iter()
            .map(|d| d.with_module_identifier(Some(module.identifier())))
            .collect();
          (t.with_diagnostic(diagnostics), module)
        })
      })
      .await
    {
      Ok(result) => result,
      Err(err) => panic!("build module get error: {}", err),
    };

    if is_cache_valid {
      plugin_driver.still_valid_module(module.as_ref()).await?;
    }

    if let Some(current_profile) = &self.current_profile {
      current_profile.mark_building_end();
    }

    build_result.map(|build_result| {
      let (build_result, diagnostics) = build_result.split_into_parts();

      TaskResult::Build(Box::new(BuildTaskResult {
        module,
        build_result: Box::new(build_result),
        diagnostics,
        current_profile: self.current_profile,
        from_cache: is_cache_valid,
      }))
    })
  }
}

pub type BuildQueue = WorkerQueue<BuildTask>;

#[derive(Debug)]
pub struct ProcessDependenciesTask {
  pub original_module_identifier: ModuleIdentifier,
  pub dependencies: Vec<DependencyId>,
  pub resolve_options: Option<Box<Resolve>>,
}

#[derive(Debug)]
pub struct ProcessDependenciesResult {
  pub module_identifier: ModuleIdentifier,
}

pub type ProcessDependenciesQueue = WorkerQueue<ProcessDependenciesTask>;

#[derive(Clone, Debug)]
pub struct BuildTimeExecutionOption {
  pub public_path: Option<String>,
  pub base_uri: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BuildTimeExecutionTask {
  pub module: ModuleIdentifier,
  pub request: String,
  pub options: BuildTimeExecutionOption,
  pub sender: UnboundedSender<Result<ExecuteModuleResult>>,
}

pub type BuildTimeExecutionQueue = WorkerQueue<BuildTimeExecutionTask>;

pub struct CleanTask {
  pub module_identifier: ModuleIdentifier,
}

#[derive(Debug)]
pub enum CleanTaskResult {
  ModuleIsUsed {
    module_identifier: ModuleIdentifier,
  },
  ModuleIsCleaned {
    module_identifier: ModuleIdentifier,
    dependent_module_identifiers: Vec<ModuleIdentifier>,
  },
}

impl CleanTask {
  pub fn run(self, compilation: &mut Compilation) -> CleanTaskResult {
    let module_identifier = self.module_identifier;
    let mgm = match compilation
      .module_graph
      .module_graph_module_by_identifier(&module_identifier)
    {
      Some(mgm) => mgm,
      None => {
        return CleanTaskResult::ModuleIsCleaned {
          module_identifier,
          dependent_module_identifiers: vec![],
        }
      }
    };

    if !mgm.incoming_connections.is_empty() {
      return CleanTaskResult::ModuleIsUsed { module_identifier };
    }

    let dependent_module_identifiers: Vec<ModuleIdentifier> = compilation
      .module_graph
      .get_module_all_depended_modules(&module_identifier)
      .expect("should have module")
      .into_iter()
      .copied()
      .collect();
    compilation.module_graph.revoke_module(&module_identifier);
    CleanTaskResult::ModuleIsCleaned {
      module_identifier,
      dependent_module_identifiers,
    }
  }
}

pub type CleanQueue = WorkerQueue<CleanTask>;

pub type ModuleCreationCallback = Box<dyn FnOnce(&BoxModule) + Send>;

pub type QueueHandleCallback = Box<dyn FnOnce(WaitTaskResult, &mut Compilation) + Send + Sync>;

#[derive(Debug)]
pub enum QueueTask {
  Factorize(Box<FactorizeTask>),
  Add(Box<AddTask>),
  Build(Box<BuildTask>),
  ProcessDependencies(Box<ProcessDependenciesTask>),
  BuildTimeExecution(Box<BuildTimeExecutionTask>),

  Subscription(Box<Subscription>),
}

#[derive(Debug, Copy, Clone)]
pub enum WaitTask {
  Factorize(DependencyId),
  Add(ModuleIdentifier),
  Build(ModuleIdentifier),
  ProcessDependencies(ModuleIdentifier),
}

#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum WaitTaskKey {
  Dependency(DependencyId),
  Identifier(ModuleIdentifier),
}

impl From<ModuleIdentifier> for WaitTaskKey {
  fn from(value: ModuleIdentifier) -> Self {
    Self::Identifier(value)
  }
}

impl From<DependencyId> for WaitTaskKey {
  fn from(value: DependencyId) -> Self {
    Self::Dependency(value)
  }
}

pub type WaitTaskResult = ModuleIdentifier;

#[derive(Derivative)]
#[derivative(Debug)]
pub struct Subscription {
  task: WaitTask,
  #[derivative(Debug = "ignore")]
  callback: QueueHandleCallback,
}

/// QueueHandler can let you have access to `make` phase.
/// You can also subscribe task, by calling wait_for to
/// wait for certain task
#[derive(Clone, Debug)]
pub struct QueueHandler {
  sender: UnboundedSender<QueueTask>,
}

impl QueueHandler {
  pub fn add_task(&self, task: QueueTask) -> Result<()> {
    self.sender.send(task).expect("Unexpected dropped receiver");
    Ok(())
  }

  pub fn wait_for(&self, wait_task: WaitTask, callback: QueueHandleCallback) {
    self
      .sender
      .send(QueueTask::Subscription(Box::new(Subscription {
        task: wait_task,
        callback,
      })))
      .expect("failed to wait task");
  }
}

pub struct QueueHandlerProcessor {
  receiver: UnboundedReceiver<QueueTask>,
  callbacks: [HashMap<WaitTaskKey, Vec<QueueHandleCallback>>; 4],
  finished: [HashMap<WaitTaskKey, WaitTaskResult>; 4],
}

impl QueueHandlerProcessor {
  fn get_bucket_and_key(task: WaitTask) -> (usize, WaitTaskKey) {
    let (bucket, key) = match task {
      WaitTask::Factorize(dep_id) => (0, dep_id.into()),
      WaitTask::Add(m) => (1, m.into()),
      WaitTask::Build(m) => (2, m.into()),
      WaitTask::ProcessDependencies(m) => (3, m.into()),
    };

    (bucket, key)
  }

  pub fn try_process(
    &mut self,
    compilation: &mut Compilation,
    factorize_queue: &mut FactorizeQueue,
    add_queue: &mut AddQueue,
    build_queue: &mut BuildQueue,
    process_dependencies_queue: &mut ProcessDependenciesQueue,
    buildtime_execution_queue: &mut BuildTimeExecutionQueue,
  ) {
    while let Ok(task) = self.receiver.try_recv() {
      match task {
        QueueTask::Factorize(task) => {
          factorize_queue.add_task(*task);
        }
        QueueTask::Add(task) => {
          add_queue.add_task(*task);
        }
        QueueTask::Build(task) => {
          build_queue.add_task(*task);
        }
        QueueTask::ProcessDependencies(task) => {
          process_dependencies_queue.add_task(*task);
        }
        QueueTask::BuildTimeExecution(task) => {
          buildtime_execution_queue.add_task(*task);
        }
        QueueTask::Subscription(subscription) => {
          let Subscription { task, callback } = *subscription;
          let (bucket, key) = Self::get_bucket_and_key(task);

          if let Some(module) = self.finished[bucket].get(&key) {
            // already finished
            callback(*module, compilation);
          } else {
            self.callbacks[bucket]
              .entry(key)
              .or_default()
              .push(callback);
          }
        }
      }
    }
  }

  pub fn complete_task(
    &mut self,
    task: WaitTask,
    task_result: WaitTaskResult,
    compilation: &mut Compilation,
  ) {
    let (bucket, key) = Self::get_bucket_and_key(task);
    self.finished[bucket].insert(key, task_result);
    if let Some(callbacks) = self.callbacks[bucket].get_mut(&key) {
      while let Some(cb) = callbacks.pop() {
        cb(task_result, compilation);
      }
    }
  }
}

pub fn create_queue_handle() -> (QueueHandler, QueueHandlerProcessor) {
  let (tx, rx) = unbounded_channel();

  (
    QueueHandler { sender: tx },
    QueueHandlerProcessor {
      receiver: rx,
      callbacks: Default::default(),
      finished: Default::default(),
    },
  )
}
