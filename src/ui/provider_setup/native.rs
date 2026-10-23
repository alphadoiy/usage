//! Native BYOK/OAuth/import forms shared by every Provider entry point.
use super::picker::{self, Picker};
use crate::codex_manager::{self, AuthMode, Manager, Profile, config, oauth};
use crate::provider_catalog::{CatalogService, ModelSource, Models};
use crate::ui::forms::{edit_input, expand_path, modal};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::style::{Color, Stylize};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::Duration;
use tokio::runtime::Handle;

struct Form {
    fields: Vec<String>,
    selected: usize,
    editing: Option<(String, i64, String)>,
    auth_mode: AuthMode,
    plugin: bool,
    picker: Option<Picker>,
    /// None until requested; a saved Provider is fetched when its form opens.
    models: Option<Models>,
}
impl Form {
    fn new(
        fields: Vec<String>,
        editing: Option<(String, i64, String)>,
        auth_mode: AuthMode,
        plugin: bool,
    ) -> Self {
        Self {
            fields,
            selected: 0,
            editing,
            auth_mode,
            plugin,
            picker: None,
            models: None,
        }
    }
    fn model_field(&self) -> usize {
        if self.auth_mode == AuthMode::Oauth {
            1
        } else {
            2
        }
    }
    fn effort_field(&self) -> usize {
        if self.auth_mode == AuthMode::Oauth {
            2
        } else {
            3
        }
    }
    /// A typed key lists models at the typed endpoint; otherwise the saved Provider's query is used.
    fn model_source(&self) -> Option<ModelSource> {
        if self.auth_mode == AuthMode::Byok && !self.plugin && !self.fields[4].trim().is_empty() {
            return Some(ModelSource::Endpoint {
                base: self.fields[1].trim().into(),
                key: self.fields[4].trim().into(),
            });
        }
        self.editing
            .as_ref()
            .map(|(id, _, _)| ModelSource::Saved(id.clone()))
    }
    fn open_picker(&mut self, field: usize, query: String) {
        let optional = self.auth_mode == AuthMode::Oauth;
        let (label, items, models) = if field == self.model_field() {
            ("模型", picker::models(self.models.as_ref(), optional), true)
        } else {
            ("推理强度", picker::efforts(optional), false)
        };
        self.picker = Some(Picker {
            field,
            label,
            query,
            cursor: 0,
            items: picker::with_current(items, &self.fields[field]),
            models,
        });
    }
    /// A model list that arrives while its picker is open replaces the options, keeping the query.
    fn models_arrived(&mut self, models: Models) {
        self.models = Some(models);
        if let Some(open) = self.picker.take() {
            let (field, query, cursor) = (open.field, open.query, open.cursor);
            self.open_picker(field, query);
            if let Some(p) = &mut self.picker {
                p.cursor = cursor.min(p.matches().len().saturating_sub(1));
            }
        }
    }
    fn fields(&self) -> Vec<(usize, &'static str)> {
        if self.auth_mode == AuthMode::Oauth {
            vec![
                (0, "名称"),
                (1, "模型（可留空）"),
                (2, "推理强度（可留空）"),
            ]
        } else if self.plugin {
            vec![(0, "名称"), (2, "模型"), (3, "推理强度")]
        } else {
            vec![
                (0, "名称"),
                (1, "Base URL"),
                (2, "模型"),
                (3, "推理强度"),
                (4, "API Key"),
            ]
        }
    }
}
enum Prompt {
    LoginMethod(String, Option<String>),
    AuthSource(String),
    AuthImport(PathBuf, oauth::Identity),
    Source(String),
    Import(PathBuf, String),
}

/// Native onboarding reuses the account editor and authentication flow in a parent popup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ui) enum AddMethod {
    Byok,
    Oauth,
    ImportOauth,
    CcSwitch,
}
fn add_state(method: AddMethod) -> (Option<Form>, Option<Prompt>) {
    match method {
        AddMethod::Byok => (Some(byok_form()), None),
        AddMethod::Oauth => (None, Some(Prompt::LoginMethod(String::new(), None))),
        AddMethod::ImportOauth => (
            None,
            Some(Prompt::AuthSource(
                crate::config::home_dir()
                    .join(".codex/auth.json")
                    .to_string_lossy()
                    .into(),
            )),
        ),
        AddMethod::CcSwitch => (
            None,
            Some(Prompt::Source(
                codex_manager::ccswitch::default_path()
                    .to_string_lossy()
                    .into(),
            )),
        ),
    }
}
pub(in crate::ui) fn run(
    handle: Handle,
    terminal: &mut ratatui::DefaultTerminal,
    background: &dyn Fn(&mut ratatui::Frame),
    method: AddMethod,
) -> io::Result<Option<super::Added>> {
    let mut manager = Manager::open_default().map_err(io::Error::other)?;
    let (form, prompt) = add_state(method);
    run_inner(handle, terminal, background, &mut manager, form, prompt)
}
pub(in crate::ui) fn edit(
    handle: Handle,
    terminal: &mut ratatui::DefaultTerminal,
    background: &dyn Fn(&mut ratatui::Frame),
    profile: &Profile,
) -> io::Result<Option<super::Added>> {
    let mut manager = Manager::open_default().map_err(io::Error::other)?;
    let rev = manager
        .revision(&profile.id, profile.revision)
        .map_err(io::Error::other)?;
    let doc = config::parse(&rev.config).map_err(io::Error::other)?;
    let effort = doc
        .get("model_reasoning_effort")
        .and_then(toml_edit::Item::as_str)
        .unwrap_or_default()
        .to_owned();
    let fields = if profile.auth_mode == AuthMode::Oauth {
        let model = doc
            .get("model")
            .and_then(toml_edit::Item::as_str)
            .unwrap_or_default()
            .to_owned();
        vec![profile.name.clone(), model, effort]
    } else {
        vec![
            profile.name.clone(),
            profile.base_url.clone(),
            profile.model.clone(),
            effort,
            String::new(),
        ]
    };
    let form = Form::new(
        fields,
        Some((profile.id.clone(), profile.revision, rev.config)),
        profile.auth_mode,
        crate::plugins::endpoint_account(&profile.base_url).is_some(),
    );
    let added = run_inner(handle, terminal, background, &mut manager, Some(form), None)?;
    Ok(added.map(|mut added| {
        added.notice = "已保存 Agent 配置".into();
        added
    }))
}
pub(in crate::ui) fn relogin(
    handle: Handle,
    terminal: &mut ratatui::DefaultTerminal,
    background: &dyn Fn(&mut ratatui::Frame),
    profile: &Profile,
) -> io::Result<Option<super::Added>> {
    let mut manager = Manager::open_default().map_err(io::Error::other)?;
    run_inner(
        handle,
        terminal,
        background,
        &mut manager,
        None,
        Some(Prompt::LoginMethod(
            profile.name.clone(),
            Some(profile.id.clone()),
        )),
    )
}
fn run_inner(
    handle: Handle,
    terminal: &mut ratatui::DefaultTerminal,
    background: &dyn Fn(&mut ratatui::Frame),
    manager: &mut Manager,
    mut form: Option<Form>,
    mut prompt: Option<Prompt>,
) -> io::Result<Option<super::Added>> {
    let mut notice = String::new();
    let mut models_rx = match &mut form {
        Some(f) if f.editing.is_some() => fetch_models(&handle, f),
        _ => None,
    };
    loop {
        if let (Some(rx), Some(f)) = (&models_rx, &mut form)
            && let Ok(models) = rx.try_recv()
        {
            f.models_arrived(models);
            models_rx = None;
        }
        if !notice.is_empty() {
            super::show_notice(terminal, background, &notice)?;
            notice.clear();
        }
        if form.is_none() && prompt.is_none() {
            return Ok(None);
        }
        terminal.draw(|frame| {
            background(frame);
            if let Some(form) = &form {
                draw_form(frame, form);
            }
            if let Some(prompt) = &prompt {
                draw_prompt(frame, prompt);
            }
        })?;
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(None);
        }
        if let Some(edit) = &mut form {
            let control = key.modifiers.contains(KeyModifiers::CONTROL);
            if let Some(open) = &mut edit.picker {
                match key.code {
                    KeyCode::Esc => edit.picker = None,
                    KeyCode::Up => open.move_by(-1),
                    KeyCode::Down => open.move_by(1),
                    KeyCode::Char('p') if control => open.move_by(-1),
                    KeyCode::Char('n') if control => open.move_by(1),
                    KeyCode::Char('r') if control && open.models => {
                        models_rx = fetch_models(&handle, edit)
                    }
                    KeyCode::Char('u') if control => {
                        open.query.clear();
                        open.cursor = 0;
                    }
                    KeyCode::Enter => {
                        if let Some(value) = open.choice() {
                            edit.fields[open.field] = value;
                            edit.picker = None;
                        }
                    }
                    code if !control => {
                        edit_input(&mut open.query, code);
                        open.cursor = 0;
                    }
                    _ => {}
                }
                continue;
            }
            let fields = edit.fields();
            let index = fields[edit.selected].0;
            let choosable = index == edit.model_field() || index == edit.effort_field();
            match key.code {
                KeyCode::Esc => form = None,
                KeyCode::Tab | KeyCode::Down => edit.selected = (edit.selected + 1) % fields.len(),
                KeyCode::BackTab | KeyCode::Up => {
                    edit.selected = (edit.selected + fields.len() - 1) % fields.len()
                }
                KeyCode::Backspace => {
                    edit.fields[index].pop();
                }
                KeyCode::Char('u') if control => edit.fields[index].clear(),
                // Enter or Space opens the options; typing starts filtering them.
                code @ (KeyCode::Enter | KeyCode::Char(_))
                    if choosable && (code == KeyCode::Enter || !control) =>
                {
                    if index == edit.model_field()
                        && models_rx.is_none()
                        && !matches!(edit.models, Some(Models::Ready(_)))
                    {
                        models_rx = fetch_models(&handle, edit);
                    }
                    let query = match code {
                        KeyCode::Char(ch) if ch != ' ' => ch.to_string(),
                        _ => String::new(),
                    };
                    edit.open_picker(index, query);
                }
                KeyCode::Char(ch) if !control => edit.fields[index].push(ch),
                KeyCode::Enter | KeyCode::Char('s') if key.code == KeyCode::Enter || control => {
                    match save_form(manager, edit) {
                        Ok(p) => {
                            return Ok(Some(p.into()));
                        }
                        Err(e) => notice = e.to_string(),
                    }
                }
                _ => {}
            }
            continue;
        }
        if let Some(current) = prompt.take() {
            match current {
                Prompt::LoginMethod(name, account) => {
                    if [
                        KeyCode::Enter,
                        KeyCode::Char('1'),
                        KeyCode::Char('b'),
                        KeyCode::Char('2'),
                        KeyCode::Char('d'),
                    ]
                    .contains(&key.code)
                    {
                        let device = [KeyCode::Char('2'), KeyCode::Char('d')].contains(&key.code);
                        crate::ui::terminal::leave();
                        let result = {
                            let _runtime = handle.enter();
                            let adapter =
                                crate::hub::shared().authentication(crate::auth::AuthSpec {
                                    service: "codex_oauth".into(),
                                    context: crate::auth::AuthContext {
                                        root: manager.root().into(),
                                        account: account.clone(),
                                    },
                                });
                            adapter
                                .map_err(|error| crate::auth::AuthError::Failed(error.to_string()))
                                .and_then(|adapter| {
                                    handle.block_on(crate::auth::run_in_terminal(
                                        adapter.as_ref(),
                                        crate::auth::AuthRequest {
                                            name: (!name.trim().is_empty()).then_some(name.clone()),
                                            config: None,
                                            program: "codex".into(),
                                            method: if device {
                                                crate::auth::AuthMethod::DEVICE_CODE.id
                                            } else {
                                                crate::auth::AuthMethod::BROWSER.id
                                            }
                                            .into(),
                                        },
                                    ))
                                })
                        };
                        *terminal = crate::ui::terminal::enter().map_err(io::Error::other)?;
                        match result {
                            Ok(account) => {
                                return manager
                                    .resolve(&account.id)
                                    .map(|p| Some(p.into()))
                                    .map_err(io::Error::other);
                            }
                            Err(crate::auth::AuthError::Cancelled) => return Ok(None),
                            Err(error) => {
                                notice = error.to_string();
                                prompt = Some(Prompt::LoginMethod(name, account));
                            }
                        }
                    } else if key.code != KeyCode::Esc {
                        prompt = Some(Prompt::LoginMethod(name, account));
                    }
                }
                Prompt::AuthSource(mut text) => match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Enter => {
                        let path = expand_path(&text);
                        match oauth::read_auth(&path) {
                            Ok(auth) => prompt = Some(Prompt::AuthImport(path, auth.identity)),
                            Err(e) => {
                                notice = e.to_string();
                                prompt = Some(Prompt::AuthSource(text));
                            }
                        }
                    }
                    code => {
                        if code == KeyCode::Char('u')
                            && key.modifiers.contains(KeyModifiers::CONTROL)
                        {
                            text.clear();
                        } else if !key.modifiers.contains(KeyModifiers::CONTROL) {
                            edit_input(&mut text, code);
                        }
                        prompt = Some(Prompt::AuthSource(text));
                    }
                },
                Prompt::AuthImport(path, identity) => {
                    if key.code == KeyCode::Enter {
                        match manager.import_oauth(None, None, &path, None) {
                            Ok(profile) => return Ok(Some(profile.into())),
                            Err(error) => {
                                notice = error.to_string();
                                prompt = Some(Prompt::AuthSource(path.to_string_lossy().into()));
                            }
                        }
                    } else if key.code == KeyCode::Esc {
                        prompt = Some(Prompt::AuthSource(path.to_string_lossy().into()));
                    } else if key.code != KeyCode::Esc {
                        prompt = Some(Prompt::AuthImport(path, identity));
                    }
                }
                Prompt::Source(mut text) => match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Enter => {
                        let path = expand_path(&text);
                        match manager.import_ccswitch(&path, false, &[], true) {
                            Ok(report) => prompt = Some(Prompt::Import(path, report.to_string())),
                            Err(e) => {
                                notice = e.to_string();
                                prompt = Some(Prompt::Source(text));
                            }
                        }
                    }
                    code => {
                        if code == KeyCode::Char('u')
                            && key.modifiers.contains(KeyModifiers::CONTROL)
                        {
                            text.clear();
                        } else if !key.modifiers.contains(KeyModifiers::CONTROL) {
                            edit_input(&mut text, code);
                        }
                        prompt = Some(Prompt::Source(text));
                    }
                },
                Prompt::Import(path, preview) => {
                    if key.code == KeyCode::Enter || key.code == KeyCode::Char('f') {
                        match manager.import_ccswitch(
                            &path,
                            key.code == KeyCode::Char('f'),
                            &[],
                            false,
                        ) {
                            Ok(report) => {
                                return Ok(Some(super::Added {
                                    profile: None,
                                    notice: report.to_string(),
                                }));
                            }
                            Err(e) => {
                                notice = e.to_string();
                                prompt = Some(Prompt::Import(path.clone(), preview.clone()));
                            }
                        }
                    } else if key.code == KeyCode::Esc {
                        prompt = Some(Prompt::Source(path.to_string_lossy().into()));
                    } else if key.code != KeyCode::Esc {
                        prompt = Some(Prompt::Import(path, preview));
                    }
                }
            }
            continue;
        }
    }
}

fn save_form(manager: &mut Manager, form: &Form) -> codex_manager::Result<Profile> {
    let fields = &form.fields;
    if form.auth_mode == AuthMode::Oauth {
        let (id, revision, original) = form
            .editing
            .as_ref()
            .ok_or_else(|| codex_manager::Error::message("OAuth 请通过登录添加"))?;
        let text = oauth::patch_config(original, Some(&fields[1]), Some(&fields[2]))?;
        return manager.update(id, *revision, &text, None, Some(&fields[0]));
    }
    let effort = (!fields[3].is_empty()).then_some(fields[3].as_str());
    if let Some((id, revision, original)) = &form.editing {
        let text = config::patch(original, Some(&fields[1]), Some(&fields[2]), effort)?;
        manager.update(
            id,
            *revision,
            &text,
            (!fields[4].is_empty()).then_some(fields[4].as_str()),
            Some(&fields[0]),
        )
    } else {
        manager.add(
            &fields[0],
            &config::new_config(&fields[1], &fields[2], effort)?,
            &fields[4],
        )
    }
}
fn byok_form() -> Form {
    Form::new(
        vec![
            String::new(),
            "https://api.openai.com/v1".into(),
            String::new(),
            "high".into(),
            String::new(),
        ],
        None,
        AuthMode::Byok,
        false,
    )
}
fn fetch_models(handle: &Handle, form: &mut Form) -> Option<mpsc::Receiver<Models>> {
    let Some(source) = form.model_source() else {
        form.models_arrived(Models::Unavailable(
            "填写 Base URL 和 API Key 后可获取".into(),
        ));
        return None;
    };
    form.models_arrived(Models::Loading);
    let (tx, rx) = mpsc::channel();
    let service = Arc::new(CatalogService::from_environment());
    handle.spawn(async move {
        let _ = tx.send(service.models(source).await);
    });
    Some(rx)
}

fn draw_form(frame: &mut ratatui::Frame, form: &Form) {
    let area = modal(frame.area(), 15);
    frame.render_widget(Clear, area);
    let mut lines = vec![
        Line::from(if form.auth_mode == AuthMode::Oauth {
            "OAuth：模型留空使用默认；保存配置不会复制登录凭据"
        } else if form.plugin {
            "Agent 配置 · 共用原订阅账号和额度"
        } else if form.editing.is_some() {
            "Agent 配置 · 保存后生成新版本；旧会话保留原版本"
        } else {
            "添加 BYOK Provider · Agent 配置"
        }),
        Line::from(""),
    ];
    let fields = form.fields();
    for (visible, (index, label)) in fields.iter().copied().enumerate() {
        let selected = visible == form.selected;
        let choosable = index == form.model_field() || index == form.effort_field();
        let mut text = if index == 4 {
            if form.fields[index].is_empty() && form.editing.is_some() {
                "（留空保留原密钥）".into()
            } else {
                "•".repeat(form.fields[index].chars().count().min(32))
            }
        } else if choosable && form.fields[index].is_empty() {
            "（默认）".into()
        } else {
            form.fields[index].clone()
        };
        if selected && choosable {
            text.push_str("  ▾ Enter 选择或输入");
        } else if selected
            && !(index == 4 && form.fields[index].is_empty() && form.editing.is_some())
        {
            text.push('▏');
        }
        let line = Line::from(format!(
            "{} {label}: {text}",
            if selected { "›" } else { " " }
        ));
        lines.push(if selected {
            line.fg(Color::LightBlue)
        } else {
            line
        });
        lines.push(Line::from(""));
    }
    let choosable = fields
        .get(form.selected)
        .is_some_and(|(i, _)| *i == form.model_field() || *i == form.effort_field());
    lines.push(Line::from(if choosable {
        "Tab / ↑↓ 换字段 · Enter 选择 · Ctrl+S 保存 · Esc 取消"
    } else {
        "Tab / ↑↓ 换字段 · Ctrl+U 清空 · Enter / Ctrl+S 保存 · Esc 取消"
    }));
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered())
            .wrap(Wrap { trim: false }),
        area,
    );
    if let Some(open) = &form.picker {
        const ROWS: usize = 8;
        let area = modal(frame.area(), ROWS as u16 + 9);
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(open.lines(form.models.as_ref(), ROWS))
                .block(Block::bordered().title(format!("选择{}", open.label)))
                .wrap(Wrap { trim: false }),
            area,
        );
    }
}

fn draw_prompt(frame: &mut ratatui::Frame, prompt: &Prompt) {
    let (title, text) = match prompt {
        Prompt::LoginMethod(name, _) => (
            "ChatGPT 登录方式",
            format!(
                "账号：{}\n\n1 / b / Enter  浏览器登录\n2 / d  设备码登录（需账号启用设备授权）\n\n凭据保存在独立账号目录，Codex 自动刷新。\n同账号可多开，共用登录；运行中不能替换登录。\n授权中 Ctrl+C 取消 · Esc 返回",
                if name.trim().is_empty() {
                    "登录后自动读取邮箱"
                } else {
                    name.as_str()
                }
            ),
        ),
        Prompt::AuthSource(path) => (
            "OAuth 登录文件",
            format!(
                "{path}\n\n支持官方 auth.json 或 Cockpit 单账号 JSON。\n只读来源，导入后独立保存。\nEnter 预览 · Ctrl+U 清空 · Esc 取消"
            ),
        ),
        Prompt::AuthImport(_, identity) => (
            "OAuth 导入预览",
            format!(
                "{}\n账号：{}\n\n包含可刷新登录凭据；本地文件权限 600。\n重复身份更新原账号，运行中不能更新。\nEnter 导入 · Esc 取消",
                identity.label(),
                identity.account_id
            ),
        ),
        Prompt::Source(path) => (
            "CC Switch 数据库",
            format!("{path}\n\nEnter 预览 · Esc 取消"),
        ),
        Prompt::Import(_, preview) => (
            "导入预览",
            format!("{preview}\nEnter 一次性导入 · f 导入并跟随 · Esc 取消"),
        ),
    };
    let area = modal(frame.area(), 18);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(text)
            .block(Block::bordered().title(title))
            .wrap(Wrap { trim: false }),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn editing_a_plugin_binding_exposes_agent_fields_and_preserves_shared_credentials() {
        let root =
            std::env::temp_dir().join(format!("usage-provider-edit-{}", uuid::Uuid::new_v4()));
        let mut manager = Manager::open(root.clone()).unwrap();
        let endpoint = crate::plugins::account_endpoint(&uuid::Uuid::new_v4().to_string());
        let config = config::new_config(&endpoint, "model-a", Some("high")).unwrap();
        let profile = manager
            .add("Subscription", &config, "synthetic-inference-key")
            .unwrap();
        let mut form = Form::new(
            vec![
                profile.name.clone(),
                endpoint.clone(),
                "model-b".into(),
                "low".into(),
                String::new(),
            ],
            Some((profile.id.clone(), profile.revision, config)),
            AuthMode::Byok,
            true,
        );
        assert_eq!(
            form.fields().iter().map(|(i, _)| *i).collect::<Vec<_>>(),
            [0, 2, 3]
        );
        let updated = save_form(&mut manager, &form).unwrap();
        assert_eq!(updated.id, profile.id);
        assert_eq!(updated.base_url, endpoint);
        assert_eq!(updated.model, "model-b");
        assert_eq!(updated.revision, profile.revision + 1);
        let database = rusqlite::Connection::open_with_flags(
            root.join("manager.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let secret_ref = |revision: i64| {
            database
                .query_row(
                    "SELECT secret_ref FROM revisions WHERE profile_id=?1 AND revision=?2",
                    rusqlite::params![profile.id, revision],
                    |row| row.get::<_, String>(0),
                )
                .unwrap()
        };
        assert_eq!(
            secret_ref(profile.revision),
            secret_ref(updated.revision),
            "editing an Agent must retain the shared credential reference"
        );
        // A second configuration for the same account retains the same account endpoint.
        let copy = manager.copy(&updated.id, "Subscription copy").unwrap();
        assert_eq!(copy.base_url, endpoint);
        form.fields[2] = "model-c".into();
        assert!(
            save_form(&mut manager, &form).is_err(),
            "stale edits must not overwrite newer revisions"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_add_entries_reuse_existing_forms_and_authentication_prompts() {
        let (form, prompt) = add_state(AddMethod::Byok);
        let form = form.unwrap();
        assert!(prompt.is_none());
        assert_eq!(form.auth_mode, AuthMode::Byok);
        assert_eq!(form.fields[1], "https://api.openai.com/v1");
        assert!(form.fields[4].is_empty());
        assert!(form.editing.is_none());
        for (method, expected) in [
            (AddMethod::Oauth, "ChatGPT 登录方式"),
            (AddMethod::ImportOauth, "OAuth 登录文件"),
            (AddMethod::CcSwitch, "CC Switch 数据库"),
        ] {
            let (form, prompt) = add_state(method);
            assert!(form.is_none());
            let prompt = prompt.unwrap();
            for width in [40, 100] {
                let mut terminal = Terminal::new(TestBackend::new(width, 30)).unwrap();
                terminal
                    .draw(|frame| {
                        frame.render_widget(Paragraph::new("首页"), frame.area());
                        draw_prompt(frame, &prompt);
                    })
                    .unwrap();
                let text: String = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|c| c.symbol())
                    .collect();
                let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
                assert!(compact.contains(&expected.replace(' ', "")));
                assert!(compact.contains("首页"));
            }
        }
    }

    #[test]
    fn oauth_options_and_identity_preview_are_visible_without_tokens() {
        let identity = oauth::Identity {
            email: "fixture@example.test".into(),
            account_id: "fixture-workspace".into(),
            user_id: "fixture-user".into(),
            organization_id: None,
            plan: Some("plus".into()),
        };
        for prompt in [
            Prompt::LoginMethod("Work".into(), None),
            Prompt::AuthImport("fixture.json".into(), identity),
        ] {
            for (width, height) in [(100, 30), (40, 20)] {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal.draw(|frame| draw_prompt(frame, &prompt)).unwrap();
                let text: String = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|c| c.symbol())
                    .collect();
                match &prompt {
                    Prompt::LoginMethod(_, _) => assert!(
                        text.chars()
                            .filter(|c| !c.is_whitespace())
                            .collect::<String>()
                            .contains("浏览器登录")
                    ),
                    Prompt::AuthImport(_, _) => assert!(text.contains("fixture@example.test")),
                    _ => unreachable!(),
                }
                assert!(!text.contains("refresh_token"));
            }
        }
    }

    #[test]
    fn model_and_effort_fields_pick_fetched_options_or_custom_values() {
        let mut form = Form::new(
            vec![
                "Trae".into(),
                String::new(),
                "model-a".into(),
                "high".into(),
                String::new(),
            ],
            Some(("fixture".into(), 1, String::new())),
            AuthMode::Byok,
            true,
        );
        assert!(matches!(form.model_source(), Some(ModelSource::Saved(id)) if id == "fixture"));
        form.open_picker(form.model_field(), String::new());
        assert_eq!(
            form.picker.as_ref().unwrap().items.len(),
            1,
            "only the current value before fetch"
        );
        // A list arriving while the picker is open replaces its options and keeps the query.
        form.picker.as_mut().unwrap().query = "flash".into();
        form.models_arrived(Models::Ready(vec![
            "DeepSeek-V4-Flash".into(),
            "model-b".into(),
        ]));
        let open = form.picker.as_ref().unwrap();
        assert_eq!(open.query, "flash");
        assert_eq!(open.choice().as_deref(), Some("DeepSeek-V4-Flash"));
        for (width, height) in [(100, 30), (40, 24)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| draw_form(frame, &form)).unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect();
            let text: String = text.chars().filter(|c| !c.is_whitespace()).collect();
            assert!(text.contains("选择模型"), "{text}");
            assert!(text.contains("DeepSeek-V4-Flash"));
        }
        form.picker.as_mut().unwrap().query = "custom-model".into();
        assert_eq!(
            form.picker.as_ref().unwrap().choice().as_deref(),
            Some("custom-model")
        );
        form.open_picker(form.effort_field(), "x".into());
        assert_eq!(
            form.picker.as_ref().unwrap().choice().as_deref(),
            Some("xhigh")
        );
        // A key typed into a new BYOK form lists models at the typed endpoint.
        let mut form = byok_form();
        assert!(form.model_source().is_none());
        form.fields[4] = "synthetic-key".into();
        assert!(
            matches!(form.model_source(), Some(ModelSource::Endpoint { base, .. }) if base == "https://api.openai.com/v1")
        );
    }

    #[test]
    fn key_field_is_masked_on_wide_and_narrow_terminals() {
        let mut form = Form::new(
            vec![
                "A".into(),
                "https://example.com/v1".into(),
                "m".into(),
                "high".into(),
                "fixture-ui-secret".into(),
            ],
            None,
            AuthMode::Byok,
            false,
        );
        form.selected = 4;
        for (width, height) in [(100, 30), (40, 18), (20, 10)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| draw_form(frame, &form)).unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(!text.contains("fixture-ui-secret"));
            if width == 100 {
                assert!(text.contains("••••"));
            }
        }
    }
}
