mod builder;
pub use builder::VersoBuilder;

use dpi::{PhysicalPosition, PhysicalSize, Position, Size};
use ipc_channel::{
    ipc::{IpcOneShotServer, IpcSender},
    router::ROUTER,
};
use log::error;
use std::{
    collections::HashMap,
    path::Path,
    process::Command,
    sync::{Arc, Mutex, mpsc::Sender as MpscSender},
};
pub use versoview_messages::{
    ConfigFromController as VersoviewSettings, CustomProtocol, CustomProtocolBuilder, Icon,
    ProfilerSettings, Theme, UserScript, WindowLevel,
};
use versoview_messages::{
    PositionType, SizeType, ToControllerMessage, ToVersoMessage, WebResourceRequestResponse,
};

type ResponseFunction = Box<dyn FnOnce(Option<http::Response<Vec<u8>>>) + Send>;
type Listener<T> = Arc<Mutex<Option<T>>>;
type ResponseListener<T> = Arc<Mutex<HashMap<uuid::Uuid, MpscSender<T>>>>;

#[derive(Default)]
struct EventListeners {
    on_close_requested: Listener<Box<dyn Fn() + Send + 'static>>,
    on_navigation_starting: Listener<Box<dyn Fn(url::Url) -> bool + Send + 'static>>,
    on_web_resource_requested:
        Listener<Box<dyn Fn(http::Request<Vec<u8>>, ResponseFunction) + Send + 'static>>,
    title_response: ResponseListener<String>,
    size_response: ResponseListener<PhysicalSize<u32>>,
    position_response: ResponseListener<Option<PhysicalPosition<i32>>>,
    maximized_response: ResponseListener<bool>,
    minimized_response: ResponseListener<bool>,
    fullscreen_response: ResponseListener<bool>,
    visible_response: ResponseListener<bool>,
    scale_factor_response: ResponseListener<f64>,
    theme_response: ResponseListener<Theme>,
    get_url_response: ResponseListener<url::Url>,
}

/// A VersoView controller
///
/// Send an exit signal to this Verso instance when dropped
pub struct VersoviewController {
    sender: IpcSender<ToVersoMessage>,
    event_listeners: EventListeners,
}

impl VersoviewController {
    /// Create a new verso instance with settings and get the controller to it
    fn create(
        verso_path: impl AsRef<Path>,
        initial_url: url::Url,
        mut settings: VersoviewSettings,
    ) -> Self {
        let path = verso_path.as_ref();
        let (server, server_name) = IpcOneShotServer::<ToControllerMessage>::new().unwrap();
        Command::new(path)
            .arg(format!("--ipc-channel={server_name}"))
            .spawn()
            .unwrap();

        let (receiver, message) = server.accept().unwrap();
        let ToControllerMessage::SetToVersoSender(sender) = message else {
            panic!(
                "The initial message sent from versoview is not a `ToControllerMessage::SetToVersoSender`"
            )
        };

        settings.url.replace(initial_url);
        sender
            .send(ToVersoMessage::SetConfig(settings))
            .expect("Failed to send initial settings to versoview");

        let event_listeners = EventListeners::default();
        let on_close_requested = event_listeners.on_close_requested.clone();
        let on_navigation_starting = event_listeners.on_navigation_starting.clone();
        let on_web_resource_requested = event_listeners.on_web_resource_requested.clone();
        let title_response = event_listeners.title_response.clone();
        let size_response = event_listeners.size_response.clone();
        let position_response = event_listeners.position_response.clone();
        let minimized_response = event_listeners.minimized_response.clone();
        let maximized_response = event_listeners.maximized_response.clone();
        let fullscreen_response = event_listeners.fullscreen_response.clone();
        let visible_response = event_listeners.visible_response.clone();
        let scale_factor_response = event_listeners.scale_factor_response.clone();
        let theme_response = event_listeners.theme_response.clone();
        let get_url_response = event_listeners.get_url_response.clone();
        let to_verso_sender = sender.clone();
        ROUTER.add_typed_route(
            receiver,
            Box::new(move |message| match message {
                Ok(message) => match message {
                    ToControllerMessage::OnCloseRequested => {
                        if let Some(ref callback) = *on_close_requested.lock().unwrap() {
                            callback();
                        }
                    }
                    ToControllerMessage::OnNavigationStarting(id, url) => {
                        if let Some(ref callback) = *on_navigation_starting.lock().unwrap() {
                            if let Err(error) = to_verso_sender.send(
                                ToVersoMessage::OnNavigationStartingResponse(id, callback(url)),
                            ) {
                                error!(
                                    "Error while sending back OnNavigationStarting result: {error}"
                                );
                            }
                        }
                    }
                    ToControllerMessage::OnWebResourceRequested(request) => {
                        if let Some(ref callback) = *on_web_resource_requested.lock().unwrap() {
                            let sender_clone = to_verso_sender.clone();
                            let id = request.id;
                            callback(
                                request.request,
                                Box::new(move |response| {
                                    if let Err(error) = sender_clone.send(ToVersoMessage::WebResourceRequestResponse(
                                        WebResourceRequestResponse { id, response },
                                    )) {
                                        error!("Error while sending back OnNavigationStarting result: {error}");
                                    }
                                }),
                            );
                        }
                    }
                    ToControllerMessage::GetTitleResponse(id, title) => {
                        if let Some(sender) = title_response.lock().unwrap().get(&id).take() {
                            sender.send(title).unwrap();
                        }
                    }
                    ToControllerMessage::GetSizeResponse(id, size) => {
                        if let Some(sender) = size_response.lock().unwrap().get(&id).take() {
                            sender.send(size).unwrap();
                        }
                    }
                    ToControllerMessage::GetPositionResponse(id, position) => {
                        if let Some(sender) = position_response.lock().unwrap().get(&id).take() {
                            sender.send(position).unwrap();
                        }
                    }
                    ToControllerMessage::GetMaximizedResponse(id, maximized) => {
                        if let Some(sender) = maximized_response.lock().unwrap().get(&id).take() {
                            sender.send(maximized).unwrap();
                        }
                    }
                    ToControllerMessage::GetMinimizedResponse(id, minimized) => {
                        if let Some(sender) = minimized_response.lock().unwrap().get(&id).take() {
                            sender.send(minimized).unwrap();
                        }
                    }
                    ToControllerMessage::GetFullscreenResponse(id, fullscreen) => {
                        if let Some(sender) = fullscreen_response.lock().unwrap().get(&id).take() {
                            sender.send(fullscreen).unwrap();
                        }
                    }
                    ToControllerMessage::GetVisibleResponse(id, visible) => {
                        if let Some(sender) = visible_response.lock().unwrap().get(&id).take() {
                            sender.send(visible).unwrap();
                        }
                    }
                    ToControllerMessage::GetScaleFactorResponse(id, scale_factor) => {
                        if let Some(sender) = scale_factor_response.lock().unwrap().get(&id).take() {
                            sender.send(scale_factor).unwrap();
                        }
                    }
                    ToControllerMessage::GetThemeResponse(id, theme) => {
                        if let Some(sender) = theme_response.lock().unwrap().get(&id).take() {
                            sender.send(theme).unwrap();
                        }
                    }
                    ToControllerMessage::GetCurrentUrlResponse(id, url) => {
                        if let Some(sender) = get_url_response.lock().unwrap().get(&id).take() {
                            sender.send(url).unwrap();
                        }
                    }
                    ToControllerMessage::SetToVersoSender(..) => {
                        log::error!("`ToControllerMessage::SetToVersoSender` should not be received after the initial setup")
                    }
                },
                Err(e) => error!("Error while receiving VersoMessage: {e}"),
            }),
        );
        Self {
            sender,
            event_listeners,
        }
    }

    /// Create a new verso instance with default settings and get the controller to it
    pub fn new(verso_path: impl AsRef<Path>, initial_url: url::Url) -> Self {
        Self::create(verso_path, initial_url, VersoviewSettings::default())
    }

    /// Exit
    pub fn exit(&self) -> Result<(), Box<ipc_channel::ErrorKind>> {
        self.sender.send(ToVersoMessage::Exit)
    }

    /// Listen on close requested from the OS,
    /// if you decide to use it, verso will not close the window by itself anymore,
    /// so make sure you handle it properly by either do your own logic or call [`Self::exit`] as a fallback
    pub fn on_close_requested(
        &self,
        callback: impl Fn() + Send + 'static,
    ) -> Result<(), Box<ipc_channel::ErrorKind>> {
        let old_listener = self
            .event_listeners
            .on_close_requested
            .lock()
            .unwrap()
            .replace(Box::new(callback));
        if old_listener.is_none() {
            self.sender.send(ToVersoMessage::ListenToOnCloseRequested)?;
        }
        Ok(())
    }

    /// Execute script
    pub fn execute_script(&self, script: String) -> Result<(), Box<ipc_channel::ErrorKind>> {
        self.sender.send(ToVersoMessage::ExecuteScript(script))
    }

    /// Navigate to url
    pub fn navigate(&self, url: url::Url) -> Result<(), Box<ipc_channel::ErrorKind>> {
        self.sender.send(ToVersoMessage::NavigateTo(url))
    }

    /// Reload the current webview
    pub fn reload(&self) -> Result<(), Box<ipc_channel::ErrorKind>> {
        self.sender.send(ToVersoMessage::Reload)
    }

    /// Listen on navigation starting triggered by user click on a link,
    /// return a boolean in the callback to decide whether or not allowing this navigation
    pub fn on_navigation_starting(
        &self,
        callback: impl Fn(url::Url) -> bool + Send + 'static,
    ) -> Result<(), Box<ipc_channel::ErrorKind>> {
        let old_listener = self
            .event_listeners
            .on_navigation_starting
            .lock()
            .unwrap()
            .replace(Box::new(callback));
        if old_listener.is_none() {
            self.sender
                .send(ToVersoMessage::ListenToOnNavigationStarting)?;
        }
        Ok(())
    }

    /// Listen on web resource requests,
    /// return a boolean in the callback to decide whether or not allowing this navigation
    pub fn on_web_resource_requested(
        &self,
        callback: impl Fn(http::Request<Vec<u8>>, ResponseFunction) + Send + 'static,
    ) -> Result<(), Box<ipc_channel::ErrorKind>> {
        let old_listener = self
            .event_listeners
            .on_web_resource_requested
            .lock()
            .unwrap()
            .replace(Box::new(callback));
        if old_listener.is_none() {
            self.sender
                .send(ToVersoMessage::ListenToWebResourceRequests)?;
        }
        Ok(())
    }

    /// Sets the window title
    pub fn set_title<S: Into<String>>(&self, title: S) -> Result<(), Box<ipc_channel::ErrorKind>> {
        self.sender.send(ToVersoMessage::SetTitle(title.into()))?;
        Ok(())
    }

    /// Sets the webview window's size
    pub fn set_size<S: Into<Size>>(&self, size: S) -> Result<(), Box<ipc_channel::ErrorKind>> {
        self.sender.send(ToVersoMessage::SetSize(size.into()))?;
        Ok(())
    }

    /// Sets the webview window's position
    pub fn set_position<P: Into<Position>>(
        &self,
        position: P,
    ) -> Result<(), Box<ipc_channel::ErrorKind>> {
        self.sender
            .send(ToVersoMessage::SetPosition(position.into()))?;
        Ok(())
    }

    /// Maximize or unmaximize the window
    pub fn set_maximized(&self, maximized: bool) -> Result<(), Box<ipc_channel::ErrorKind>> {
        self.sender.send(ToVersoMessage::SetMaximized(maximized))?;
        Ok(())
    }

    /// Minimize or unminimize the window
    pub fn set_minimized(&self, minimized: bool) -> Result<(), Box<ipc_channel::ErrorKind>> {
        self.sender.send(ToVersoMessage::SetMinimized(minimized))?;
        Ok(())
    }

    /// Sets the window to fullscreen or back
    pub fn set_fullscreen(&self, fullscreen: bool) -> Result<(), Box<ipc_channel::ErrorKind>> {
        self.sender
            .send(ToVersoMessage::SetFullscreen(fullscreen))?;
        Ok(())
    }

    /// Show or hide the window
    pub fn set_visible(&self, visible: bool) -> Result<(), Box<ipc_channel::ErrorKind>> {
        self.sender.send(ToVersoMessage::SetVisible(visible))?;
        Ok(())
    }

    /// Change the window level
    pub fn set_window_level(
        &self,
        window_level: WindowLevel,
    ) -> Result<(), Box<ipc_channel::ErrorKind>> {
        self.sender
            .send(ToVersoMessage::SetWindowLevel(window_level))?;
        Ok(())
    }

    /// Sets the theme
    pub fn set_theme(&self, theme: Option<Theme>) -> Result<(), Box<ipc_channel::ErrorKind>> {
        self.sender.send(ToVersoMessage::SetTheme(theme))?;
        Ok(())
    }

    /// Moves the window with the left mouse button until the button is released
    pub fn start_dragging(&self) -> Result<(), Box<ipc_channel::ErrorKind>> {
        self.sender.send(ToVersoMessage::StartDragging)?;
        Ok(())
    }

    /// Bring the window to the front, and capture input focus
    pub fn focus(&self) -> Result<(), Box<ipc_channel::ErrorKind>> {
        self.sender.send(ToVersoMessage::Focus)?;
        Ok(())
    }

    /// Get the title of the window
    pub fn get_title(&self) -> Result<String, Box<ipc_channel::ErrorKind>> {
        self.get_response(&self.event_listeners.title_response, |id| {
            ToVersoMessage::GetTitle(id)
        })
    }

    /// Get the window's size
    fn get_size(
        &self,
        size_type: SizeType,
    ) -> Result<PhysicalSize<u32>, Box<ipc_channel::ErrorKind>> {
        self.get_response(&self.event_listeners.size_response, |id| {
            ToVersoMessage::GetSize(id, size_type)
        })
    }

    /// Returns the physical size of the window's client area.
    ///
    /// The client area is the content of the window, excluding the title bar and borders.
    pub fn get_inner_size(&self) -> Result<PhysicalSize<u32>, Box<ipc_channel::ErrorKind>> {
        self.get_size(SizeType::Inner)
    }

    /// Returns the physical size of the entire window.
    ///
    /// These dimensions include the title bar and borders.
    /// If you don't want that (and you usually don't), use [`Self::get_inner_size`] instead.
    pub fn get_outer_size(&self) -> Result<PhysicalSize<u32>, Box<ipc_channel::ErrorKind>> {
        self.get_size(SizeType::Outer)
    }

    /// Get the window's position,
    /// returns [`None`] on unsupported platforms (currently only Wayland)
    fn get_position(
        &self,
        position_type: PositionType,
    ) -> Result<Option<PhysicalPosition<i32>>, Box<ipc_channel::ErrorKind>> {
        self.get_response(&self.event_listeners.position_response, |id| {
            ToVersoMessage::GetPosition(id, position_type)
        })
    }

    /// Get the window's inner position,
    /// returns [`None`] on unsupported platforms (currently only Wayland)
    pub fn get_inner_position(
        &self,
    ) -> Result<Option<PhysicalPosition<i32>>, Box<ipc_channel::ErrorKind>> {
        self.get_position(PositionType::Inner)
    }

    /// Get the window's outer position,
    /// returns [`None`] on unsupported platforms (currently only Wayland)
    pub fn get_outer_position(
        &self,
    ) -> Result<Option<PhysicalPosition<i32>>, Box<ipc_channel::ErrorKind>> {
        self.get_position(PositionType::Outer)
    }

    /// Get if the window is currently maximized or not
    pub fn is_maximized(&self) -> Result<bool, Box<ipc_channel::ErrorKind>> {
        self.get_response(&self.event_listeners.maximized_response, |id| {
            ToVersoMessage::GetMaximized(id)
        })
    }

    /// Get if the window is currently minimized or not
    pub fn is_minimized(&self) -> Result<bool, Box<ipc_channel::ErrorKind>> {
        self.get_response(&self.event_listeners.minimized_response, |id| {
            ToVersoMessage::GetMinimized(id)
        })
    }

    /// Get if the window is currently fullscreen or not
    pub fn is_fullscreen(&self) -> Result<bool, Box<ipc_channel::ErrorKind>> {
        self.get_response(&self.event_listeners.fullscreen_response, |id| {
            ToVersoMessage::GetFullscreen(id)
        })
    }

    /// Get the visibility of the window
    pub fn is_visible(&self) -> Result<bool, Box<ipc_channel::ErrorKind>> {
        self.get_response(&self.event_listeners.visible_response, |id| {
            ToVersoMessage::GetVisible(id)
        })
    }

    /// Get the scale factor of the window
    pub fn get_scale_factor(&self) -> Result<f64, Box<ipc_channel::ErrorKind>> {
        self.get_response(&self.event_listeners.scale_factor_response, |id| {
            ToVersoMessage::GetScaleFactor(id)
        })
    }

    /// Get the current theme of the window
    pub fn get_theme(&self) -> Result<Theme, Box<ipc_channel::ErrorKind>> {
        self.get_response(&self.event_listeners.theme_response, |id| {
            ToVersoMessage::GetTheme(id)
        })
    }

    /// Get the URL of the webview
    pub fn get_current_url(&self) -> Result<url::Url, Box<ipc_channel::ErrorKind>> {
        self.get_response(&self.event_listeners.get_url_response, |id| {
            ToVersoMessage::GetCurrentUrl(id)
        })
    }

    /// Resigters the listener and sends the get message to Verso,
    /// then waits for the response message
    fn get_response<T>(
        &self,
        listener: &ResponseListener<T>,
        message: impl FnOnce(uuid::Uuid) -> ToVersoMessage,
    ) -> Result<T, Box<ipc_channel::ErrorKind>> {
        let id = uuid::Uuid::new_v4();
        let (sender, receiver) = std::sync::mpsc::channel();
        listener.lock().unwrap().insert(id, sender);
        if let Err(error) = self.sender.send(message(id)) {
            listener.lock().unwrap().remove(&id);
            return Err(error);
        };
        Ok(receiver.recv().unwrap())
    }

    // /// Add init script to run on document started to load
    // pub fn add_init_script(&self, script: String) -> Result<(), Box<ipc_channel::ErrorKind>> {
    //     self.sender.send(ToVersoMessage::AddInitScript(script))
    // }
}

impl Drop for VersoviewController {
    /// Send an exit signal to this Verso instance
    fn drop(&mut self) {
        let _ = self.exit();
    }
}
