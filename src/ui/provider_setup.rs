//! Shared onboarding; implementation packages remain behind provider identities.
mod native;
mod picker;
mod quota;
use crate::{auth, codex_manager::Profile, plugins, provider_catalog::presets};
pub(super) use native::run as run_native;
pub(super) use native::{AddMethod, edit, relogin};
pub(super) use quota::edit as edit_quota;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{future::Future, io, path::PathBuf, sync::mpsc, time::Duration};
use tokio::runtime::Handle;

enum Source {
    Account(plugins::store::Account),
    Plugin(plugins::store::Plugin),
    Install,
}
impl Source {
    fn label(&self) -> String {
        match self {
            Self::Account(a) => format!("绑定已有账号 · {} · {}", a.name, a.provider),
            Self::Plugin(p) => format!(
                "{} · {} · {}",
                if p.enabled {
                    "登录插件"
                } else {
                    "启用并登录"
                },
                p.id,
                p.version
            ),
            Self::Install => "安装自定义插件…".into(),
        }
    }
}
#[derive(Clone, Deserialize)]
struct Provider {
    id: String,
    name: String,
    methods: Vec<Method>,
}
#[derive(Clone, Deserialize)]
struct Method {
    id: String,
    label: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    prompts: Vec<Input>,
}
#[derive(Clone, Deserialize)]
struct Rule {
    key: String,
    op: String,
    value: String,
}
#[derive(Clone, Deserialize)]
struct Choice {
    label: String,
    value: String,
}
#[derive(Clone, Deserialize)]
struct Input {
    key: String,
    message: String,
    #[serde(default)]
    options: Vec<Choice>,
    when: Option<Rule>,
}
struct Field {
    input: Input,
    value: String,
    secret: bool,
}
impl Field {
    fn text(key: &str, label: &str, value: &str) -> Self {
        Self {
            input: Input {
                key: key.into(),
                message: label.into(),
                options: Vec::new(),
                when: None,
            },
            value: value.into(),
            secret: false,
        }
    }
    fn from_input(input: Input) -> Self {
        let key = input.key.to_ascii_lowercase();
        let secret = ["key", "token", "secret", "password"]
            .iter()
            .any(|part| key.contains(part));
        let value = input
            .options
            .first()
            .map(|o| o.value.clone())
            .unwrap_or_default();
        Self {
            input,
            value,
            secret,
        }
    }
    fn display(&self) -> String {
        if self.secret {
            "•".repeat(self.value.chars().count().min(32))
        } else {
            self.input
                .options
                .iter()
                .find(|o| o.value == self.value)
                .map(|o| o.label.clone())
                .unwrap_or_else(|| self.value.clone())
        }
    }
}
fn visible(fields: &[Field]) -> Vec<usize> {
    fields
        .iter()
        .enumerate()
        .filter(|(_, f)| {
            f.input.when.as_ref().is_none_or(|r| {
                let equal = fields
                    .iter()
                    .find(|f| f.input.key == r.key)
                    .is_some_and(|f| f.value == r.value);
                if r.op == "neq" { !equal } else { equal }
            })
        })
        .map(|(i, _)| i)
        .collect()
}
fn inputs(fields: &[Field]) -> Value {
    Value::Object(
        visible(fields)
            .into_iter()
            .map(|i| (fields[i].input.key.clone(), fields[i].value.clone().into()))
            .collect(),
    )
}

/// A bulk import can succeed without selecting one particular profile.
pub(super) struct Added {
    pub profile: Option<Profile>,
    pub notice: String,
}
impl From<Profile> for Added {
    fn from(profile: Profile) -> Self {
        Self {
            notice: format!("已添加 Provider：{}", profile.name),
            profile: Some(profile),
        }
    }
}

/// Reuse the parent screen so every onboarding step remains a submenu.
struct Setup<'a> {
    terminal: &'a mut DefaultTerminal,
    background: &'a dyn Fn(&mut Frame),
}
impl Setup<'_> {
    fn draw(&mut self, render: impl FnOnce(&mut Frame)) -> io::Result<()> {
        self.terminal.draw(|frame| {
            (self.background)(frame);
            render(frame);
        })?;
        Ok(())
    }
}
pub(super) fn show_notice(
    terminal: &mut DefaultTerminal,
    background: &dyn Fn(&mut Frame),
    text: &str,
) -> io::Result<()> {
    notice(
        &mut Setup {
            terminal,
            background,
        },
        text,
    )
}
pub(super) fn run(
    handle: Handle,
    terminal: &mut DefaultTerminal,
    background: &dyn Fn(&mut Frame),
) -> io::Result<Option<Added>> {
    run_inner(
        &handle,
        &mut Setup {
            terminal,
            background,
        },
    )
}
enum Entry {
    Native(AddMethod),
    Service(&'static presets::Preset),
    Advanced,
}
fn entries() -> Vec<(Entry, MenuItem)> {
    let mut entries = Vec::new();
    for (method, label, description) in [
        (
            AddMethod::Byok,
            "BYOK · API Key",
            "接入官方 API 或自定义中转站",
        ),
        (
            AddMethod::Oauth,
            "ChatGPT OAuth",
            "通过浏览器或设备码登录，自动读取邮箱",
        ),
        (
            AddMethod::ImportOauth,
            "导入 OAuth",
            "从 auth.json / Cockpit 单账号文件导入",
        ),
        (
            AddMethod::CcSwitch,
            "从 CC Switch 导入",
            "预览后一次性导入，或导入并跟随同步",
        ),
    ] {
        entries.push((
            Entry::Native(method),
            MenuItem {
                label: label.into(),
                description: Some(description.into()),
                enabled: true,
            },
        ));
    }
    entries.extend(presets::PRESETS.iter().map(|p| {
        (
            Entry::Service(p),
            MenuItem {
                label: format!("{} · {}", p.name, p.kind.label()),
                description: Some("登录服务商账号并选择模型".into()),
                enabled: true,
            },
        )
    }));
    entries.push((
        Entry::Advanced,
        MenuItem {
            label: "高级 · 自定义接入".into(),
            description: Some("管理自定义 Provider 插件与已有账号".into()),
            enabled: true,
        },
    ));
    entries
}

struct AccountChoice {
    account: plugins::store::Account,
    name: String,
}
fn run_inner(handle: &Handle, terminal: &mut Setup<'_>) -> io::Result<Option<Added>> {
    loop {
        let preset = {
            let entries = entries();
            let rows = entries
                .iter()
                .map(|(_, row)| row.clone())
                .collect::<Vec<_>>();
            let Some(index) = pick_options(terminal, "添加 Provider › 接入方式", &rows)?
            else {
                return Ok(None);
            };
            match entries[index].0 {
                Entry::Native(method) => {
                    if let Some(added) = run_native(
                        handle.clone(),
                        terminal.terminal,
                        terminal.background,
                        method,
                    )? {
                        return Ok(Some(added));
                    }
                    continue;
                }
                Entry::Service(preset) => Some(preset),
                Entry::Advanced => None,
            }
        };
        let choice = match preset {
            Some(preset) => service_account(handle, terminal, preset)?,
            None => advanced_account(handle, terminal)?,
        };
        let Some(choice) = choice else {
            continue;
        };
        let account = choice.account;
        let account_id = account.id.clone();
        let Some(models) = wait(terminal, handle, "读取此账号可用模型…", true, async move {
            let value = plugins::call(json!({"operation":"models","account":account_id})).await?;
            value
                .as_array()
                .cloned()
                .ok_or_else(|| "模型目录格式不兼容".into())
        })?
        else {
            continue;
        };
        let models: Vec<_> = models
            .into_iter()
            .filter(|m| m["id"].as_str().is_some_and(|s| !s.is_empty()))
            .collect();
        let labels: Vec<_> = models
            .iter()
            .map(|m| {
                let id = m["id"].as_str().unwrap_or_default();
                match m["name"].as_str().filter(|name| *name != id) {
                    Some(name) => format!("{name} · {id}"),
                    None => id.into(),
                }
            })
            .collect();
        let Some(index) = pick(terminal, "选择默认模型 · 输入文字筛选", &labels)?
        else {
            continue;
        };
        let model = models[index]["id"].as_str().unwrap_or_default().to_owned();
        let mut fields = [Field::text("name", "Provider 名称", &choice.name)];
        if !form(terminal, "保存 Provider · 保存后可从项目启动", &mut fields)? {
            continue;
        }
        let name = fields[0].value.clone();
        let id = account.id;
        if let Some(profile) = wait(
            terminal,
            handle,
            "保存 Provider…",
            false,
            blocking(move || plugins::bind(&id, &name, &model)),
        )? {
            return Ok(Some(profile.into()));
        }
    }
}

fn service_account(
    handle: &Handle,
    terminal: &mut Setup<'_>,
    preset: &'static presets::Preset,
) -> io::Result<Option<AccountChoice>> {
    let Some(plugin) = wait(
        terminal,
        handle,
        &format!("正在准备 {}…", preset.name),
        false,
        plugins::ensure_preset(preset),
    )?
    else {
        return Ok(None);
    };
    let Some(accounts) = wait(
        terminal,
        handle,
        "读取已有账号…",
        true,
        blocking(move || {
            let store = plugins::store::Store::new(plugins::store::root());
            let ids: Vec<_> = store
                .plugins()?
                .into_iter()
                .filter(|p| p.package == preset.package)
                .map(|p| p.id)
                .collect();
            Ok(store
                .accounts()?
                .into_iter()
                .filter(|a| ids.contains(&a.plugin) && preset.variant_name(&a.provider).is_some())
                .collect::<Vec<_>>())
        }),
    )?
    else {
        return Ok(None);
    };
    let selected = if accounts.is_empty() {
        0
    } else {
        let mut rows: Vec<_> = accounts
            .iter()
            .map(|a| preset.account_name(&a.provider, &a.name))
            .collect();
        rows.push("登录新账号".into());
        let Some(index) = pick(terminal, &format!("{} · 选择账号", preset.name), &rows)?
        else {
            return Ok(None);
        };
        index
    };
    let account = if let Some(account) = accounts.get(selected) {
        let owner = account.plugin.clone();
        if wait(
            terminal,
            handle,
            "准备账号…",
            false,
            blocking(move || {
                let store = plugins::store::Store::new(plugins::store::root());
                let mut plugin = store
                    .plugins()?
                    .into_iter()
                    .find(|p| p.id == owner)
                    .ok_or("账号连接不可用")?;
                if !plugin.enabled {
                    plugin.enabled = true;
                    store.register(plugin)?;
                }
                Ok(())
            }),
        )?
        .is_none()
        {
            return Ok(None);
        }
        account.clone()
    } else {
        let Some(account) = login_account(handle, terminal, &plugin, Some(preset))? else {
            return Ok(None);
        };
        account
    };
    Ok(Some(AccountChoice {
        name: preset.account_name(&account.provider, &account.name),
        account,
    }))
}
fn advanced_account(
    handle: &Handle,
    terminal: &mut Setup<'_>,
) -> io::Result<Option<AccountChoice>> {
    loop {
        let Some(sources) = wait(
            terminal,
            handle,
            "读取自定义接入…",
            true,
            blocking(|| {
                let store = plugins::store::Store::new(plugins::store::root());
                let mut sources: Vec<_> =
                    store.accounts()?.into_iter().map(Source::Account).collect();
                sources.extend(store.plugins()?.into_iter().map(Source::Plugin));
                sources.push(Source::Install);
                Ok(sources)
            }),
        )?
        else {
            return Ok(None);
        };
        let rows = sources.iter().map(Source::label).collect::<Vec<_>>();
        let Some(index) = pick(terminal, "高级 · 自定义接入", &rows)? else {
            return Ok(None);
        };
        let account = match &sources[index] {
            Source::Install => {
                let mut fields = [
                    Field::text("package", "npm 包（必须带固定版本）", ""),
                    Field::text("id", "插件名称", ""),
                ];
                if form(terminal, "安装自定义插件 · 插件将在本机执行", &mut fields)?
                {
                    wait(
                        terminal,
                        handle,
                        "安装插件（最长 120 秒）…",
                        false,
                        plugins::install(fields[0].value.clone(), fields[1].value.clone(), None),
                    )?;
                }
                continue;
            }
            Source::Account(account) => account.clone(),
            Source::Plugin(plugin) => {
                let Some(account) = login_account(handle, terminal, plugin, None)? else {
                    continue;
                };
                account
            }
        };
        return Ok(Some(AccountChoice {
            name: format!("{} · {}", account.provider, account.name),
            account,
        }));
    }
}

fn login_account(
    handle: &Handle,
    terminal: &mut Setup<'_>,
    plugin: &plugins::store::Plugin,
    preset: Option<&presets::Preset>,
) -> io::Result<Option<plugins::store::Account>> {
    let plugin_id = plugin.id.clone();
    let enabled = plugin.enabled;
    let mut definition = plugin.clone();
    let Some(descriptors) = wait(terminal, handle, "读取登录方式…", true, async move {
        if !enabled {
            definition.enabled = true;
            blocking(move || {
                plugins::store::Store::new(plugins::store::root()).register(definition)
            })
            .await?;
        }
        plugins::call(json!({"operation":"providers","plugin":plugin_id})).await
    })?
    else {
        return Ok(None);
    };
    let mut providers: Vec<Provider> = match serde_json::from_value(descriptors.clone()) {
        Ok(p) => p,
        Err(_) => {
            notice(terminal, "服务返回的登录方式格式不兼容")?;
            return Ok(None);
        }
    };
    if let Some(preset) = preset {
        providers.retain(|p| preset.variant_name(&p.id).is_some());
        providers.sort_by_key(|p| preset.variants.iter().position(|v| v.id == p.id));
    }
    let rows: Vec<_> = providers
        .iter()
        .map(|p| {
            let variant = preset.and_then(|s| s.variants.iter().find(|v| v.id == p.id));
            MenuItem {
                label: variant.map_or_else(|| p.name.clone(), |v| v.name.into()),
                description: variant.map(|v| v.description.into()),
                enabled: true,
            }
        })
        .collect();
    let title = preset.map_or_else(
        || "添加 Provider › 服务商".into(),
        |p| format!("添加 Provider › {} › 地区", p.name),
    );
    loop {
        let Some(index) = pick_options(terminal, &title, &rows)? else {
            return Ok(None);
        };
        let provider = &providers[index];
        let region = &rows[index].label;
        let methods_title = preset.map_or_else(
            || format!("{} › 登录方式", provider.name),
            |p| format!("{} › {region} › 登录方式", p.name),
        );
        loop {
            // Recheck when returning from authorization: the desktop client may have changed.
            let methods = method_rows(provider, preset, &crate::config::home_dir());
            let Some(index) = pick_options(terminal, &methods_title, &methods)? else {
                break;
            };
            let method = &provider.methods[index];
            let mut fields = login_fields(method);
            if !fields.is_empty() && !form(terminal, "登录参数", &mut fields)? {
                continue;
            }
            let adapter = match plugins::auth::adapter(&plugin.id, &provider.id, &descriptors) {
                Ok(a) => a,
                Err(error) => {
                    notice(terminal, &error)?;
                    continue;
                }
            };
            // Browser/code instructions continue to use the existing cancellable auth task.
            crate::ui::terminal::leave();
            let result = handle.block_on(auth::run_in_terminal(
                adapter.as_ref(),
                auth::AuthRequest {
                    method: method.id.clone(),
                    name: None,
                    config: Some(json!({"inputs":inputs(&fields),"open_browser":true}).to_string()),
                    program: PathBuf::new(),
                },
            ));
            *terminal.terminal = crate::ui::terminal::enter().map_err(io::Error::other)?;
            match result {
                Ok(account) => {
                    return Ok(Some(plugins::store::Account {
                        id: account.id,
                        name: account.name,
                        plugin: plugin.id.clone(),
                        provider: provider.id.clone(),
                    }));
                }
                Err(auth::AuthError::Cancelled) => {}
                Err(error) => notice(terminal, &error.to_string())?,
            }
        }
    }
}

async fn blocking<T: Send + 'static>(
    job: impl FnOnce() -> plugins::Result<T> + Send + 'static,
) -> plugins::Result<T> {
    tokio::task::spawn_blocking(job)
        .await
        .map_err(|_| "插件任务中断".to_string())?
}
fn key() -> io::Result<Option<KeyEvent>> {
    if event::poll(Duration::from_millis(80))?
        && let Event::Key(key) = event::read()?
        && key.kind == KeyEventKind::Press
    {
        Ok(Some(key))
    } else {
        Ok(None)
    }
}
fn cancelled(key: KeyEvent) -> bool {
    key.code == KeyCode::Esc
        || (key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c'))
}
fn wait<T: Send + 'static>(
    terminal: &mut Setup<'_>,
    handle: &Handle,
    title: &str,
    cancellable: bool,
    future: impl Future<Output = plugins::Result<T>> + Send + 'static,
) -> io::Result<Option<T>> {
    let (tx, rx) = mpsc::sync_channel(1);
    let _pending = if cancellable {
        Some(crate::background::Task::query(
            handle,
            future,
            move |result| {
                let _ = tx.send(result);
            },
        ))
    } else {
        // An accepted save/install must finish even if terminal rendering fails.
        handle.spawn(async move {
            let _ = tx.send(future.await);
        });
        None
    };
    loop {
        match rx.try_recv() {
            Ok(Ok(value)) => return Ok(Some(value)),
            Ok(Err(error)) => {
                notice(terminal, &error)?;
                return Ok(None);
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                notice(terminal, "插件任务异常结束")?;
                return Ok(None);
            }
            _ => {}
        }
        terminal.draw(|frame| {
            page(
                frame,
                title,
                Vec::new(),
                None,
                if cancellable {
                    "Esc 返回"
                } else {
                    "正在保存或安装，请稍候…"
                },
            )
        })?;
        if let Some(key) = key()?
            && cancelled(key)
            && cancellable
        {
            return Ok(None);
        }
    }
}
fn notice(terminal: &mut Setup<'_>, text: &str) -> io::Result<()> {
    loop {
        terminal.draw(|frame| {
            page(
                frame,
                "添加 Provider",
                vec![text.into()],
                None,
                "Enter / Esc 返回",
            )
        })?;
        if let Some(key) = key()?
            && (cancelled(key) || key.code == KeyCode::Enter)
        {
            return Ok(());
        }
    }
}
#[derive(Default)]
struct Picker {
    query: String,
    selected: usize,
}
impl Picker {
    fn indices(&self, rows: &[String]) -> Vec<usize> {
        let query = self.query.to_lowercase();
        rows.iter()
            .enumerate()
            .filter(|(_, r)| r.to_lowercase().contains(&query))
            .map(|(i, _)| i)
            .collect()
    }
    fn key(&mut self, key: KeyEvent, count: usize) {
        match key.code {
            KeyCode::Up | KeyCode::BackTab => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => {
                self.selected = (self.selected + 1).min(count.saturating_sub(1))
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.selected = 0;
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.query.clear();
                self.selected = 0;
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.query.push(c);
                self.selected = 0;
            }
            _ => {}
        }
    }
}
#[derive(Clone)]
struct MenuItem {
    label: String,
    description: Option<String>,
    enabled: bool,
}
impl From<String> for MenuItem {
    fn from(label: String) -> Self {
        Self {
            label,
            description: None,
            enabled: true,
        }
    }
}
fn login_fields(method: &Method) -> Vec<Field> {
    let mut fields: Vec<_> = method
        .prompts
        .iter()
        .cloned()
        .map(Field::from_input)
        .collect();
    if method.kind == "api" && !fields.iter().any(|field| field.input.key == "key") {
        let mut field = Field::text("key", "API Key", "");
        field.secret = true;
        fields.push(field);
    }
    fields
}
fn method_rows(
    provider: &Provider,
    preset: Option<&presets::Preset>,
    home: &std::path::Path,
) -> Vec<MenuItem> {
    provider
        .methods
        .iter()
        .map(
            |method| match preset.and_then(|p| p.sign_in_option(&provider.id, &method.id, home)) {
                Some(option) => MenuItem {
                    label: if option.available {
                        option.name.into()
                    } else {
                        format!("{}（不可用）", option.name)
                    },
                    description: Some(option.description),
                    enabled: option.available,
                },
                None => method.label.clone().into(),
            },
        )
        .collect()
}
fn pick(terminal: &mut Setup<'_>, title: &str, rows: &[String]) -> io::Result<Option<usize>> {
    let options: Vec<_> = rows.iter().cloned().map(MenuItem::from).collect();
    pick_options(terminal, title, &options)
}
fn selected_option(rows: &[MenuItem], indices: &[usize], selected: usize) -> Option<usize> {
    indices.get(selected).copied().filter(|&i| rows[i].enabled)
}
fn pick_options(
    terminal: &mut Setup<'_>,
    title: &str,
    rows: &[MenuItem],
) -> io::Result<Option<usize>> {
    if rows.is_empty() {
        notice(terminal, "没有可选项；可返回重试或选择其他账号")?;
        return Ok(None);
    }
    let labels: Vec<_> = rows.iter().map(|r| r.label.clone()).collect();
    let mut picker = Picker {
        selected: rows.iter().position(|r| r.enabled).unwrap_or(0),
        ..Picker::default()
    };
    loop {
        let indices = picker.indices(&labels);
        let visible = indices.iter().map(|&i| rows[i].clone()).collect();
        terminal.draw(|frame| {
            menu_page(
                frame,
                title,
                visible,
                Some(picker.selected),
                &format!(
                    "↑↓ 选择 · Enter 进入 · Esc 上一级 · 输入筛选：{}",
                    picker.query
                ),
            )
        })?;
        if let Some(key) = key()? {
            if cancelled(key) {
                return Ok(None);
            }
            if key.code == KeyCode::Enter
                && let Some(index) = selected_option(rows, &indices, picker.selected)
            {
                return Ok(Some(index));
            }
            picker.key(key, indices.len());
        }
    }
}
fn form(terminal: &mut Setup<'_>, title: &str, fields: &mut [Field]) -> io::Result<bool> {
    let mut selected = 0;
    loop {
        let indices = visible(fields);
        selected = selected.min(indices.len().saturating_sub(1));
        let rows = indices
            .iter()
            .map(|&i| format!("{}：{}", fields[i].input.message, fields[i].display()))
            .collect();
        terminal.draw(|frame| {
            page(
                frame,
                title,
                rows,
                Some(selected),
                "Tab / ↑↓ 切换字段 · ←→ 选项 · Ctrl+U 清空 · Enter 确认 · Esc 返回",
            )
        })?;
        if let Some(key) = key()? {
            if cancelled(key) {
                return Ok(false);
            }
            if key.code == KeyCode::Enter {
                return Ok(true);
            }
            match key.code {
                KeyCode::Up | KeyCode::BackTab => selected = selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Tab => {
                    selected = (selected + 1).min(indices.len().saturating_sub(1))
                }
                _ => {
                    if let Some(&index) = indices.get(selected) {
                        let field = &mut fields[index];
                        if field.input.options.is_empty() {
                            match key.code {
                                KeyCode::Backspace => {
                                    field.value.pop();
                                }
                                KeyCode::Char('u')
                                    if key.modifiers.contains(KeyModifiers::CONTROL) =>
                                {
                                    field.value.clear()
                                }
                                KeyCode::Char(c)
                                    if !key.modifiers.contains(KeyModifiers::CONTROL) =>
                                {
                                    field.value.push(c)
                                }
                                _ => {}
                            }
                        } else if matches!(key.code, KeyCode::Left | KeyCode::Right) {
                            let n = field.input.options.len();
                            let at = field
                                .input
                                .options
                                .iter()
                                .position(|o| o.value == field.value)
                                .unwrap_or(0);
                            let next = if key.code == KeyCode::Left {
                                (at + n - 1) % n
                            } else {
                                (at + 1) % n
                            };
                            field.value = field.input.options[next].value.clone();
                        }
                    }
                }
            }
        }
    }
}
fn page(frame: &mut Frame, title: &str, rows: Vec<String>, selected: Option<usize>, hint: &str) {
    menu_page(
        frame,
        title,
        rows.into_iter().map(MenuItem::from).collect(),
        selected,
        hint,
    );
}
fn popup_area(area: Rect, height: u16) -> Rect {
    let width = area.width.saturating_sub(4).min(80);
    let height = height.min(area.height.saturating_sub(2));
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}
fn menu_page(
    frame: &mut Frame,
    title: &str,
    rows: Vec<MenuItem>,
    selected: Option<usize>,
    hint: &str,
) {
    let area = popup_area(frame.area(), u16::MAX);
    let width = area.width.saturating_sub(4).max(1);
    let items: Vec<_> = rows
        .into_iter()
        .map(|row| {
            let style = if row.enabled {
                Style::default()
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let mut lines =
                super::overview::wrap(vec![Line::from(super::overview::clean(&row.label))], width);
            if let Some(description) = row.description {
                lines.extend(super::overview::wrap(
                    vec![
                        Line::from(super::overview::clean(&description))
                            .style(Style::default().fg(Color::Gray)),
                    ],
                    width,
                ));
            }
            ListItem::new(lines).style(style)
        })
        .collect();
    let content_height: usize = items.iter().map(ListItem::height).sum();
    let hints = super::overview::wrap(
        vec![Line::from(super::overview::clean(hint))],
        area.width.saturating_sub(2).max(1),
    );
    let hint_height = hints.len().min(3) as u16;
    // Bound long model lists while sizing short submenus to their actual content.
    let area = popup_area(
        frame.area(),
        (content_height.clamp(1, 12) as u16) + hint_height + 3,
    );
    frame.render_widget(Clear, area);
    let block = Block::bordered().title(format!(" {} ", super::overview::clean(title)));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [body, _, footer] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(hint_height),
    ])
    .areas(inner);
    frame.render_stateful_widget(
        List::new(items)
            .highlight_symbol("› ")
            .highlight_style(Style::new().fg(Color::LightBlue)),
        body,
        &mut ListState::default().with_selected(selected),
    );
    frame.render_widget(
        Paragraph::new(hints)
            .style(Style::new().fg(Color::Gray))
            .wrap(Wrap { trim: false }),
        footer,
    );
}

#[cfg(test)]
mod tests {
    #[test]
    fn api_login_keeps_site_selection_and_adds_a_secret_key_field() {
        let method: super::Method = serde_json::from_value(serde_json::json!({
            "id":"4", "label":"API Key", "type":"api", "prompts":[{
                "key":"site", "message":"网站", "options":[{"label":"智谱","value":"bigmodel"}]
            }]
        }))
        .unwrap();
        let mut fields = super::login_fields(&method);
        assert_eq!(fields.len(), 2);
        assert!(fields[1].secret);
        fields[0].value = "bigmodel".into();
        fields[1].value = "synthetic-secret".into();
        assert_eq!(
            super::inputs(&fields),
            serde_json::json!({"site":"bigmodel","key":"synthetic-secret"})
        );
    }
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn login_submenu_is_compact_preserves_background_and_blocks_unavailable_methods() {
        let home =
            std::env::temp_dir().join(format!("usage-workbuddy-menu-{}", uuid::Uuid::new_v4()));
        let provider: Provider = serde_json::from_value(json!({
            "id":"workbuddy", "name":"WorkBuddy", "methods":[
                {"id":"0","label":"account (browser)","type":"oauth"},
                {"id":"1","label":"desktop","type":"oauth"}
            ]
        }))
        .unwrap();
        let rows = method_rows(&provider, Some(&presets::WORKBUDDY), &home);
        assert_eq!(selected_option(&rows, &[0, 1], 0), Some(0));
        assert_eq!(selected_option(&rows, &[0, 1], 1), None);
        assert_eq!(selected_option(&rows, &[1], 0), None);
        for (width, height) in [(40, 16), (100, 30), (160, 50)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    frame.render_widget(Paragraph::new("首页 · Providers"), frame.area());
                    menu_page(
                        frame,
                        "WorkBuddy › 国内版 › 登录方式",
                        rows.clone(),
                        Some(0),
                        "↑↓ 选择 · Enter 进入 · Esc 上一级",
                    );
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            let text: String = buffer.content.iter().map(|c| c.symbol()).collect();
            let compact: String = text
                .chars()
                .filter(|c| !c.is_whitespace() && *c != '│')
                .collect();
            for expected in [
                "首页",
                "国内版",
                "浏览器登录（推荐）",
                "读取桌面登录（不可用）",
                "仅支持未加密",
                "本机未发现",
                "Esc上一级",
            ] {
                assert!(
                    compact.contains(expected),
                    "{width}x{height}: missing {expected}\n{buffer:?}"
                );
            }
            let border_rows: Vec<_> = (0..height)
                .filter(|&y| (0..width).any(|x| buffer[(x, y)].symbol() == "┌"))
                .collect();
            assert_eq!(border_rows.len(), 1);
            assert!(border_rows[0] > 0);
            assert_eq!(buffer[(0, 0)].symbol(), "首");
            if height >= 30 {
                assert!(
                    border_rows[0] >= height / 4,
                    "short menu should not fill the screen"
                );
            }
        }
    }

    #[test]
    fn popup_fits_even_a_tiny_terminal() {
        for width in 0..12 {
            for height in 0..8 {
                let area = Rect::new(2, 3, width, height);
                let popup = popup_area(area, 10);
                assert!(area.contains((popup.x, popup.y).into()) || popup.is_empty());
                assert!(popup.right() <= area.right());
                assert!(popup.bottom() <= area.bottom());
            }
        }
    }

    #[test]
    fn unified_picker_keeps_every_native_and_service_entry_accessible() {
        use super::AddMethod;
        let entries = entries();
        let methods: Vec<_> = entries
            .iter()
            .filter_map(|(entry, _)| match entry {
                Entry::Native(method) => Some(*method),
                _ => None,
            })
            .collect();
        assert_eq!(
            methods,
            [
                AddMethod::Byok,
                AddMethod::Oauth,
                AddMethod::ImportOauth,
                AddMethod::CcSwitch
            ]
        );
        assert!(
            entries.iter().any(
                |(entry, _)| matches!(entry, Entry::Service(preset) if preset.id == "workbuddy")
            )
        );
        assert!(matches!(entries.last().unwrap().0, Entry::Advanced));
        assert!(
            entries
                .iter()
                .any(|(entry, _)| matches!(entry, Entry::Service(preset) if preset.id == "trae"))
        );
        assert!(
            entries
                .iter()
                .any(|(entry, _)| matches!(entry, Entry::Service(preset) if preset.id == "zcode"))
        );
        let rows: Vec<_> = entries.into_iter().map(|(_, row)| row).collect();
        for width in [40, 100] {
            for (selected, row) in rows.iter().enumerate() {
                let mut terminal = Terminal::new(TestBackend::new(width, 20)).unwrap();
                terminal
                    .draw(|frame| {
                        menu_page(
                            frame,
                            "添加 Provider › 接入方式",
                            rows.clone(),
                            Some(selected),
                            "Enter 进入 · Esc 返回",
                        )
                    })
                    .unwrap();
                let text: String = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|c| c.symbol())
                    .collect();
                let compact: String = text
                    .chars()
                    .filter(|c| !c.is_whitespace() && *c != '│')
                    .collect();
                let label: String = row.label.chars().filter(|c| !c.is_whitespace()).collect();
                assert!(compact.contains(&label), "{width}: missing {label}\n{text}");
                assert!(!text.contains("@magpie-community"));
            }
        }
    }

    #[test]
    fn model_filter_keeps_the_original_selection_index() {
        let rows = vec!["First · model-a".into(), "WorkBuddy · model-b".into()];
        let mut picker = Picker::default();
        for c in "MODEL-B".chars() {
            picker.key(KeyCode::Char(c).into(), 2);
        }
        assert_eq!(picker.indices(&rows), [1]);
        picker.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL), 1);
        assert_eq!(picker.indices(&rows), [0, 1]);
        picker.key(KeyCode::Down.into(), 2);
        assert_eq!(picker.indices(&rows)[picker.selected], 1);
    }

    #[test]
    fn conditional_login_fields_are_hidden_and_never_submitted() {
        let mut fields = vec![
            Field::text("kind", "方式", "browser"),
            Field::from_input(
                serde_json::from_value(json!({
                    "key":"apiKey","message":"密钥","when":{"key":"kind","op":"eq","value":"api"}
                }))
                .unwrap(),
            ),
        ];
        fields[1].value = "synthetic-secret".into();
        assert_eq!(visible(&fields), [0]);
        assert!(inputs(&fields).get("apiKey").is_none());
        fields[0].value = "api".into();
        assert_eq!(visible(&fields), [0, 1]);
        assert_eq!(inputs(&fields)["apiKey"], "synthetic-secret");
        assert!(!fields[1].display().contains("synthetic-secret"));
    }

    #[test]
    fn plugin_entry_and_masked_fields_render_on_small_and_wide_terminals() {
        let mut field = Field::from_input(
            serde_json::from_value(json!({"key":"token","message":"Token"})).unwrap(),
        );
        field.value = "synthetic-secret".into();
        for width in [40, 100] {
            let mut terminal = Terminal::new(TestBackend::new(width, 16)).unwrap();
            terminal
                .draw(|frame| {
                    page(
                        frame,
                        "添加 Provider",
                        vec![
                            "登录插件 · workbuddy · 0.1.24".into(),
                            Source::Install.label(),
                            field.display(),
                        ],
                        Some(0),
                        "Enter 确认 · Esc 返回",
                    )
                })
                .unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect();
            let compact = text.split_whitespace().collect::<String>();
            assert!(compact.contains("workbuddy"));
            assert!(compact.contains("安装自定义插件"));
            assert!(!text.contains("synthetic-secret"));
        }
    }
}
