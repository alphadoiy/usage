//! One managed launch, in stages: select the account, read its credentials, build the CLI
//! command, attach observation and capture the launch identity, inject hooks, then spawn and
//! record. Each stage keeps its own logging step.
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output};
use std::time::Duration;

use rusqlite::params;

use super::{
    AuthMode, Error, Manager, Profile, Result, Revision, RunRecord, bark, clear_auth_env, config,
    effective, launcher, now, oauth, runtime, secrets,
};
use crate::logging::Operation;

/// The selected account and what its revision needs at launch.
struct Selected {
    profile: Profile,
    revision: i64,
    home: PathBuf,
    saved: Revision,
    /// BYOK credential, passed to the child through its environment.
    key: Option<String>,
    /// For plugin bindings: the saved endpoint and the plugin account it names.
    plugin_base: Option<String>,
    plugin_account: Option<String>,
    /// Held for the run's lifetime so login replacement waits for every OAuth run.
    oauth_lease: Option<std::fs::File>,
}

/// The CLI command before observation and hooks are added.
struct Prepared {
    command: Command,
    cwd: PathBuf,
    help: Output,
}

impl Manager {
    #[allow(clippy::too_many_arguments)]
    pub fn start_recorded_with_config(
        &mut self,
        selector: &str,
        cwd: &Path,
        revision: Option<i64>,
        resume: Option<&str>,
        program: &Path,
        args: &[String],
        usage_config: Option<&Path>,
    ) -> Result<crate::agents::ProcessHandle> {
        crate::logging::operation("codex.run_start", |op| {
            let selected = self.select_run(op, selector, revision, resume)?;
            let mut prepared = self.prepare_command(op, &selected, cwd, resume, program, args)?;
            let run_id = op.id().to_owned();
            let mut gateway = None;
            let capture = op.step("run_identity_capture", || {
                self.resolve_launch(
                    &selected,
                    &mut prepared,
                    &run_id,
                    resume,
                    args,
                    usage_config,
                    &mut gateway,
                )
            })?;
            self.inject_hooks(op, &selected, &mut prepared, &run_id)?;
            self.spawn_recorded(
                op, selected, prepared, run_id, resume, args, capture, gateway,
            )
        })
    }

    fn select_run(
        &mut self,
        op: &mut Operation,
        selector: &str,
        revision: Option<i64>,
        resume: Option<&str>,
    ) -> Result<Selected> {
        // Sync completes before selecting a revision. Failures keep the last saved snapshot usable.
        // The launch terminal belongs to Codex; the report is logged and shown in Provider management.
        if let Err(error) = op.step("source_sync", || self.sync(false)) {
            tracing::warn!(event = "source_sync_failed", error = %error, action = "launch_saved_snapshot");
        }
        let profile = op.step("account_resolve", || self.resolve(selector))?;
        let revision = revision.unwrap_or(profile.revision);
        op.account(&profile.id, revision, profile.auth_mode.log_name());
        tracing::info!(event = "run_selected", resume = resume.is_some());
        let oauth_lease = if profile.auth_mode == AuthMode::Oauth {
            Some(op.step("oauth_run_lease", || self.oauth_run_lease(&profile.id))?)
        } else {
            None
        };
        let home = op.step("home_materialize", || {
            self.materialize(&profile.id, revision)
        })?;
        let saved = op.step("revision_read", || self.revision(&profile.id, revision))?;
        let key = if profile.auth_mode == AuthMode::Byok {
            Some(op.step("credential_read", || {
                secrets::get(&self.root, &saved.secret_ref)
            })?)
        } else {
            let auth = op.step("oauth_auth_read", || {
                oauth::read_auth(&home.join("auth.json"))
            })?;
            if profile.oauth.as_ref().is_none_or(|o| {
                o.account_id != auth.identity.account_id || o.user_id != auth.identity.user_id
            }) {
                tracing::error!(event = "oauth_identity_mismatch");
                return Err(Error::message("OAuth 登录文件与账号身份不匹配；请重新登录"));
            }
            None
        };
        let plugin_base = if profile.auth_mode == AuthMode::Byok {
            Some(config::summary(&saved.config)?.0)
        } else {
            None
        };
        let plugin_account = plugin_base
            .as_deref()
            .and_then(crate::plugins::endpoint_account);
        if let Some(id) = &plugin_account {
            let plugin_store = crate::plugins::store::Store::new(crate::plugins::store::root());
            let account = plugin_store.account(id).map_err(Error::message)?;
            plugin_store
                .plugin(&account.plugin)
                .map_err(Error::message)?;
            let expected = plugin_store.inference_key(id).map_err(Error::message)?;
            if key.as_deref() != Some(expected.as_str()) {
                return Err(Error::message("插件账号与启动凭据不匹配"));
            }
            if !crate::gateway::enabled() {
                return Err(Error::message("插件推理依赖本地网关，请启用 USAGE_GATEWAY"));
            }
        }
        Ok(Selected {
            profile,
            revision,
            home,
            saved,
            key,
            plugin_base,
            plugin_account,
            oauth_lease,
        })
    }

    fn prepare_command(
        &self,
        op: &mut Operation,
        selected: &Selected,
        cwd: &Path,
        resume: Option<&str>,
        program: &Path,
        args: &[String],
    ) -> Result<Prepared> {
        let Selected {
            profile,
            revision,
            home,
            key,
            ..
        } = selected;
        op.begin("cwd_validate");
        let cwd = cwd
            .canonicalize()
            .map_err(|_| Error::message("项目目录不存在"))?;
        if !cwd.is_dir() {
            return Err(Error::message("项目路径不是目录"));
        }
        if let Some(id) = resume {
            uuid::Uuid::parse_str(id).map_err(|_| Error::message("无效的会话 ID"))?;
        }
        if profile.auth_mode == AuthMode::Oauth {
            op.step("arguments_validate", || oauth::validate_run_args(args))?;
            op.step("cli_version_detect", || oauth::verify_cli_version(program))?;
        } else {
            op.step("arguments_validate", || {
                launcher::validate_run_args(args, profile.auth_mode)
            })?;
        }
        op.begin("cli_capability_detect");
        let mut help = Command::new(program);
        clear_auth_env(&mut help);
        help.arg("--help");
        let help = crate::credentials::output_with_timeout(help, Duration::from_secs(10))
            .ok_or_else(|| {
                Error::message(
                    "无法运行 Codex CLI 或能力检测超时；请先安装 codex 或使用 --binary 指定路径",
                )
            })?;
        if !help.status.success() {
            return Err(Error::message("Codex CLI 能力检测失败"));
        }
        let mut command = Command::new(program);
        clear_auth_env(&mut command);
        // An inherited state directory would share the user's state database with this Home.
        command
            .current_dir(&cwd)
            .env("CODEX_HOME", home)
            .env_remove("CODEX_SQLITE_HOME");
        if let Some(key) = key {
            command.env(config::KEY_ENV, key);
        }
        if profile.auth_mode == AuthMode::Oauth {
            if !String::from_utf8_lossy(&help.stdout).contains("--profile") {
                return Err(Error::message(
                    "OAuth 配置版本需要支持独立 profile 文件的新版 Codex CLI；请升级 codex",
                ));
            }
            command.args(["--profile", &format!("usage-r{revision}")]);
            command.args([
                "-c",
                "cli_auth_credentials_store=\"file\"",
                "-c",
                "forced_login_method=\"chatgpt\"",
                "-c",
                "model_provider=\"openai\"",
            ]);
            if let Some(identity) = &profile.oauth {
                command.args([
                    "-c",
                    &format!(
                        "forced_chatgpt_workspace_id={}",
                        toml_edit::value(&identity.account_id)
                    ),
                ]);
            }
        }
        if String::from_utf8_lossy(&help.stdout).contains("--no-daemon") {
            command.arg("--no-daemon");
        }
        Ok(Prepared { command, cwd, help })
    }

    /// Resolves what this launch will actually use, attaches observation where it can be
    /// confirmed, and captures the identity recorded with the run.
    #[allow(clippy::too_many_arguments)]
    fn resolve_launch(
        &self,
        selected: &Selected,
        prepared: &mut Prepared,
        run_id: &str,
        resume: Option<&str>,
        args: &[String],
        usage_config: Option<&Path>,
        gateway: &mut Option<crate::gateway::Attachment>,
    ) -> Result<crate::runs::CapturedContext> {
        let Selected {
            profile,
            saved,
            key,
            plugin_account,
            ..
        } = selected;
        let environment = |name: &str| std::env::var(name).ok();
        let effective = effective::resolve(
            &saved.config,
            profile.auth_mode,
            &prepared.cwd,
            resume.is_some(),
            args,
            key.as_deref(),
            &environment,
        );
        if let (Some(env), Some(key)) = (&effective.credential_env, &effective.key) {
            prepared.command.env(env, key);
        }
        // A plugin resume pins its transport explicitly, while its resource identity
        // remains unresolved until an Agent event confirms the session configuration.
        let resume_transport = (plugin_account.is_some() && resume.is_some()).then(|| {
            effective::resolve(
                &saved.config,
                profile.auth_mode,
                &prepared.cwd,
                false,
                args,
                key.as_deref(),
                &environment,
            )
        });
        let route_effective = resume_transport.as_ref().unwrap_or(&effective);
        *gateway = self.attach_gateway(selected, &mut prepared.command, run_id, route_effective)?;
        let path = usage_config
            .map(Path::to_path_buf)
            .unwrap_or_else(crate::config::default_config_path);
        let config = crate::config::load_config(&path, usage_config.is_some());
        let resources = std::cell::OnceCell::new();
        let captured = crate::runs::CapturedContext::capture(
            run_id,
            &profile.id,
            effective.identity,
            effective.key.as_deref(),
            config
                .as_ref()
                .map(|c| c.resources.as_slice())
                .map_err(|_| "启动时资源配置读取失败"),
            &environment,
            &|config| {
                resources
                    .get_or_init(|| {
                        crate::resources::connections::ResourceService::from_environment(
                            crate::hub::shared().resources.clone(),
                        )
                    })
                    .prepare(config)
                    .ok()
                    .map(|reader| reader.scope())
            },
        );
        tracing::info!(event="run_identity_captured", state=?captured.context.identity.state,
            reason=captured.context.identity.reason.as_deref(),
            resource_count=captured.context.resources.len(),
            matched_resources=captured.context.resources.iter().filter(|l| l.state == crate::runs::LinkState::Matched).count(),
            resources_error=captured.context.resources_error.as_deref());
        for link in &captured.context.resources {
            tracing::info!(event="run_resource_link", connection_id=link.binding.account,
                service=link.service, revision=link.binding.revision, state=?link.state);
        }
        Ok(captured)
    }

    /// Official subscriptions always reach OpenAI directly; their performance comes from the
    /// session log. BYOK observation degrades to a direct launch; plugin accounts require it.
    fn attach_gateway(
        &self,
        selected: &Selected,
        command: &mut Command,
        run_id: &str,
        route: &effective::Resolution,
    ) -> Result<Option<crate::gateway::Attachment>> {
        let Selected {
            profile,
            revision,
            key,
            plugin_base,
            plugin_account,
            ..
        } = selected;
        if profile.auth_mode == AuthMode::Oauth {
            tracing::info!(
                event = "gateway_skipped",
                run_id,
                reason = "official_direct"
            );
            return Ok(None);
        }
        if !crate::gateway::enabled() {
            return Ok(None);
        }
        if !route.gateway_supported {
            let reason = route
                .identity
                .reason
                .as_deref()
                .unwrap_or("HTTP/SSE route cannot be confirmed");
            if plugin_account.is_some() {
                return Err(Error::message(
                    "插件会话的启动配置无法确认，请移除冲突覆盖参数",
                ));
            }
            tracing::info!(event = "gateway_skipped", run_id, reason);
            return Ok(None);
        }
        let provider = route
            .identity
            .model_provider
            .as_deref()
            .expect("resolved provider");
        // Launching directly would silently drop observation, so an unaddressable provider fails the launch.
        let Some(base_url_key) = launcher::provider_override(provider, "base_url") else {
            tracing::error!(
                event = "gateway_provider_unaddressable",
                run_id,
                provider_id = profile.id
            );
            return Err(Error::message(
                "启动失败：模型提供方 ID 只能包含字母、数字、_ 或 -，Codex 无法通过 -c 参数接入本地网关；请在 Agent 配置中修改 model_provider 名称",
            ));
        };
        let prepared = (|| -> Result<crate::gateway::Attachment> {
            let binding = self.performance_binding(profile, *revision)?;
            let upstream = route
                .identity
                .endpoint
                .as_deref()
                .expect("resolved endpoint");
            let auth = if let Some(id) = plugin_account {
                if route.identity.endpoint.as_deref() != plugin_base.as_deref() || route.key != *key
                {
                    return Err(Error::message("插件启动配置被覆盖，无法确认账号关联"));
                }
                crate::gateway::RouteAuth::Plugin {
                    account: id.clone(),
                }
            } else {
                crate::gateway::RouteAuth::ApiKey {
                    key: route.key.clone().expect("resolved credential"),
                }
            };
            let attached =
                crate::gateway::attach(&self.root, run_id, &binding, upstream, provider, auth)
                    .map_err(Error::message)?;
            #[cfg(unix)]
            launcher::inherit_run_lease(command, &attached.lease)?;
            Ok(attached)
        })();
        let attached = match prepared {
            Ok(attached) => attached,
            Err(error) => {
                if plugin_account.is_some() {
                    return Err(error);
                }
                tracing::warn!(event = "gateway_fallback", run_id, provider_id = profile.id, reason = %error, action = "direct_launch");
                return Ok(None);
            }
        };
        let base = toml::Value::String(attached.base.clone());
        command.env(
            route
                .credential_env
                .as_deref()
                .expect("resolved environment"),
            &attached.token,
        );
        command.args(["-c", &format!("{base_url_key}={base}")]);
        if plugin_account.is_some() {
            let catalog_key = launcher::provider_override(provider, "model_catalog_url")
                .expect("validated provider id");
            let catalog_url = toml::Value::String(format!("{}/codex/models", attached.base));
            command.args([
                "-c",
                &format!("model_provider={}", toml::Value::String(provider.into())),
                "-c",
                &format!("{catalog_key}={catalog_url}"),
            ]);
        }
        for variable in ["NO_PROXY", "no_proxy"] {
            let mut bypass = std::env::var(variable).unwrap_or_default();
            if !bypass.is_empty() {
                bypass.push(',');
            }
            bypass.push_str("127.0.0.1,localhost,::1");
            command.env(variable, bypass);
        }
        tracing::info!(event = "gateway_injected", run_id, provider_id = profile.id);
        Ok(Some(attached))
    }

    fn inject_hooks(
        &self,
        op: &mut Operation,
        selected: &Selected,
        prepared: &mut Prepared,
        run_id: &str,
    ) -> Result<()> {
        op.step("notification_inject", || {
            bark::inject(
                &mut prepared.command,
                &self.root,
                &selected.home,
                &selected.profile.name,
                run_id,
                &selected.saved.config,
            )
        })?;
        op.step("runtime_events_inject", || {
            runtime::inject(
                &mut prepared.command,
                &self.root,
                &selected.home,
                run_id,
                &selected.saved.config,
                String::from_utf8_lossy(&prepared.help.stdout)
                    .contains("--dangerously-bypass-hook-trust"),
            )
        })?;
        Ok(())
    }

    /// The run row is committed in the same step as the spawn; a failed record kills the child.
    #[allow(clippy::too_many_arguments)]
    fn spawn_recorded(
        &mut self,
        op: &mut Operation,
        selected: Selected,
        prepared: Prepared,
        run_id: String,
        resume: Option<&str>,
        args: &[String],
        mut capture: crate::runs::CapturedContext,
        gateway: Option<crate::gateway::Attachment>,
    ) -> Result<crate::agents::ProcessHandle> {
        let Selected {
            profile,
            revision,
            home,
            oauth_lease,
            ..
        } = selected;
        let Prepared {
            mut command, cwd, ..
        } = prepared;
        if let Some(id) = resume {
            command.args(["resume", id]);
        }
        command.args(args);
        #[cfg(unix)]
        if let Some(lease) = &oauth_lease {
            launcher::inherit_run_lease(&mut command, lease)?;
        }
        let mut child = op.step("child_spawn", || command.spawn())?;
        tracing::info!(event = "child_spawned", child_pid = child.id());
        op.begin("run_record");
        let recorded = (|| -> Result<RunRecord> {
            capture.process = crate::runs::ProcessIdentity::capture(child.id());
            let capture = serde_json::to_string(&capture)
                .map_err(|_| Error::message("运行身份无法序列化"))?;
            let tx = self.conn.transaction()?;
            tx.execute("INSERT INTO runs(id,profile_id,revision,cwd,started_at,pid) VALUES (?1,?2,?3,?4,?5,?6)",
                params![run_id,profile.id,revision,cwd.to_string_lossy(),now(),child.id()])?;
            tx.execute(
                "INSERT INTO run_contexts(run_id,context_json) VALUES (?1,?2)",
                params![run_id, capture],
            )?;
            runtime::record_started(&tx, &run_id)?;
            tx.commit()?;
            self.run_record(&run_id)?
                .ok_or_else(|| Error::message("启动记录不存在"))
        })();
        let run = match recorded {
            Ok(run) => run,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let root = self.root.clone();
        let span = op.span();
        let finish = Box::new(move |status: ExitStatus| {
            // Keep the OAuth lease and the gateway route owned by this process handle until exit.
            let _lease = &oauth_lease;
            let _gateway = &gateway;
            span.in_scope(|| runtime::finish_run(&root, &home, &run_id, profile.auth_mode, status))
                .map_err(|error| error.to_string())
        });
        Ok(crate::agents::ProcessHandle::new(
            child,
            Some(run),
            Some(finish),
        ))
    }
}
