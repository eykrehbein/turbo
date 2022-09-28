use std::collections::BTreeMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use turbo_tasks::{debug::ValueDebugFormat, trace::TraceRawVcs, Value};
use turbo_tasks_fs::{
    glob::{Glob, GlobVc},
    FileSystemPathVc,
};

use super::{
    alias_map::{AliasMap, AliasTemplate},
    ResolveResult, ResolveResultVc, SpecialType,
};
use crate::resolve::parse::RequestVc;

#[turbo_tasks::value(shared)]
#[derive(Hash, Debug)]
pub struct LockedVersions {}

/// A location where to resolve modules.
#[derive(
    TraceRawVcs, Hash, PartialEq, Eq, Clone, Debug, Serialize, Deserialize, ValueDebugFormat,
)]
pub enum ResolveModules {
    /// when inside of path, use the list of directories to
    /// resolve inside these
    Nested(FileSystemPathVc, Vec<String>),
    /// look into that directory
    Path(FileSystemPathVc),
    /// lookup versions based on lockfile in the registry filesystem
    /// registry filesystem is assumed to have structure like
    /// @scope/module/version/<path-in-package>
    Registry(FileSystemPathVc, LockedVersionsVc),
}

#[derive(TraceRawVcs, Hash, PartialEq, Eq, Clone, Debug, Serialize, Deserialize)]
pub enum ConditionValue {
    Set,
    Unset,
    Unknown,
}

impl From<bool> for ConditionValue {
    fn from(v: bool) -> Self {
        if v {
            ConditionValue::Set
        } else {
            ConditionValue::Unset
        }
    }
}

/// The different ways to resolve a package, as described in package.json.
#[derive(TraceRawVcs, Hash, PartialEq, Eq, Clone, Debug, Serialize, Deserialize)]
pub enum ResolveIntoPackage {
    /// Using the [exports] field.
    ///
    /// [exports]: https://nodejs.org/api/packages.html#exports
    ExportsField {
        field: String,
        conditions: BTreeMap<String, ConditionValue>,
        unspecified_conditions: ConditionValue,
    },
    /// Using a [main]-like field (e.g. [main], [module], [browser], etc.).
    ///
    /// [main]: https://nodejs.org/api/packages.html#main
    /// [module]: https://esbuild.github.io/api/#main-fields
    /// [browser]: https://esbuild.github.io/api/#main-fields
    MainField(String),
    /// Default behavior of using the index.js file at the root of the package.
    Default(String),
}

#[derive(
    TraceRawVcs, Hash, PartialEq, Eq, Clone, Debug, Serialize, Deserialize, ValueDebugFormat,
)]
pub enum ImportMapping {
    External(Option<String>),
    /// A request alias that will be resolved first, and fall back to resolving
    /// the original request if it fails. Useful for the tsconfig.json
    /// `compilerOptions.paths` option.
    PrimaryAlternative(String, Option<FileSystemPathVc>),
    Ignore,
    Empty,
    Alternatives(Vec<ImportMapping>),
}

impl ImportMapping {
    pub fn primary_alternatives(
        list: Vec<String>,
        context: Option<FileSystemPathVc>,
    ) -> ImportMapping {
        if list.is_empty() {
            ImportMapping::Ignore
        } else if list.len() == 1 {
            ImportMapping::PrimaryAlternative(list.into_iter().next().unwrap(), context)
        } else {
            ImportMapping::Alternatives(
                list.into_iter()
                    .map(|s| ImportMapping::PrimaryAlternative(s, context))
                    .collect(),
            )
        }
    }
}

impl AliasTemplate for ImportMapping {
    type Output = Result<Self>;

    fn replace(&self, capture: &str) -> Result<Self> {
        match self {
            ImportMapping::External(name) => {
                if let Some(name) = name {
                    Ok(ImportMapping::External(Some(
                        name.clone().replace('*', capture),
                    )))
                } else {
                    Ok(ImportMapping::External(None))
                }
            }
            ImportMapping::PrimaryAlternative(name, context) => Ok(
                ImportMapping::PrimaryAlternative(name.clone().replace('*', capture), *context),
            ),
            ImportMapping::Ignore | ImportMapping::Empty => Ok(self.clone()),
            ImportMapping::Alternatives(alternatives) => Ok(ImportMapping::Alternatives(
                alternatives
                    .iter()
                    .map(|mapping| mapping.replace(capture))
                    .collect::<Result<Vec<_>>>()?,
            )),
        }
    }
}

#[turbo_tasks::value(shared)]
#[derive(Clone, Debug, Default)]
pub struct ImportMap {
    pub direct: AliasMap<ImportMapping>,
    pub by_glob: Vec<(Glob, ImportMapping)>,
}

#[turbo_tasks::value(shared)]
#[derive(Clone, Debug, Default)]
pub struct ResolvedMap {
    pub by_glob: Vec<(FileSystemPathVc, GlobVc, ImportMapping)>,
}

#[turbo_tasks::value(shared)]
#[derive(Clone, Debug)]
pub enum ImportMapResult {
    Result(ResolveResultVc),
    Alias(RequestVc, Option<FileSystemPathVc>),
    Alternatives(Vec<ImportMapResult>),
    NoEntry,
}

fn import_mapping_to_result(mapping: &ImportMapping) -> ImportMapResult {
    match mapping {
        ImportMapping::External(name) => ImportMapResult::Result(
            ResolveResult::Special(
                name.as_ref().map_or_else(
                    || SpecialType::OriginalReferenceExternal,
                    |req| SpecialType::OriginalReferenceTypeExternal(req.to_string()),
                ),
                Vec::new(),
            )
            .into(),
        ),
        ImportMapping::Ignore => {
            ImportMapResult::Result(ResolveResult::Special(SpecialType::Ignore, Vec::new()).into())
        }
        ImportMapping::Empty => {
            ImportMapResult::Result(ResolveResult::Special(SpecialType::Empty, Vec::new()).into())
        }
        ImportMapping::PrimaryAlternative(name, context) => {
            let request = RequestVc::parse(Value::new(name.to_string().into()));

            ImportMapResult::Alias(request, *context)
        }
        ImportMapping::Alternatives(list) => {
            ImportMapResult::Alternatives(list.iter().map(import_mapping_to_result).collect())
        }
    }
}

#[turbo_tasks::value_impl]
impl ImportMapVc {
    #[turbo_tasks::function]
    pub async fn lookup(self, request: RequestVc) -> Result<ImportMapResultVc> {
        let this = self.await?;
        // TODO lookup pattern
        if let Some(request_string) = request.await?.request() {
            if let Some(result) = this.direct.lookup(&request_string).next() {
                return Ok(import_mapping_to_result(result.try_into_self()?.as_ref()).into());
            }
            let request_string_without_slash = if request_string.ends_with('/') {
                &request_string[..request_string.len() - 1]
            } else {
                &request_string
            };
            for (glob, mapping) in this.by_glob.iter() {
                if glob.execute(request_string_without_slash) {
                    return Ok(import_mapping_to_result(mapping).into());
                }
            }
        }
        Ok(ImportMapResult::NoEntry.into())
    }
}

#[turbo_tasks::value_impl]
impl ResolvedMapVc {
    #[turbo_tasks::function]
    pub async fn lookup(self, resolved: FileSystemPathVc) -> Result<ImportMapResultVc> {
        let this = self.await?;
        let resolved = resolved.await?;
        for (root, glob, mapping) in this.by_glob.iter() {
            let root = root.await?;
            if let Some(path) = root.get_path_to(&resolved) {
                if glob.await?.execute(path) {
                    return Ok(import_mapping_to_result(mapping).into());
                }
            }
        }
        Ok(ImportMapResult::NoEntry.into())
    }
}

#[turbo_tasks::value(shared)]
#[derive(Clone, Debug, Default)]
pub struct ResolveOptions {
    pub extensions: Vec<String>,
    /// The locations where to resolve modules.
    pub modules: Vec<ResolveModules>,
    /// How to resolve packages.
    pub into_package: Vec<ResolveIntoPackage>,
    pub import_map: Option<ImportMapVc>,
    pub resolved_map: Option<ResolvedMapVc>,
    pub placeholder_for_future_extensions: (),
}

#[turbo_tasks::value_impl]
impl ResolveOptionsVc {
    #[turbo_tasks::function]
    pub async fn modules(self) -> Result<ResolveModulesOptionsVc> {
        Ok(ResolveModulesOptions {
            modules: self.await?.modules.clone(),
        }
        .into())
    }
}

#[turbo_tasks::value(shared)]
#[derive(Hash, Clone, Debug)]
pub struct ResolveModulesOptions {
    pub modules: Vec<ResolveModules>,
}

#[turbo_tasks::function]
pub async fn resolve_modules_options(options: ResolveOptionsVc) -> Result<ResolveModulesOptionsVc> {
    Ok(ResolveModulesOptions {
        modules: options.await?.modules.clone(),
    }
    .into())
}
