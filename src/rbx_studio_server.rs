use crate::error::{Report, Result};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{extract::State, Json};
use color_eyre::eyre::{eyre, Error, OptionExt};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Content, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
    },
    schemars, tool, tool_handler, tool_router, ErrorData, ServerHandler,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::oneshot::Receiver;
use tokio::sync::{mpsc, watch, Mutex};
use tokio::time::Duration;
use uuid::Uuid;

#[cfg(any(target_os = "windows", target_os = "macos"))]
use base64::Engine as _;

pub const STUDIO_PLUGIN_PORT: u16 = 44755;
const LONG_POLL_DURATION: Duration = Duration::from_secs(15);

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct ToolArguments {
    args: ToolArgumentValues,
    id: Option<Uuid>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct RunCommandResponse {
    success: bool,
    response: String,
    id: Uuid,
}

pub struct AppState {
    process_queue: VecDeque<ToolArguments>,
    output_map: HashMap<Uuid, mpsc::UnboundedSender<Result<String>>>,
    waiter: watch::Receiver<()>,
    trigger: watch::Sender<()>,
}
pub type PackedState = Arc<Mutex<AppState>>;

impl AppState {
    pub fn new() -> Self {
        let (trigger, waiter) = watch::channel(());
        Self {
            process_queue: VecDeque::new(),
            output_map: HashMap::new(),
            waiter,
            trigger,
        }
    }
}

impl ToolArguments {
    fn new(args: ToolArgumentValues) -> (Self, Uuid) {
        Self { args, id: None }.with_id()
    }
    fn with_id(self) -> (Self, Uuid) {
        let id = Uuid::new_v4();
        (
            Self {
                args: self.args,
                id: Some(id),
            },
            id,
        )
    }
}
#[derive(Clone)]
pub struct RBXStudioServer {
    state: PackedState,
    tool_router: ToolRouter<Self>,
}

#[tool_handler]
impl ServerHandler for RBXStudioServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::LATEST,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "Roblox_Studio".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                title: Some("Roblox Studio MCP Server".to_string()),
                icons: None,
                website_url: None,
            },
            instructions: Some(
                "You must be aware of current studio mode before using any tools, infer the mode from conversation context or get_studio_mode.
Do NOT use run_code unless the user explicitly asks you to run code or query something in Studio. Never call run_code proactively to move the camera, teleport the player, or perform actions the user did not request. run_code is for read-only inspection of the live datamodel only — to make permanent changes, edit the user's local source code instead.
After calling run_script_in_play_mode, the datamodel status will be reset to stop mode.
Prefer using start_stop_play tool instead of run_script_in_play_mode. Only use run_script_in_play_mode to run one-time unit test code on the server datamodel.
"
                    .to_string(),
            ),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone)]
struct RunCode {
    #[schemars(description = "Code to run")]
    command: String,
}
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone)]
struct InsertModel {
    #[schemars(description = "Query to search for the model")]
    query: String,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone)]
struct GetConsoleOutput {}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone)]
struct GetStudioMode {}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone)]
struct TakeScreenshot {}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone)]
struct StartStopPlay {
    #[schemars(
        description = "Mode to start or stop, must be start_play, stop, or run_server. Don't use run_server unless you are sure no client/player is needed."
    )]
    mode: String,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone)]
struct RunScriptInPlayMode {
    #[schemars(description = "Code to run")]
    code: String,
    #[schemars(description = "Timeout in seconds, defaults to 100 seconds")]
    timeout: Option<u32>,
    #[schemars(description = "Mode to run in, must be start_play or run_server")]
    mode: String,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone)]
enum ToolArgumentValues {
    RunCode(RunCode),
    InsertModel(InsertModel),
    GetConsoleOutput(GetConsoleOutput),
    StartStopPlay(StartStopPlay),
    RunScriptInPlayMode(RunScriptInPlayMode),
    GetStudioMode(GetStudioMode),
}
#[tool_router]
impl RBXStudioServer {
    pub fn new(state: PackedState) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Runs a Luau command in Roblox Studio and returns the printed output. Only use this tool when the user explicitly asks you to run code or query data from the Studio datamodel. Do not call this tool proactively or on your own initiative. This tool is for READ-ONLY inspection of the live datamodel — do not use it to make changes, as any modifications are temporary and lost when play mode restarts. To make permanent changes, edit the user's local source code instead."
    )]
    async fn run_code(
        &self,
        Parameters(args): Parameters<RunCode>,
    ) -> Result<CallToolResult, ErrorData> {
        self.generic_tool_run(ToolArgumentValues::RunCode(args))
            .await
    }

    #[tool(
        description = "Inserts a model from the Roblox marketplace into the workspace. Returns the inserted model name."
    )]
    async fn insert_model(
        &self,
        Parameters(args): Parameters<InsertModel>,
    ) -> Result<CallToolResult, ErrorData> {
        self.generic_tool_run(ToolArgumentValues::InsertModel(args))
            .await
    }

    #[tool(description = "Get the console output from Roblox Studio.")]
    async fn get_console_output(
        &self,
        Parameters(args): Parameters<GetConsoleOutput>,
    ) -> Result<CallToolResult, ErrorData> {
        self.generic_tool_run(ToolArgumentValues::GetConsoleOutput(args))
            .await
    }

    #[tool(
        description = "Start or stop play mode or run the server, Don't enter run_server mode unless you are sure no client/player is needed."
    )]
    async fn start_stop_play(
        &self,
        Parameters(args): Parameters<StartStopPlay>,
    ) -> Result<CallToolResult, ErrorData> {
        self.generic_tool_run(ToolArgumentValues::StartStopPlay(args))
            .await
    }

    #[tool(
        description = "Run a script in play mode and automatically stop play after script finishes or timeout. Returns the output of the script.
        Result format: { success: boolean, value: string, error: string, logs: { level: string, message: string, ts: number }[], errors: { level: string, message: string, ts: number }[], duration: number, isTimeout: boolean }.
        - Prefer using start_stop_play tool instead run_script_in_play_mode, Only used run_script_in_play_mode to run one time unit test code on server datamodel.
        - After calling run_script_in_play_mode, the datamodel status will be reset to stop mode.
        - If It returns `StudioTestService: Previous call to start play session has not been completed`, call start_stop_play tool to stop play mode first then try it again."
    )]
    async fn run_script_in_play_mode(
        &self,
        Parameters(args): Parameters<RunScriptInPlayMode>,
    ) -> Result<CallToolResult, ErrorData> {
        self.generic_tool_run(ToolArgumentValues::RunScriptInPlayMode(args))
            .await
    }

    #[tool(
        description = "Get the current studio mode. Returns the studio mode. The result will be one of start_play, run_server, or stop."
    )]
    async fn get_studio_mode(
        &self,
        Parameters(args): Parameters<GetStudioMode>,
    ) -> Result<CallToolResult, ErrorData> {
        self.generic_tool_run(ToolArgumentValues::GetStudioMode(args))
            .await
    }

    #[tool(
        description = "Take a screenshot of the Roblox Studio window. Returns the screenshot as a PNG image."
    )]
    async fn take_screenshot(
        &self,
        Parameters(_args): Parameters<TakeScreenshot>,
    ) -> Result<CallToolResult, ErrorData> {
        Self::capture_studio_screenshot()
    }

    async fn generic_tool_run(
        &self,
        args: ToolArgumentValues,
    ) -> Result<CallToolResult, ErrorData> {
        let (command, id) = ToolArguments::new(args);
        tracing::debug!("Running command: {:?}", command);
        let (tx, mut rx) = mpsc::unbounded_channel::<Result<String>>();
        let trigger = {
            let mut state = self.state.lock().await;
            state.process_queue.push_back(command);
            state.output_map.insert(id, tx);
            state.trigger.clone()
        };
        trigger
            .send(())
            .map_err(|e| ErrorData::internal_error(format!("Unable to trigger send {e}"), None))?;
        let result = rx
            .recv()
            .await
            .ok_or(ErrorData::internal_error("Couldn't receive response", None))?;
        {
            let mut state = self.state.lock().await;
            state.output_map.remove_entry(&id);
        }
        tracing::debug!("Sending to MCP: {result:?}");
        match result {
            Ok(result) => Ok(CallToolResult::success(vec![Content::text(result)])),
            Err(err) => Ok(CallToolResult::error(vec![Content::text(err.to_string())])),
        }
    }
}

pub async fn request_handler(State(state): State<PackedState>) -> Result<impl IntoResponse> {
    let timeout = tokio::time::timeout(LONG_POLL_DURATION, async {
        let mut waiter = { state.lock().await.waiter.clone() };
        loop {
            {
                let mut state = state.lock().await;
                if let Some(task) = state.process_queue.pop_front() {
                    return Ok::<ToolArguments, Error>(task);
                }
            }
            waiter.changed().await?
        }
    })
    .await;
    match timeout {
        Ok(result) => Ok(Json(result?).into_response()),
        _ => Ok((StatusCode::LOCKED, String::new()).into_response()),
    }
}

pub async fn response_handler(
    State(state): State<PackedState>,
    Json(payload): Json<RunCommandResponse>,
) -> Result<impl IntoResponse> {
    tracing::debug!("Received reply from studio {payload:?}");
    let mut state = state.lock().await;
    let tx = state
        .output_map
        .remove(&payload.id)
        .ok_or_eyre("Unknown ID")?;
    let result: Result<String, Report> = if payload.success {
        Ok(payload.response)
    } else {
        Err(Report::from(eyre!(payload.response)))
    };
    Ok(tx.send(result)?)
}

pub async fn proxy_handler(
    State(state): State<PackedState>,
    Json(command): Json<ToolArguments>,
) -> Result<impl IntoResponse> {
    let id = command.id.ok_or_eyre("Got proxy command with no id")?;
    tracing::debug!("Received request to proxy {command:?}");
    let (tx, mut rx) = mpsc::unbounded_channel();
    {
        let mut state = state.lock().await;
        state.process_queue.push_back(command);
        state.output_map.insert(id, tx);
    }
    let result = rx.recv().await.ok_or_eyre("Couldn't receive response")?;
    {
        let mut state = state.lock().await;
        state.output_map.remove_entry(&id);
    }
    let (success, response) = match result {
        Ok(s) => (true, s),
        Err(e) => (false, e.to_string()),
    };
    tracing::debug!("Sending back to dud: success={success}, response={response:?}");
    Ok(Json(RunCommandResponse {
        success,
        response,
        id,
    }))
}

pub async fn dud_proxy_loop(state: PackedState, exit: Receiver<()>) {
    let client = reqwest::Client::new();

    let mut waiter = { state.lock().await.waiter.clone() };
    while exit.is_empty() {
        let entry = { state.lock().await.process_queue.pop_front() };
        if let Some(entry) = entry {
            let res = client
                .post(format!("http://127.0.0.1:{STUDIO_PLUGIN_PORT}/proxy"))
                .json(&entry)
                .send()
                .await;
            if let Ok(res) = res {
                let tx = {
                    state
                        .lock()
                        .await
                        .output_map
                        .remove(&entry.id.unwrap())
                        .unwrap()
                };
                let res = res
                    .json::<RunCommandResponse>()
                    .await
                    .map(|r| r.response)
                    .map_err(Into::into);
                tx.send(res).unwrap();
            } else {
                tracing::error!("Failed to proxy: {res:?}");
            };
        } else {
            waiter.changed().await.unwrap();
        }
    }
}

// Screenshot implementation
impl RBXStudioServer {
    #[cfg(target_os = "windows")]
    fn capture_studio_screenshot() -> std::result::Result<CallToolResult, ErrorData> {
        use image::ImageEncoder;
        use win_screenshot::prelude::*;

        // Base64 adds ~33% overhead, so PNG must be under ~750KB to stay within 1MB tool result limit.
        const MAX_DIMENSION: u32 = 1280;

        let windows = window_list().map_err(|e| {
            ErrorData::internal_error(format!("Failed to enumerate windows: {e:?}"), None)
        })?;

        let studio_window = windows
            .iter()
            .find(|w| w.window_name.contains("Roblox Studio"))
            .ok_or_else(|| {
                ErrorData::invalid_request(
                    "Roblox Studio window not found. Make sure Roblox Studio is open.",
                    None,
                )
            })?;

        let buf = capture_window(studio_window.hwnd).map_err(|e| {
            ErrorData::internal_error(
                format!("Failed to capture Roblox Studio window: {e}"),
                None,
            )
        })?;

        let img = image::RgbaImage::from_raw(buf.width, buf.height, buf.pixels).ok_or_else(|| {
            ErrorData::internal_error("Failed to create image from capture buffer", None)
        })?;

        // Scale down if either dimension exceeds the limit
        let img = if buf.width > MAX_DIMENSION || buf.height > MAX_DIMENSION {
            let scale = MAX_DIMENSION as f64 / buf.width.max(buf.height) as f64;
            let new_w = (buf.width as f64 * scale) as u32;
            let new_h = (buf.height as f64 * scale) as u32;
            image::imageops::resize(&img, new_w, new_h, image::imageops::FilterType::Triangle)
        } else {
            img
        };

        let (w, h) = img.dimensions();
        let mut png_bytes = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(std::io::Cursor::new(&mut png_bytes));
        encoder
            .write_image(&img, w, h, image::ExtendedColorType::Rgba8)
            .map_err(|e| {
                ErrorData::internal_error(format!("Failed to encode screenshot as PNG: {e}"), None)
            })?;

        let base64_str =
            base64::engine::general_purpose::STANDARD.encode(&png_bytes);

        Ok(CallToolResult::success(vec![Content::image(
            base64_str,
            "image/png",
        )]))
    }

    #[cfg(target_os = "macos")]
    fn capture_studio_screenshot() -> std::result::Result<CallToolResult, ErrorData> {
        use image::ImageEncoder;
        use xcap::Window;

        const MAX_DIMENSION: u32 = 1280;

        let windows = Window::all().map_err(|e| {
            ErrorData::internal_error(format!("Failed to enumerate windows: {e}"), None)
        })?;

        let studio_window = windows
            .iter()
            .find(|w| {
                w.title()
                    .map(|t| t.contains("Roblox Studio"))
                    .unwrap_or(false)
            })
            .ok_or_else(|| {
                ErrorData::invalid_request(
                    "Roblox Studio window not found. Make sure Roblox Studio is open.",
                    None,
                )
            })?;

        if studio_window.is_minimized().unwrap_or(false) {
            return Err(ErrorData::invalid_request(
                "Roblox Studio window is minimized. Please restore the window before taking a screenshot — macOS cannot capture minimized windows.",
                None,
            ));
        }

        let img = studio_window.capture_image().map_err(|e| {
            ErrorData::internal_error(
                format!(
                    "Failed to capture Roblox Studio window: {e}. \
                     On macOS, ensure Screen Recording permission is granted in \
                     System Settings > Privacy & Security > Screen Recording."
                ),
                None,
            )
        })?;

        // Scale down if either dimension exceeds the limit
        let (orig_w, orig_h) = img.dimensions();
        let img = if orig_w > MAX_DIMENSION || orig_h > MAX_DIMENSION {
            let scale = MAX_DIMENSION as f64 / orig_w.max(orig_h) as f64;
            let new_w = (orig_w as f64 * scale) as u32;
            let new_h = (orig_h as f64 * scale) as u32;
            image::imageops::resize(&img, new_w, new_h, image::imageops::FilterType::Triangle)
        } else {
            img
        };

        let (w, h) = img.dimensions();
        let mut png_bytes = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(std::io::Cursor::new(&mut png_bytes));
        encoder
            .write_image(&img, w, h, image::ExtendedColorType::Rgba8)
            .map_err(|e| {
                ErrorData::internal_error(format!("Failed to encode screenshot as PNG: {e}"), None)
            })?;

        let base64_str = base64::engine::general_purpose::STANDARD.encode(&png_bytes);

        Ok(CallToolResult::success(vec![Content::image(
            base64_str,
            "image/png",
        )]))
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    fn capture_studio_screenshot() -> std::result::Result<CallToolResult, ErrorData> {
        Err(ErrorData::invalid_request(
            "Screenshot tool is only available on Windows and macOS",
            None,
        ))
    }
}
