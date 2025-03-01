mod input;
mod role;
mod session;

pub use self::input::{Input, InputContext};
use self::role::Role;
use self::session::{Session, TEMP_SESSION_NAME};

use crate::client::{
    create_client_config, list_client_types, list_models, ClientConfig, ExtraConfig, Message,
    Model, OpenAIClient, SendData,
};
use crate::render::{MarkdownRender, RenderOptions};
use crate::utils::{get_env_name, light_theme_from_colorfgbg, now, render_prompt, set_text};

use anyhow::{anyhow, bail, Context, Result};
use inquire::{Confirm, Select, Text};
use is_terminal::IsTerminal;
use parking_lot::RwLock;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::{
    env,
    fs::{create_dir_all, read_dir, read_to_string, remove_file, File, OpenOptions},
    io::{stdout, Write},
    path::{Path, PathBuf},
    process::exit,
    sync::Arc,
};
use syntect::highlighting::ThemeSet;

/// Monokai Extended
const DARK_THEME: &[u8] = include_bytes!("../../assets/monokai-extended.theme.bin");
const LIGHT_THEME: &[u8] = include_bytes!("../../assets/monokai-extended-light.theme.bin");

const CONFIG_FILE_NAME: &str = "config.yaml";
const ROLES_FILE_NAME: &str = "roles.yaml";
const MESSAGES_FILE_NAME: &str = "messages.md";
const SESSIONS_DIR_NAME: &str = "sessions";

const CLIENTS_FIELD: &str = "clients";

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// LLM model
    #[serde(rename(serialize = "model", deserialize = "model"))]
    pub model_id: Option<String>,
    /// GPT temperature, between 0 and 2
    #[serde(rename(serialize = "temperature", deserialize = "temperature"))]
    pub default_temperature: Option<f64>,
    /// Dry-run flag
    pub dry_run: bool,
    /// Whether to save the message
    pub save: bool,
    /// Whether to disable highlight
    pub highlight: bool,
    /// Whether to use a light theme
    pub light_theme: bool,
    /// Specify the text-wrapping mode (no, auto, <max-width>)
    pub wrap: Option<String>,
    /// Whether wrap code block
    pub wrap_code: bool,
    /// Automatically copy the last output to the clipboard
    pub auto_copy: bool,
    /// REPL keybindings. (emacs, vi)
    pub keybindings: Keybindings,
    /// Set a default role or session (role:<name>, session:<name>)
    pub prelude: String,
    /// Compress session if tokens exceed this value (>=1000)
    pub compress_threshold: usize,
    /// The prompt for summarizing session messages
    pub summarize_prompt: String,
    // The prompt for the summary of the session
    pub summary_prompt: String,
    /// REPL left prompt
    pub left_prompt: String,
    /// REPL right prompt
    pub right_prompt: String,
    /// Setup clients
    pub clients: Vec<ClientConfig>,
    /// Predefined roles
    #[serde(skip)]
    pub roles: Vec<Role>,
    /// Current selected role
    #[serde(skip)]
    pub role: Option<Role>,
    /// Current session
    #[serde(skip)]
    pub session: Option<Session>,
    #[serde(skip)]
    pub model: Model,
    #[serde(skip)]
    pub last_message: Option<(Input, String)>,
    #[serde(skip)]
    pub temperature: Option<f64>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model_id: None,
            default_temperature: None,
            save: true,
            highlight: true,
            dry_run: false,
            light_theme: false,
            wrap: None,
            wrap_code: false,
            auto_copy: false,
            keybindings: Default::default(),
            prelude: String::new(),
            compress_threshold: 2000,
            summarize_prompt: "Summarize the discussion briefly in 200 words or less to use as a prompt for future context.".to_string(),
            summary_prompt: "This is a summary of the chat history as a recap: ".into(),
            left_prompt: "{color.green}{?session {session}{?role /}}{role}{color.cyan}{?session )}{!session >}{color.reset} ".to_string(),
            right_prompt: "{color.purple}{?session {?consume_tokens {consume_tokens}({consume_percent}%)}{!consume_tokens {consume_tokens}}}{color.reset}"
                .to_string(),
            clients: vec![ClientConfig::default()],
            roles: vec![],
            role: None,
            session: None,
            model: Default::default(),
            last_message: None,
            temperature: None,
        }
    }
}

pub type GlobalConfig = Arc<RwLock<Config>>;

impl Config {
    pub fn init(is_interactive: bool) -> Result<Self> {
        let config_path = Self::config_file()?;

        let api_key = env::var("OPENAI_API_KEY").ok();

        let exist_config_path = config_path.exists();
        if is_interactive && api_key.is_none() && !exist_config_path {
            create_config_file(&config_path)?;
        }
        let mut config = if api_key.is_some() && !exist_config_path {
            Self::default()
        } else {
            Self::load_config(&config_path)?
        };

        // Compatible with old configuration files
        if exist_config_path {
            config.compat_old_config(&config_path)?;
        }

        if let Some(wrap) = config.wrap.clone() {
            config.set_wrap(&wrap)?;
        }

        config.temperature = config.default_temperature;

        config.load_roles()?;

        config.setup_model()?;
        config.setup_highlight();
        config.setup_light_theme()?;

        setup_logger()?;

        Ok(config)
    }

    pub fn prelude(&mut self) -> Result<()> {
        let prelude = self.prelude.clone();
        let err_msg = || format!("Invalid prelude '{}", prelude);
        match prelude.split_once(':') {
            Some(("role", name)) => {
                if self.role.is_none() && self.session.is_none() {
                    self.set_role(name).with_context(err_msg)?;
                }
            }
            Some(("session", name)) => {
                if self.session.is_none() {
                    self.start_session(Some(name)).with_context(err_msg)?;
                }
            }
            Some(_) => {
                bail!("{}", err_msg())
            }
            None => {}
        }
        Ok(())
    }

    pub fn retrieve_role(&self, name: &str) -> Result<Role> {
        self.roles
            .iter()
            .find(|v| v.match_name(name))
            .map(|v| {
                let mut role = v.clone();
                role.complete_prompt_args(name);
                role
            })
            .ok_or_else(|| anyhow!("Unknown role `{name}`"))
    }

    pub fn config_dir() -> Result<PathBuf> {
        let env_name = get_env_name("config_dir");
        let path = if let Some(v) = env::var_os(env_name) {
            PathBuf::from(v)
        } else {
            let mut dir = dirs::config_dir().ok_or_else(|| anyhow!("Not found config dir"))?;
            dir.push(env!("CARGO_CRATE_NAME"));
            dir
        };
        Ok(path)
    }

    pub fn local_path(name: &str) -> Result<PathBuf> {
        let mut path = Self::config_dir()?;
        path.push(name);
        Ok(path)
    }

    pub fn save_message(&mut self, input: Input, output: &str) -> Result<()> {
        self.last_message = Some((input.clone(), output.to_string()));

        if self.dry_run {
            return Ok(());
        }

        if let Some(session) = input.session_mut(&mut self.session) {
            session.add_message(&input, output)?;
            return Ok(());
        }

        if !self.save {
            return Ok(());
        }
        let mut file = self.open_message_file()?;
        if output.is_empty() || !self.save {
            return Ok(());
        }
        let timestamp = now();
        let summary = input.summary();
        let input_markdown = input.render();
        let output = match input.role() {
            None => {
                format!("# CHAT: {summary} [{timestamp}]\n{input_markdown}\n--------\n{output}\n--------\n\n",)
            }
            Some(v) => {
                format!(
                    "# CHAT: {summary} [{timestamp}] ({})\n{input_markdown}\n--------\n{output}\n--------\n\n",
                    v.name,
                )
            }
        };
        file.write_all(output.as_bytes())
            .with_context(|| "Failed to save message")
    }

    pub fn maybe_copy(&self, text: &str) {
        if self.auto_copy {
            let _ = set_text(text);
        }
    }

    pub fn config_file() -> Result<PathBuf> {
        Self::local_path(CONFIG_FILE_NAME)
    }

    pub fn roles_file() -> Result<PathBuf> {
        let env_name = get_env_name("roles_file");
        env::var(env_name).map_or_else(
            |_| Self::local_path(ROLES_FILE_NAME),
            |value| Ok(PathBuf::from(value)),
        )
    }

    pub fn messages_file() -> Result<PathBuf> {
        Self::local_path(MESSAGES_FILE_NAME)
    }

    pub fn sessions_dir() -> Result<PathBuf> {
        Self::local_path(SESSIONS_DIR_NAME)
    }

    pub fn session_file(name: &str) -> Result<PathBuf> {
        let mut path = Self::sessions_dir()?;
        path.push(&format!("{name}.yaml"));
        Ok(path)
    }

    pub fn set_role(&mut self, name: &str) -> Result<()> {
        let role = self.retrieve_role(name)?;
        self.set_role_obj(role)
    }

    pub fn set_execute_role(&mut self) -> Result<()> {
        let role = self
            .retrieve_role(Role::EXECUTE)
            .unwrap_or_else(|_| Role::for_execute());
        self.set_role_obj(role)
    }

    pub fn set_describe_command_role(&mut self) -> Result<()> {
        let role = self
            .retrieve_role(Role::DESCRIBE_COMMAND)
            .unwrap_or_else(|_| Role::for_describe_command());
        self.set_role_obj(role)
    }

    pub fn set_code_role(&mut self) -> Result<()> {
        let role = self
            .retrieve_role(Role::CODE)
            .unwrap_or_else(|_| Role::for_code());
        self.set_role_obj(role)
    }

    pub fn set_role_obj(&mut self, role: Role) -> Result<()> {
        if let Some(session) = self.session.as_mut() {
            session.guard_empty()?;
            session.set_temperature(role.temperature);
        }
        self.temperature = role.temperature;
        self.role = Some(role);
        Ok(())
    }

    pub fn clear_role(&mut self) -> Result<()> {
        self.temperature = self.default_temperature;
        self.role = None;
        Ok(())
    }

    pub fn get_state(&self) -> State {
        if let Some(session) = &self.session {
            if session.is_empty() {
                if self.role.is_some() {
                    State::EmptySessionWithRole
                } else {
                    State::EmptySession
                }
            } else {
                State::Session
            }
        } else if self.role.is_some() {
            State::Role
        } else {
            State::Normal
        }
    }

    pub fn get_temperature(&self) -> Option<f64> {
        self.temperature
    }

    pub fn set_temperature(&mut self, value: Option<f64>) {
        self.temperature = value;
        if let Some(session) = self.session.as_mut() {
            session.set_temperature(value);
        }
    }

    pub fn set_compress_threshold(&mut self, value: usize) {
        self.compress_threshold = value;
        if let Some(session) = self.session.as_mut() {
            session.set_compress_threshold(value);
        }
    }

    pub fn echo_messages(&self, input: &Input) -> String {
        if let Some(session) = input.session(&self.session) {
            session.echo_messages(input)
        } else if let Some(role) = input.role() {
            role.echo_messages(input)
        } else {
            input.render()
        }
    }

    pub fn build_messages(&self, input: &Input) -> Result<Vec<Message>> {
        let messages = if let Some(session) = input.session(&self.session) {
            session.build_emssages(input)
        } else if let Some(role) = input.role() {
            role.build_messages(input)
        } else {
            let message = Message::new(input);
            vec![message]
        };
        Ok(messages)
    }

    pub fn set_wrap(&mut self, value: &str) -> Result<()> {
        if value == "no" {
            self.wrap = None;
        } else if value == "auto" {
            self.wrap = Some(value.into());
        } else {
            value
                .parse::<u16>()
                .map_err(|_| anyhow!("Invalid wrap value"))?;
            self.wrap = Some(value.into())
        }
        Ok(())
    }

    pub fn set_model(&mut self, value: &str) -> Result<()> {
        let models = list_models(self);
        let model = Model::find(&models, value);
        match model {
            None => bail!("Invalid model '{}'", value),
            Some(model) => {
                if let Some(session) = self.session.as_mut() {
                    session.set_model(model.clone())?;
                }
                self.model = model;
                Ok(())
            }
        }
    }

    pub fn sys_info(&self) -> Result<String> {
        let display_path = |path: &Path| path.display().to_string();
        let temperature = self
            .temperature
            .map_or_else(|| String::from("-"), |v| v.to_string());
        let wrap = self
            .wrap
            .clone()
            .map_or_else(|| String::from("no"), |v| v.to_string());
        let prelude = if self.prelude.is_empty() {
            String::from("-")
        } else {
            self.prelude.clone()
        };
        let items = vec![
            ("model", self.model.id()),
            ("temperature", temperature),
            ("dry_run", self.dry_run.to_string()),
            ("save", self.save.to_string()),
            ("highlight", self.highlight.to_string()),
            ("light_theme", self.light_theme.to_string()),
            ("wrap", wrap),
            ("wrap_code", self.wrap_code.to_string()),
            ("auto_copy", self.auto_copy.to_string()),
            ("keybindings", self.keybindings.stringify().into()),
            ("prelude", prelude),
            ("compress_threshold", self.compress_threshold.to_string()),
            ("config_file", display_path(&Self::config_file()?)),
            ("roles_file", display_path(&Self::roles_file()?)),
            ("messages_file", display_path(&Self::messages_file()?)),
            ("sessions_dir", display_path(&Self::sessions_dir()?)),
        ];
        let output = items
            .iter()
            .map(|(name, value)| format!("{name:<20}{value}"))
            .collect::<Vec<String>>()
            .join("\n");
        Ok(output)
    }

    pub fn role_info(&self) -> Result<String> {
        if let Some(role) = &self.role {
            role.export()
        } else {
            bail!("No role")
        }
    }

    pub fn session_info(&self) -> Result<String> {
        if let Some(session) = &self.session {
            let render_options = self.get_render_options()?;
            let mut markdown_render = MarkdownRender::init(render_options)?;
            session.info(&mut markdown_render)
        } else {
            bail!("No session")
        }
    }

    pub fn info(&self) -> Result<String> {
        if let Some(session) = &self.session {
            session.export()
        } else if let Some(role) = &self.role {
            role.export()
        } else {
            self.sys_info()
        }
    }

    pub fn last_reply(&self) -> &str {
        self.last_message
            .as_ref()
            .map(|(_, reply)| reply.as_str())
            .unwrap_or_default()
    }

    pub fn repl_complete(&self, cmd: &str, args: &[&str]) -> Vec<String> {
        let (values, filter) = if args.len() == 1 {
            let values = match cmd {
                ".role" => self.roles.iter().map(|v| v.name.clone()).collect(),
                ".model" => list_models(self).into_iter().map(|v| v.id()).collect(),
                ".session" => self.list_sessions(),
                ".set" => vec![
                    "temperature ",
                    "compress_threshold",
                    "save ",
                    "highlight ",
                    "dry_run ",
                    "auto_copy ",
                ]
                .into_iter()
                .map(|v| v.to_string())
                .collect(),
                _ => vec![],
            };
            (values, args[0])
        } else if args.len() == 2 {
            let to_vec = |v: bool| vec![v.to_string()];
            let values = match args[0] {
                "save" => to_vec(!self.save),
                "highlight" => to_vec(!self.highlight),
                "dry_run" => to_vec(!self.dry_run),
                "auto_copy" => to_vec(!self.auto_copy),
                _ => vec![],
            };
            (values, args[1])
        } else {
            return vec![];
        };
        values
            .into_iter()
            .filter(|v| v.starts_with(filter))
            .collect()
    }

    pub fn update(&mut self, data: &str) -> Result<()> {
        let parts: Vec<&str> = data.split_whitespace().collect();
        if parts.len() != 2 {
            bail!("Usage: .set <key> <value>. If value is null, unset key.");
        }
        let key = parts[0];
        let value = parts[1];
        let unset = value == "null";
        match key {
            "temperature" => {
                let value = if unset {
                    None
                } else {
                    let value = value.parse().with_context(|| "Invalid value")?;
                    Some(value)
                };
                self.set_temperature(value);
            }
            "compress_threshold" => {
                let value = value.parse().with_context(|| "Invalid value")?;
                self.set_compress_threshold(value);
            }
            "save" => {
                let value = value.parse().with_context(|| "Invalid value")?;
                self.save = value;
            }
            "highlight" => {
                let value = value.parse().with_context(|| "Invalid value")?;
                self.highlight = value;
            }
            "dry_run" => {
                let value = value.parse().with_context(|| "Invalid value")?;
                self.dry_run = value;
            }
            "auto_copy" => {
                let value = value.parse().with_context(|| "Invalid value")?;
                self.auto_copy = value;
            }
            _ => bail!("Unknown key `{key}`"),
        }
        Ok(())
    }

    pub fn start_session(&mut self, session: Option<&str>) -> Result<()> {
        if self.session.is_some() {
            bail!(
                "Already in a session, please run '.exit session' first to exit the current session."
            );
        }
        match session {
            None => {
                let session_file = Self::session_file(TEMP_SESSION_NAME)?;
                if session_file.exists() {
                    remove_file(session_file)
                        .with_context(|| "Failed to clean previous session")?;
                }
                self.session = Some(Session::new(
                    TEMP_SESSION_NAME,
                    self.model.clone(),
                    self.temperature,
                ));
            }
            Some(name) => {
                let session_path = Self::session_file(name)?;
                if !session_path.exists() {
                    self.session = Some(Session::new(name, self.model.clone(), self.temperature));
                } else {
                    let session = Session::load(name, &session_path)?;
                    let model = session.model().to_string();
                    self.temperature = session.temperature();
                    self.session = Some(session);
                    self.set_model(&model)?;
                }
            }
        }
        if let Some(session) = self.session.as_mut() {
            if session.is_empty() {
                if let Some((input, output)) = &self.last_message {
                    let ans = Confirm::new(
                        "Start a session that incorporates the last question and answer?",
                    )
                    .with_default(false)
                    .prompt()?;
                    if ans {
                        session.add_message(input, output)?;
                    }
                }
            }
        }
        Ok(())
    }

    pub fn end_session(&mut self) -> Result<()> {
        if let Some(mut session) = self.session.take() {
            self.last_message = None;
            self.temperature = self.default_temperature;
            if session.dirty {
                let ans = Confirm::new("Save session?").with_default(false).prompt()?;
                if !ans {
                    return Ok(());
                }
                let mut name = session.name().to_string();
                if session.is_temp() {
                    name = Text::new("Session name:").with_default(&name).prompt()?;
                }
                let session_path = Self::session_file(&name)?;
                let sessions_dir = session_path.parent().ok_or_else(|| {
                    anyhow!("Unable to save session file to {}", session_path.display())
                })?;
                if !sessions_dir.exists() {
                    create_dir_all(sessions_dir).with_context(|| {
                        format!("Failed to create session_dir '{}'", sessions_dir.display())
                    })?;
                }
                session.save(&session_path)?;
            }
        }
        Ok(())
    }

    pub fn has_session(&self) -> bool {
        self.session.is_some()
    }

    pub fn clear_session_messages(&mut self) -> Result<()> {
        if let Some(session) = self.session.as_mut() {
            session.clear_messages();
        }
        Ok(())
    }

    pub fn list_sessions(&self) -> Vec<String> {
        let sessions_dir = match Self::sessions_dir() {
            Ok(dir) => dir,
            Err(_) => return vec![],
        };
        match read_dir(sessions_dir) {
            Ok(rd) => {
                let mut names = vec![];
                for entry in rd.flatten() {
                    let name = entry.file_name();
                    if let Some(name) = name.to_string_lossy().strip_suffix(".yaml") {
                        names.push(name.to_string());
                    }
                }
                names.sort_unstable();
                names
            }
            Err(_) => vec![],
        }
    }

    pub fn should_compress_session(&mut self) -> bool {
        if let Some(session) = self.session.as_mut() {
            if session.need_compress(self.compress_threshold) {
                session.compressing = true;
                return true;
            }
        }
        false
    }

    pub fn compress_session(&mut self, summary: &str) {
        if let Some(session) = self.session.as_mut() {
            session.compress(format!("{}{}", self.summary_prompt, summary));
        }
    }

    pub fn is_compressing_session(&self) -> bool {
        self.session
            .as_ref()
            .map(|v| v.compressing)
            .unwrap_or_default()
    }

    pub fn end_compressing_session(&mut self) {
        if let Some(session) = self.session.as_mut() {
            session.compressing = false;
        }
    }

    pub fn get_render_options(&self) -> Result<RenderOptions> {
        let theme = if self.highlight {
            let theme_mode = if self.light_theme { "light" } else { "dark" };
            let theme_filename = format!("{theme_mode}.tmTheme");
            let theme_path = Self::local_path(&theme_filename)?;
            if theme_path.exists() {
                let theme = ThemeSet::get_theme(&theme_path)
                    .with_context(|| format!("Invalid theme at {}", theme_path.display()))?;
                Some(theme)
            } else {
                let theme = if self.light_theme {
                    bincode::deserialize_from(LIGHT_THEME).expect("Invalid builtin light theme")
                } else {
                    bincode::deserialize_from(DARK_THEME).expect("Invalid builtin dark theme")
                };
                Some(theme)
            }
        } else {
            None
        };
        let wrap = if stdout().is_terminal() {
            self.wrap.clone()
        } else {
            None
        };
        Ok(RenderOptions::new(theme, wrap, self.wrap_code))
    }

    pub fn render_prompt_left(&self) -> String {
        let variables = self.generate_prompt_context();
        render_prompt(&self.left_prompt, &variables)
    }

    pub fn render_prompt_right(&self) -> String {
        let variables = self.generate_prompt_context();
        render_prompt(&self.right_prompt, &variables)
    }

    pub fn prepare_send_data(&self, input: &Input, stream: bool) -> Result<SendData> {
        let messages = self.build_messages(input)?;
        self.model.max_input_tokens_limit(&messages)?;
        Ok(SendData {
            messages,
            temperature: self.get_temperature(),
            stream,
        })
    }

    pub fn input_context(&self) -> InputContext {
        InputContext::new(self.role.clone(), self.has_session())
    }

    pub fn maybe_print_send_tokens(&self, input: &Input) {
        if self.dry_run {
            if let Ok(messages) = self.build_messages(input) {
                let tokens = self.model.total_tokens(&messages);
                println!(">>> This message consumes {tokens} tokens. <<<");
            }
        }
    }

    fn generate_prompt_context(&self) -> HashMap<&str, String> {
        let mut output = HashMap::new();
        output.insert("model", self.model.id());
        output.insert("client_name", self.model.client_name.clone());
        output.insert("model_name", self.model.name.clone());
        output.insert(
            "max_input_tokens",
            self.model.max_input_tokens.unwrap_or_default().to_string(),
        );
        if let Some(temperature) = self.temperature {
            if temperature != 0.0 {
                output.insert("temperature", temperature.to_string());
            }
        }
        if self.dry_run {
            output.insert("dry_run", "true".to_string());
        }
        if self.save {
            output.insert("save", "true".to_string());
        }
        if let Some(wrap) = &self.wrap {
            if wrap != "no" {
                output.insert("wrap", wrap.clone());
            }
        }
        if self.auto_copy {
            output.insert("auto_copy", "true".to_string());
        }
        if let Some(role) = &self.role {
            output.insert("role", role.name.clone());
        }
        if let Some(session) = &self.session {
            output.insert("session", session.name().to_string());
            let (tokens, percent) = session.tokens_and_percent();
            output.insert("consume_tokens", tokens.to_string());
            output.insert("consume_percent", percent.to_string());
            output.insert("user_messages_len", session.user_messages_len().to_string());
        }

        if self.highlight {
            output.insert("color.reset", "\u{1b}[0m".to_string());
            output.insert("color.black", "\u{1b}[30m".to_string());
            output.insert("color.dark_gray", "\u{1b}[90m".to_string());
            output.insert("color.red", "\u{1b}[31m".to_string());
            output.insert("color.light_red", "\u{1b}[91m".to_string());
            output.insert("color.green", "\u{1b}[32m".to_string());
            output.insert("color.light_green", "\u{1b}[92m".to_string());
            output.insert("color.yellow", "\u{1b}[33m".to_string());
            output.insert("color.light_yellow", "\u{1b}[93m".to_string());
            output.insert("color.blue", "\u{1b}[34m".to_string());
            output.insert("color.light_blue", "\u{1b}[94m".to_string());
            output.insert("color.purple", "\u{1b}[35m".to_string());
            output.insert("color.light_purple", "\u{1b}[95m".to_string());
            output.insert("color.magenta", "\u{1b}[35m".to_string());
            output.insert("color.light_magenta", "\u{1b}[95m".to_string());
            output.insert("color.cyan", "\u{1b}[36m".to_string());
            output.insert("color.light_cyan", "\u{1b}[96m".to_string());
            output.insert("color.white", "\u{1b}[37m".to_string());
            output.insert("color.light_gray", "\u{1b}[97m".to_string());
        }

        output
    }

    fn open_message_file(&self) -> Result<File> {
        let path = Self::messages_file()?;
        ensure_parent_exists(&path)?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("Failed to create/append {}", path.display()))
    }

    fn load_config(config_path: &Path) -> Result<Self> {
        let ctx = || format!("Failed to load config at {}", config_path.display());
        let content = read_to_string(config_path).with_context(ctx)?;

        let config: Self = serde_yaml::from_str(&content)
            .map_err(|err| {
                let err_msg = err.to_string();
                if err_msg.starts_with(&format!("{}: ", CLIENTS_FIELD)) {
                    anyhow!("clients: invalid value")
                } else {
                    anyhow!("{err_msg}")
                }
            })
            .with_context(ctx)?;

        Ok(config)
    }

    fn load_roles(&mut self) -> Result<()> {
        let path = Self::roles_file()?;
        if !path.exists() {
            return Ok(());
        }
        let content = read_to_string(&path)
            .with_context(|| format!("Failed to load roles at {}", path.display()))?;
        let roles: Vec<Role> =
            serde_yaml::from_str(&content).with_context(|| "Invalid roles config")?;
        self.roles = roles;
        Ok(())
    }

    fn setup_model(&mut self) -> Result<()> {
        let model = match &self.model_id {
            Some(v) => v.clone(),
            None => {
                let models = list_models(self);
                if models.is_empty() {
                    bail!("No available model");
                }

                models[0].id()
            }
        };
        self.set_model(&model)?;
        Ok(())
    }

    fn setup_highlight(&mut self) {
        if let Ok(value) = env::var("NO_COLOR") {
            let mut no_color = false;
            set_bool(&mut no_color, &value);
            if no_color {
                self.highlight = false;
            }
        }
    }

    fn setup_light_theme(&mut self) -> Result<()> {
        if self.light_theme {
            return Ok(());
        }
        if let Ok(value) = env::var(get_env_name("light_theme")) {
            set_bool(&mut self.light_theme, &value);
            return Ok(());
        } else if let Ok(value) = env::var("COLORFGBG") {
            if let Some(light) = light_theme_from_colorfgbg(&value) {
                self.light_theme = light
            }
        };
        Ok(())
    }

    fn compat_old_config(&mut self, config_path: &PathBuf) -> Result<()> {
        let content = read_to_string(config_path)?;
        let value: serde_json::Value = serde_yaml::from_str(&content)?;
        if value.get(CLIENTS_FIELD).is_some() {
            return Ok(());
        }

        if let Some(model_name) = value.get("model").and_then(|v| v.as_str()) {
            if model_name.starts_with("gpt") {
                self.model_id = Some(format!("{}:{}", OpenAIClient::NAME, model_name));
            }
        }

        if let Some(ClientConfig::OpenAIConfig(client_config)) = self.clients.get_mut(0) {
            if let Some(api_key) = value.get("api_key").and_then(|v| v.as_str()) {
                client_config.api_key = Some(api_key.to_string())
            }

            if let Some(organization_id) = value.get("organization_id").and_then(|v| v.as_str()) {
                client_config.organization_id = Some(organization_id.to_string())
            }

            let mut extra_config = ExtraConfig::default();

            if let Some(proxy) = value.get("proxy").and_then(|v| v.as_str()) {
                extra_config.proxy = Some(proxy.to_string())
            }

            if let Some(connect_timeout) = value.get("connect_timeout").and_then(|v| v.as_i64()) {
                extra_config.connect_timeout = Some(connect_timeout as _)
            }

            client_config.extra = Some(extra_config);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub enum Keybindings {
    #[serde(rename = "emacs")]
    #[default]
    Emacs,
    #[serde(rename = "vi")]
    Vi,
}

impl Keybindings {
    pub fn is_vi(&self) -> bool {
        matches!(self, Keybindings::Vi)
    }
    pub fn stringify(&self) -> &str {
        match self {
            Keybindings::Emacs => "emacs",
            Keybindings::Vi => "vi",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum State {
    Normal,
    Role,
    EmptySession,
    EmptySessionWithRole,
    Session,
}

impl State {
    pub fn all() -> Vec<Self> {
        vec![
            Self::Normal,
            Self::Role,
            Self::EmptySession,
            Self::EmptySessionWithRole,
            Self::Session,
        ]
    }

    pub fn in_session() -> Vec<Self> {
        vec![
            Self::EmptySession,
            Self::EmptySessionWithRole,
            Self::Session,
        ]
    }

    pub fn not_in_session() -> Vec<Self> {
        let excludes: HashSet<_> = Self::in_session().into_iter().collect();
        Self::all()
            .into_iter()
            .filter(|v| !excludes.contains(v))
            .collect()
    }

    pub fn unable_change_role() -> Vec<Self> {
        vec![Self::Session]
    }

    pub fn able_change_role() -> Vec<Self> {
        let excludes: HashSet<_> = Self::unable_change_role().into_iter().collect();
        Self::all()
            .into_iter()
            .filter(|v| !excludes.contains(v))
            .collect()
    }

    pub fn in_role() -> Vec<Self> {
        vec![Self::Role, Self::EmptySessionWithRole]
    }
}

fn create_config_file(config_path: &Path) -> Result<()> {
    let ans = Confirm::new("No config file, create a new one?")
        .with_default(true)
        .prompt()?;
    if !ans {
        exit(0);
    }

    let client = Select::new("Platform:", list_client_types()).prompt()?;

    let mut config = serde_json::json!({});
    config["model"] = client.into();
    config[CLIENTS_FIELD] = create_client_config(client)?;

    let config_data = serde_yaml::to_string(&config).with_context(|| "Failed to create config")?;

    ensure_parent_exists(config_path)?;
    std::fs::write(config_path, config_data).with_context(|| "Failed to write to config file")?;
    #[cfg(unix)]
    {
        use std::os::unix::prelude::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(config_path, perms)?;
    }

    println!("✨ Saved config file to {}\n", config_path.display());

    Ok(())
}

fn ensure_parent_exists(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("Failed to write to {}, No parent path", path.display()))?;
    if !parent.exists() {
        create_dir_all(parent).with_context(|| {
            format!(
                "Failed to write {}, Cannot create parent directory",
                path.display()
            )
        })?;
    }
    Ok(())
}

fn set_bool(target: &mut bool, value: &str) {
    match value {
        "1" | "true" => *target = true,
        "0" | "false" => *target = false,
        _ => {}
    }
}

#[cfg(debug_assertions)]
fn setup_logger() -> Result<()> {
    use simplelog::{LevelFilter, WriteLogger};
    let file = std::fs::File::create(Config::local_path("debug.log")?)?;
    let log_filter = match std::env::var("AICHAT_LOG_FILTER") {
        Ok(v) => v,
        Err(_) => "aichat".into(),
    };
    let config = simplelog::ConfigBuilder::new()
        .add_filter_allow(log_filter)
        .set_thread_level(LevelFilter::Off)
        .set_time_level(LevelFilter::Off)
        .build();
    WriteLogger::init(log::LevelFilter::Debug, config, file)?;
    Ok(())
}

#[cfg(not(debug_assertions))]
fn setup_logger() -> Result<()> {
    Ok(())
}
