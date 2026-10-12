// Adapted from https://github.com/zed-extensions/tsgo and modified for Zed's
// built-in TypeScript 7 language-server installer. Licensed under Apache-2.0.
use anyhow::{Context as _, Result, anyhow, ensure};
use async_trait::async_trait;
use collections::HashMap;
use gpui::AsyncApp;
use language::{LanguageName, LspAdapter, LspAdapterDelegate, LspInstaller, Toolchain};
use lsp::{LanguageServerBinary, LanguageServerName, Uri};
use node_runtime::{NodeRuntime, VersionStrategy};
use project::lsp_store::language_server_settings;
use semver::{Version, VersionReq};
use serde_json::{Value, json};
use std::{
    future::Future,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
};
use util::merge_json_value_into;

const PACKAGE_NAME: &str = "typescript";
const SERVER_NAME: LanguageServerName = LanguageServerName::new_static("typescript-ls");
const SERVER_ARGUMENTS: [&str; 2] = ["--lsp", "--stdio"];

static TYPESCRIPT_VERSION_REQUIREMENT: LazyLock<VersionReq> =
    LazyLock::new(|| VersionReq::parse(">=7.0.0").expect("valid TypeScript version requirement"));

pub struct TsGoLspAdapter {
    node: NodeRuntime,
}

fn language_ids() -> HashMap<LanguageName, String> {
    HashMap::from_iter([
        (LanguageName::new_static("TypeScript"), "typescript".into()),
        (LanguageName::new_static("TSX"), "typescriptreact".into()),
        (LanguageName::new_static("JavaScript"), "javascript".into()),
        (LanguageName::new_static("JSX"), "javascriptreact".into()),
    ])
}

impl TsGoLspAdapter {
    pub fn new(node: NodeRuntime) -> Self {
        Self { node }
    }

    fn native_binary_path(container_dir: &Path) -> Result<PathBuf> {
        let platform = match std::env::consts::OS {
            "macos" => "darwin",
            "linux" => "linux",
            "windows" => "win32",
            platform => {
                return Err(anyhow!(
                    "TypeScript native server does not support {platform}"
                ));
            }
        };
        let architecture = match std::env::consts::ARCH {
            "aarch64" => "arm64",
            "x86_64" => "x64",
            architecture => {
                return Err(anyhow!(
                    "TypeScript native server does not support {architecture}"
                ));
            }
        };
        let binary_name = if cfg!(windows) { "tsc.exe" } else { "tsc" };

        Ok(container_dir
            .join("node_modules")
            .join("@typescript")
            .join(format!("typescript-{platform}-{architecture}"))
            .join("lib")
            .join(binary_name))
    }

    fn server_binary(path: PathBuf) -> LanguageServerBinary {
        LanguageServerBinary {
            path,
            arguments: SERVER_ARGUMENTS.iter().map(Into::into).collect(),
            env: None,
        }
    }
}

impl LspInstaller for TsGoLspAdapter {
    type BinaryVersion = Version;

    async fn fetch_latest_server_version(
        &self,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: bool,
        cx: &mut AsyncApp,
    ) -> Result<Self::BinaryVersion> {
        let configured_version = cx.update(|cx| {
            language_server_settings(delegate.as_ref(), &SERVER_NAME, cx)
                .and_then(|settings| settings.settings.as_ref())
                .and_then(|settings| settings.get("package_version"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        });

        let version = if let Some(configured_version) = configured_version {
            Version::parse(&configured_version).with_context(|| {
                format!("invalid TypeScript package_version {configured_version:?}")
            })?
        } else {
            self.node
                .npm_package_latest_version_with_requirement(
                    PACKAGE_NAME,
                    Some(&TYPESCRIPT_VERSION_REQUIREMENT),
                )
                .await?
        };

        ensure!(
            TYPESCRIPT_VERSION_REQUIREMENT.matches(&version),
            "typescript-ls requires TypeScript 7.0.0 or newer, but {version} was selected"
        );
        Ok(version)
    }

    fn check_if_version_installed(
        &self,
        version: &Self::BinaryVersion,
        container_dir: &PathBuf,
        _: &Arc<dyn LspAdapterDelegate>,
    ) -> impl Send + Future<Output = Option<LanguageServerBinary>> + use<> {
        let node = self.node.clone();
        let version = version.clone();
        let container_dir = container_dir.clone();

        async move {
            let binary_path = Self::native_binary_path(&container_dir).ok()?;
            if node
                .should_install_npm_package(
                    PACKAGE_NAME,
                    &binary_path,
                    &container_dir,
                    VersionStrategy::Pin(&version),
                )
                .await
            {
                return None;
            }

            Some(Self::server_binary(binary_path))
        }
    }

    fn fetch_server_binary(
        &self,
        version: Self::BinaryVersion,
        container_dir: PathBuf,
        _: &Arc<dyn LspAdapterDelegate>,
    ) -> impl Send + Future<Output = Result<LanguageServerBinary>> + use<> {
        let node = self.node.clone();

        async move {
            let version = version.to_string();
            node.npm_install_packages(&container_dir, &[(PACKAGE_NAME, &version)])
                .await?;

            let binary_path = Self::native_binary_path(&container_dir)?;
            anyhow::ensure!(
                binary_path.is_file(),
                "TypeScript native binary was not installed at {}",
                binary_path.display()
            );
            Ok(Self::server_binary(binary_path))
        }
    }

    async fn cached_server_binary(
        &self,
        container_dir: PathBuf,
        _: &dyn LspAdapterDelegate,
    ) -> Option<LanguageServerBinary> {
        let binary_path = Self::native_binary_path(&container_dir).ok()?;
        binary_path
            .is_file()
            .then(|| Self::server_binary(binary_path))
    }
}

#[async_trait(?Send)]
impl LspAdapter for TsGoLspAdapter {
    fn name(&self) -> LanguageServerName {
        SERVER_NAME
    }

    async fn initialization_options(
        self: Arc<Self>,
        delegate: &Arc<dyn LspAdapterDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<Option<Value>> {
        let mut options = json!({
            "codeLensShowLocationsCommandName": "editor.action.showReferences"
        });
        let user_options = cx.update(|cx| {
            language_server_settings(delegate.as_ref(), &SERVER_NAME, cx)
                .and_then(|settings| settings.initialization_options.clone())
        });
        if let Some(user_options) = user_options {
            merge_json_value_into(user_options, &mut options);
        }
        Ok(Some(options))
    }

    async fn workspace_configuration(
        self: Arc<Self>,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: Option<Toolchain>,
        _: Option<Uri>,
        cx: &mut AsyncApp,
    ) -> Result<Value> {
        let config = json!({
            "inlayHints": {
                "parameterNames": {
                    "enabled": "all",
                    "suppressWhenArgumentMatchesName": false
                },
                "parameterTypes": {
                    "enabled": true
                },
                "variableTypes": {
                    "enabled": true,
                    "suppressWhenTypeMatchesName": false
                },
                "propertyDeclarationTypes": {
                    "enabled": true
                },
                "functionLikeReturnTypes": {
                    "enabled": true
                },
                "enumMemberValues": {
                    "enabled": true
                }
            },
            "implementationsCodeLens": {
                "enabled": true,
                "showOnAllClassMethods": true,
                "showOnInterfaceMethods": true
            },
            "referencesCodeLens": {
                "enabled": true,
                "showOnAllFunctions": true
            }
        });
        let mut settings = json!({
            "typescript": config,
            "javascript": config
        });
        let user_settings = cx.update(|cx| {
            language_server_settings(delegate.as_ref(), &SERVER_NAME, cx)
                .and_then(|settings| settings.settings.clone())
        });
        if let Some(user_settings) = user_settings {
            merge_json_value_into(user_settings, &mut settings);
        }
        Ok(settings)
    }

    fn language_ids(&self) -> HashMap<LanguageName, String> {
        language_ids()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_native_binary_path_uses_platform_package() {
        let path = TsGoLspAdapter::native_binary_path(Path::new("/server"))
            .expect("the test platform is supported");
        assert!(path.starts_with("/server/node_modules/@typescript"));
        assert!(path.ends_with(if cfg!(windows) {
            "lib/tsc.exe"
        } else {
            "lib/tsc"
        }));
    }

    #[test]
    fn test_typescript_server_requires_version_seven_or_newer() {
        assert!(TYPESCRIPT_VERSION_REQUIREMENT.matches(&Version::new(7, 0, 0)));
        assert!(!TYPESCRIPT_VERSION_REQUIREMENT.matches(&Version::new(6, 9, 9)));
    }

    #[test]
    fn test_typescript_lsp_id_and_language_ids() {
        assert_eq!(SERVER_NAME.0.as_ref(), "typescript-ls");
        let language_ids = language_ids();
        assert_eq!(
            language_ids.get("TypeScript").map(String::as_str),
            Some("typescript")
        );
        assert_eq!(
            language_ids.get("TSX").map(String::as_str),
            Some("typescriptreact")
        );
        assert_eq!(
            language_ids.get("JavaScript").map(String::as_str),
            Some("javascript")
        );
        assert_eq!(
            language_ids.get("JSX").map(String::as_str),
            Some("javascriptreact")
        );
    }
}
